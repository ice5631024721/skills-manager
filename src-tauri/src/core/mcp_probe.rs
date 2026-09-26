//! One-shot liveness probes for managed MCP servers (ADR-0006 clause 2).
//!
//! The shape is deliberately trivial: spawn the stdio entry (or POST to the
//! http endpoint), send exactly one JSON-RPC `initialize` request, read back
//! the matching response, take `serverInfo`, and kill the child. Monitoring a
//! live server stays out of scope — ADR-0006 clause 2 permits one-shot
//! probes only, and a stdio MCP server detached from its agent is meaningless
//! anyway (resident engines belong to launchd, not to the manager).
//!
//! Two invariants every caller relies on:
//!
//! * **No orphans, no zombies.** [`ChildGuard`] kills and reaps the child on
//!   every exit path of [`probe_stdio`] — handshake success, timeout, stdin
//!   write error, or early process death.
//! * **Timeout is honored end-to-end.** `timeout` bounds the whole handshake
//!   (spawn to first matching response), not an individual read.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::mcp_writers::McpEntryDef;

/// What a successful handshake told us about the server. Both fields are
/// `Option`: `serverInfo` is optional in the MCP initialize result itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProbeInfo {
    pub server_name: Option<String>,
    pub server_version: Option<String>,
}

/// JSON-RPC request id for the one-shot initialize. Fixed at 1: exactly one
/// request per probe, and [`parse_response_line`] matches against it.
const REQUEST_ID: u64 = 1;

/// MCP protocol version advertised to servers. Kept verbatim from the plan
/// (Task 5); servers negotiate independently of this string.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// The initialize request, newline-delimited (the stdio transport framing).
/// Pure so the framing itself is unit-testable; both probes derive their body
/// from this single source of truth.
pub fn initialize_request_line() -> String {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": REQUEST_ID,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "skills-manager", "version": "0" },
        }
    });
    // serde_json's Display is compact (no embedded newlines), so the trailing
    // '\n' is the only newline in the line: one message per line, as the
    // newline-delimited JSON-RPC framing requires.
    format!("{request}\n")
}

/// Interpret one stdout line against the initialize request.
///
/// `None` = not our response: noise (log banners on stdout are legal),
/// non-JSON, other ids, notifications. `Some(Ok)` = a result for
/// `expect_id`; `Some(Err)` = a JSON-RPC error object for `expect_id`.
pub fn parse_response_line(line: &str, expect_id: u64) -> Option<Result<ProbeInfo, String>> {
    let trimmed = line.trim();
    // Cheap reject first: every JSON-RPC response is a single-line object.
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if value.get("id").and_then(|id| id.as_u64())? != expect_id {
        return None;
    }
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unspecified JSON-RPC error");
        return Some(Err(match error.get("code").and_then(|c| c.as_i64()) {
            Some(code) => format!("JSON-RPC error {code}: {message}"),
            None => format!("JSON-RPC error: {message}"),
        }));
    }
    let result = value.get("result")?;
    let server_info = result.get("serverInfo");
    Some(Ok(ProbeInfo {
        server_name: server_info
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        server_version: server_info
            .and_then(|s| s.get("version"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }))
}

/// Kills and reaps the wrapped child when dropped, unless ownership was
/// handed out with [`ChildGuard::take`] first. Mirrors the guard style used
/// elsewhere in the repo (commands/skills.rs `StagedPathGuard`).
pub(crate) struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    pub(crate) fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    pub(crate) fn get_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Take ownership out of the guard (caller reaps the child itself).
    /// Today only the guard's own unit test exercises it; it stays as the
    /// hand-out seam for any future consumer that waits on exit status.
    #[allow(dead_code)]
    pub(crate) fn take(&mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            // Wait after kill so the process table entry is reaped, even if
            // the child already exited (kill then just returns an error).
            let _ = child.wait();
        }
    }
}

/// Stdio probe: spawn the entry, write the initialize request, wait for the
/// matching response line, then best-effort send `notifications/initialized`
/// and let the guard kill+reap the child.
pub fn probe_stdio(entry: &McpEntryDef, timeout: Duration) -> Result<ProbeInfo, String> {
    let command = entry
        .command
        .as_deref()
        .filter(|c| !c.is_empty())
        .ok_or_else(|| "stdio entry has no command".to_string())?;

    let mut cmd = Command::new(command);
    cmd.args(&entry.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // Minimal base environment, then the entry's own env merged on top.
    // PATH + HOME (plus the Windows system-location vars) is what lets npm or
    // node resolve the same binaries the agent's own spawn context would,
    // without leaking every other manager variable into arbitrary servers.
    cmd.env_clear();
    const BASE_ENV: &[&str] = if cfg!(windows) {
        &[
            "PATH", "HOME", "USERPROFILE", "SYSTEMROOT", "SYSTEMDRIVE", "COMSPEC", "TEMP", "TMP",
        ]
    } else {
        &["PATH", "HOME"]
    };
    for key in BASE_ENV {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    // A GUI process inherits a minimal PATH, but stdio servers live in the
    // user's shell PATH (fnm shims, ~/.local/bin, cargo bins). Prepend the
    // best-effort user PATH so the probe spawns what the agent would spawn;
    // without it every shell-installed server probes as "not found".
    cmd.env("PATH", probe_path());
    for (key, value) in &entry.env {
        cmd.env(key, value);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("spawn {command} failed: {e}"))?;
    // From this line on, every `?`/return path runs the kill+wait Drop.
    let mut guard = ChildGuard::new(child);

    let request = initialize_request_line();
    let write_result = match guard.get_mut().and_then(|c| c.stdin.as_mut()) {
        Some(stdin) => stdin.write_all(request.as_bytes()).and_then(|_| stdin.flush()),
        None => Err(std::io::Error::other("stdin is not piped")),
    };
    if let Err(e) = write_result {
        // EPIPE here typically means the server died at startup before
        // reading anything; the guard kills+reaps it on the way out.
        return Err(format!("write to {command} stdin failed: {e}"));
    }

    // A reader thread forwards stdout lines so the deadline below is exact;
    // stderr is /dev/null'd, so there is no second pipe that could fill up
    // and wedge the child while we wait.
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = guard
        .get_mut()
        .and_then(|c| c.stdout.take())
        .ok_or_else(|| "stdio probe: child stdout was not piped".to_string())?;
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break; // EOF: child closed stdout
            }
            let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
            line.clear();
            if tx.send(trimmed).is_err() {
                break; // receiver gave up; stop forwarding
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let outcome = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(parsed) = parse_response_line(&line, REQUEST_ID) {
                    break parsed;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "timeout: {command} sent no initialize response within {}s",
                    timeout.as_secs_f64()
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!("{command} closed stdout without answering the initialize request"));
            }
        }
    };

    if outcome.is_ok() {
        // Best-effort lifecycle close-out per the MCP spec; the process is
        // killed right after anyway, so failures here are irrelevant.
        if let Some(stdin) = guard.get_mut().and_then(|c| c.stdin.as_mut()) {
            let notification =
                serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
            let _ = writeln!(stdin, "{notification}");
            let _ = stdin.flush();
        }
    }
    // Guard drop: kill + wait on both the success and the error paths.
    outcome
}

/// HTTP probe: one POST of the same initialize body to the entry's url.
/// `proxy` is honored through the repo's shared proxy-aware client factory.
pub fn probe_http(entry: &McpEntryDef, proxy: Option<&str>, timeout: Duration) -> Result<ProbeInfo, String> {
    let url = entry
        .url
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "http entry has no url".to_string())?;
    let client = super::skillssh_api::build_http_client(proxy, timeout.as_secs().max(1));
    let request: serde_json::Value =
        serde_json::from_str(initialize_request_line().trim())
            .expect("initialize_request_line always produces valid JSON");

    let response = client
        .post(url)
        // Streamable-http servers may answer with plain JSON or with an SSE
        // event stream; ask for both so neither side 406s the probe.
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .json(&request)
        .send()
        .map_err(compact_http_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Per the streamable-http spec the POST response is complete — the
    // server closes an SSE stream after the response event (open-ended
    // streams are the GET channel's job) — so reading the whole body is
    // protocol-correct, and the client timeout bounds a misbehaving server.
    let body = response.text().map_err(compact_http_error)?;
    parse_probe_response_body(&content_type, &body)
        .ok_or_else(|| format!("no initialize result in response from {url}"))?
}

/// Decide a probe answer from a (possibly partial) response body. `None`
/// means "not decidable yet" — the caller keeps reading. SSE bodies hide
/// the JSON-RPC payload in the first `data:` line; comment and keep-alive
/// lines are skipped.
pub(crate) fn parse_probe_response_body(
    content_type: &str,
    body: &str,
) -> Option<Result<ProbeInfo, String>> {
    if content_type.contains("text/event-stream") {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|data| !data.is_empty() && *data != "[DONE]")
            // find_map, not find-then-parse: a non-JSON keep-alive data line
            // must not abort the search for the real response event.
            .find_map(|data| parse_response_line(data, REQUEST_ID))
    } else {
        parse_response_line(body, REQUEST_ID)
    }
}

/// The user's interactive-shell PATH, resolved once per process through a
/// login shell (fnm/nvm/cargo bins live there; a GUI process never sees it).
fn login_shell_path() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            let output = if cfg!(windows) {
                std::process::Command::new("cmd")
                    .args(["/C", "echo %PATH%"])
                    .output()
            } else {
                std::process::Command::new(&shell)
                    .args(["-lc", "printf %s \"$PATH\""])
                    .output()
            };
            output
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .clone()
}

/// Directories a login shell still misses on typical setups: interactive-only
/// rc files (homebrew), tool installs that never touch shell config at all
/// (fnm's node versions are injected per-session by `fnm env`). Probing is a
/// best-effort approximation of "the environment an agent would spawn with",
/// so union the well-known user bins, newest fnm version first.
fn extra_user_paths() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    let fixed = [
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        std::path::PathBuf::from("/opt/homebrew/bin"),
        std::path::PathBuf::from("/opt/homebrew/sbin"),
        home.join(".local/share/fnm/aliases/default/bin"),
    ];
    for dir in fixed {
        if dir.is_dir() {
            out.push(dir.to_string_lossy().to_string());
        }
    }
    let versions = home.join(".local/share/fnm/node-versions");
    if let Ok(entries) = std::fs::read_dir(&versions) {
        let mut bins: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("installation/bin"))
            .filter(|p| p.is_dir())
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        // Newest first by NUMERIC version segments: a lexicographic reverse
        // would rank v8.0.0 above v24.15.0 and put a stale node first.
        let version_key = |p: &str| -> Vec<u64> {
            std::path::Path::new(p)
                .components()
                .rev()
                .nth(2) // …/node-versions/<v>/installation/bin
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .map(|v| {
                    v.trim_start_matches('v')
                        .split('.')
                        .filter_map(|s| s.parse::<u64>().ok())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        bins.sort_by(|a, b| version_key(b).cmp(&version_key(a)));
        out.extend(bins);
    }
    out
}

/// PATH handed to probed stdio servers: login shell first, then the
/// well-known user bins, then whatever the GUI process inherited.
fn probe_path() -> String {
    let mut parts: Vec<std::path::PathBuf> = Vec::new();
    if let Some(shell_path) = login_shell_path() {
        parts.extend(std::env::split_paths(&shell_path));
    }
    parts.extend(extra_user_paths().iter().map(std::path::PathBuf::from));
    if let Ok(base) = std::env::var("PATH") {
        parts.extend(std::env::split_paths(&base));
    }
    // split/join_paths honour the platform separator (':' unix, ';' windows
    // — drive-letter entries survive instead of being shredded on ':').
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<std::path::PathBuf> = parts
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()))
        .collect();
    std::env::join_paths(deduped)
        .map(|joined| joined.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// reqwest errors can be long-winded; collapse them to what a probe badge
/// tooltip can actually show. A timeout must surface as literally "timeout"
/// (Task 5 contract).
fn compact_http_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        format!("connect failed: {e}")
    } else {
        e.to_string()
    }
}

/// Transport dispatch: stdio entries get the handshake probe, everything
/// else (http, streamable-http) gets the POST probe. The stdio path ignores
/// `proxy` — a spawned child talks to whatever it wants.
pub fn probe_entry(
    entry: &McpEntryDef,
    proxy: Option<&str>,
    timeout: Duration,
) -> Result<ProbeInfo, String> {
    if entry.transport == "stdio" {
        probe_stdio(entry, timeout)
    } else {
        probe_http(entry, proxy, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write as IoWrite};
    use std::net::TcpListener;

    fn entry(command: Option<&str>, args: Vec<String>) -> McpEntryDef {
        McpEntryDef {
            name: "probe-target".to_string(),
            transport: "stdio".to_string(),
            command: command.map(str::to_string),
            args,
            url: None,
            env: BTreeMap::new(),
        }
    }

    // ── pure framing ────────────────────────────────────────────────────────

    #[test]
    fn initialize_request_line_is_newline_delimited_json_rpc() {
        let line = initialize_request_line();
        // Exactly one newline, at the end (single framed message).
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        // Compact form: no embedded newline before the terminator.
        let body = line.trim_end();
        assert!(!body.contains('\n'));

        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["protocolVersion"], "2025-03-26");
        assert_eq!(value["params"]["capabilities"], serde_json::json!({}));
        assert_eq!(value["params"]["clientInfo"]["name"], "skills-manager");
    }

    #[test]
    fn parse_response_line_accepts_matching_result() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"fake","version":"0.9.29"}}}"#;
        assert_eq!(
            parse_response_line(line, 1),
            Some(Ok(ProbeInfo {
                server_name: Some("fake".to_string()),
                server_version: Some("0.9.29".to_string()),
            }))
        );
    }

    #[test]
    fn parse_response_line_skips_noise_foreign_ids_and_notifications() {
        // Legal stdout chatter before the first JSON frame.
        assert_eq!(parse_response_line("starting server...", 1), None);
        assert_eq!(parse_response_line("", 1), None);
        assert_eq!(parse_response_line("[1,2]", 1), None);
        // A frame for some other request id is not our response.
        assert_eq!(
            parse_response_line(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#, 1),
            None
        );
        // Notification: no id at all.
        assert_eq!(
            parse_response_line(r#"{"jsonrpc":"2.0","method":"notifications/message"}"#, 1),
            None
        );
        // Malformed JSON that still starts with '{'.
        assert_eq!(parse_response_line(r#"{"broken"#, 1), None);
    }

    #[test]
    fn parse_response_line_reports_json_rpc_errors() {
        let line = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"boom"}}"#;
        match parse_response_line(line, 1) {
            Some(Err(message)) => {
                assert!(message.contains("boom"), "message kept: {message}");
                assert!(message.contains("-32601"), "code kept: {message}");
            }
            other => panic!("expected Some(Err), got {other:?}"),
        }
    }

    #[test]
    fn parse_response_line_tolerates_missing_server_info() {
        let result = parse_response_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, 1)
            .expect("matching result id")
            .expect("no error object");
        assert_eq!(result, ProbeInfo::default());
    }

    // ── stdio probe (unix) ─────────────────────────────────────────────────

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Fixture: an sh process that records its own pid, waits for one stdin
    /// line, then either answers the initialize request or stays silent.
    /// `exec sleep 30` replaces sh with the sleeper, so the recorded pid is
    /// the exact process the guard must have killed by the time the probe
    /// returns — and there is no orphaned grandchild.
    #[cfg(unix)]
    fn sh_fixture(respond: bool, pid_file: &std::path::Path) -> McpEntryDef {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"fake","version":"0"}}}"#;
        let script = if respond {
            format!(
                "echo $$ > '{}'; read _; printf '%s\\n' '{}'; exec sleep 30",
                pid_file.display(),
                json
            )
        } else {
            format!(
                "echo $$ > '{}'; read _; exec sleep 30",
                pid_file.display()
            )
        };
        entry(Some("sh"), vec!["-c".to_string(), script])
    }

    #[cfg(unix)]
    #[test]
    fn probe_stdio_handshakes_and_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");

        let info = probe_stdio(&sh_fixture(true, &pid_file), Duration::from_secs(5))
            .expect("handshake should succeed");
        assert_eq!(info.server_name.as_deref(), Some("fake"));
        assert_eq!(info.server_version.as_deref(), Some("0"));

        // The guard already killed *and reaped* the child on the success
        // path: a zombie would still answer kill -0, so failure proves both.
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(!pid_alive(pid), "child {pid} must be killed and reaped after the probe");
    }

    #[cfg(unix)]
    #[test]
    fn probe_stdio_timeout_reports_and_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");

        let err = probe_stdio(&sh_fixture(false, &pid_file), Duration::from_secs(1))
            .expect_err("silent server must time out");
        assert!(err.contains("timeout"), "error names the failure mode: {err}");

        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(!pid_alive(pid), "child {pid} must be killed on the timeout path");
    }

    #[test]
    fn probe_stdio_requires_a_command() {
        let mut e = entry(None, Vec::new());
        e.command = None;
        let err = probe_stdio(&e, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("command"), "message explains what is missing: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn child_guard_take_disarms_the_guard() {
        // run_commands (mcp_upstream) waits on the taken child itself, so the
        // guard must stop killing/reaping after ownership leaves it.
        let child = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().unwrap();
        let mut guard = ChildGuard::new(child);
        let mut owned = guard.take().expect("first take hands out the child");
        assert!(guard.take().is_none(), "guard is disarmed after take");
        owned.wait().expect("new owner can reap");
    }

    #[cfg(unix)]
    #[test]
    fn probe_stdio_child_that_exits_immediately_is_an_error_not_a_hang() {
        // /bin/true answers nothing; the probe must come back with an Err
        // (write EPIPE or stdout EOF) and the guard must reap the child.
        let e = entry(Some("/bin/true"), Vec::new());
        let err = probe_stdio(&e, Duration::from_secs(2)).expect_err("no response expected");
        assert!(!err.is_empty());
    }

    // ── http probe ──────────────────────────────────────────────────────────

    /// One-shot local responder: accepts a single connection, drains the
    /// request, writes `raw_response`, closes. Keeps the http test hermetic
    /// (no external network). Returns the bound address; the listener lives
    /// and dies inside the responder thread.
    fn serve_once(raw_response: String) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf); // best-effort request drain
                let _ = stream.write_all(raw_response.as_bytes());
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(100));
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            // listener dropped here: no further connections are accepted.
        });
        addr
    }

    #[test]
    fn probe_http_posts_initialize_and_reads_server_info() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"fake-http","version":"9.9.9"}}}"#;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve_once(raw);
        let mut e = entry(None, Vec::new());
        e.transport = "http".to_string();
        e.url = Some(format!("http://{addr}/mcp"));

        let info = probe_http(&e, None, Duration::from_secs(5)).expect("local responder answers");
        assert_eq!(info.server_name.as_deref(), Some("fake-http"));
        assert_eq!(info.server_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn probe_http_surfaces_json_rpc_errors_and_http_status() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nope"}}"#;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let addr = serve_once(raw);
        let mut e = entry(None, Vec::new());
        e.transport = "streamable-http".to_string();
        e.url = Some(format!("http://{addr}/mcp"));
        let err = probe_http(&e, None, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("nope"), "server message kept: {err}");

        // 500 with no JSON: status must be quoted back.
        let raw500 = "HTTP/1.1 500 Oops\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let addr500 = serve_once(raw500);
        e.url = Some(format!("http://{addr500}/mcp"));
        let err = probe_http(&e, None, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("500"), "status kept: {err}");
    }

    #[test]
    fn probe_http_requires_a_url() {
        let mut e = entry(None, Vec::new());
        e.transport = "http".to_string();
        e.url = None;
        let err = probe_http(&e, None, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("url"), "message explains what is missing: {err}");
    }

    // ── dispatch ────────────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn probe_entry_dispatches_on_transport() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        // stdio arm: reuse the live fixture.
        let info = probe_entry(&sh_fixture(true, &pid_file), None, Duration::from_secs(5))
            .expect("stdio dispatch");
        assert_eq!(info.server_name.as_deref(), Some("fake"));

        // http arm: a url-less non-stdio entry must be refused by probe_http.
        let mut e = entry(Some("unused"), Vec::new());
        e.transport = "streamable-http".to_string();
        let err = probe_entry(&e, None, Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("url"), "went down the http arm: {err}");
    }

    /// Streamable-http servers answer with SSE more often than with plain
    /// JSON; the probe used to read only the latter and reported live
    /// servers as dead.
    #[test]
    fn sse_response_bodies_are_decoded_from_the_first_data_line() {
        let sse = ": keep-alive\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"serverInfo\":{\"name\":\"zvec\",\"version\":\"1\"}}}\n\nevent: noise\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n";
        let parsed = parse_probe_response_body("text/event-stream; charset=utf-8", sse)
            .expect("decidable")
            .expect("initialize result");
        assert_eq!(parsed.server_name.as_deref(), Some("zvec"));
        assert_eq!(parsed.server_version.as_deref(), Some("1"));

        // A stream cut mid-payload is not decidable yet — the reader loop
        // must keep consuming chunks instead of answering from half a body.
        assert!(parse_probe_response_body(
            "text/event-stream",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"res"
        )
        .is_none());

        // Plain JSON bodies keep the old path.
        let json = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        assert!(parse_probe_response_body("application/json", json)
            .expect("decidable")
            .is_ok());
        // [DONE] sentinels and empty data lines are skipped, not answered.
        assert!(parse_probe_response_body("text/event-stream", "data: [DONE]\n\n")
            .is_none());
    }
}

