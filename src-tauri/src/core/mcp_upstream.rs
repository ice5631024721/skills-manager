//! Upstream sources for the managed MCP library (ADR-0006 clause 1).
//!
//! A definition may have an upstream it was built from: an npm package
//! installed globally, a package npx cached, a PyPI package uvx runs, or a git
//! repository cloned into the central cache (`base_dir()/mcp-sources`). The
//! module answers three questions per definition:
//!
//! * **Where does it come from?** [`infer_source`] guesses from the command
//!   line (best-effort; the user can always override), and [`McpSource`] is
//!   the explicit tag stored in `mcp_servers.source`.
//! * **Is it behind?** [`check_latest`] queries registry HTTP APIs (npm, PyPI)
//!   or compares git oids, and writes the result through
//!   `store.set_mcp_check_state` using the skills update-check vocabulary.
//! * **How would we upgrade it?** [`upgrade_commands`] renders the exact
//!   shell commands, and [`run_commands`] executes them — only ever after the
//!   confirm dialog has shown them verbatim (ADR-0006 clause 1: never
//!   auto-upgrade).
//!
//! Version checks never touch the network from tests: [`check_one`] takes the
//! http/git probes as injected closures, and the clone/update layer has a
//! local `file://` fixture test.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::mcp_probe::ChildGuard;
use super::mcp_store::McpServerRecord;
use super::repo_key::canonical_repo_key;
use super::skill_store::SkillStore;

/// Hard ceiling for one upgrade command (login shells can be slow to start).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The upstream source a definition was built from. Serialized into
/// `mcp_servers.source` with an external-tag `"kind"` so the DB default
/// `{"kind":"none"}` parses straight back into [`McpSource::None`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpSource {
    /// Foreign, hand-written, or simply unknown: no check, no plan.
    None,
    /// npm package installed with `npm i -g` and launched via its absolute
    /// node path (the fnm-safe spelling agents use).
    NpmGlobal { package: String },
    /// Package resolved by `npx` at launch time (and cached under
    /// `~/.npm/_npx`, which is the part that goes stale).
    Npx { package: String },
    /// PyPI package launched through `uvx`.
    PypiUvx { package: String },
    /// Repository cloned into the central cache; `clone_path` points at it.
    Git {
        repo_url: String,
        clone_path: PathBuf,
    },
}

/// Parse the DB `source` JSON column. A malformed value is an error, not a
/// silent downgrade to None — a corrupted tag must not look like "no upstream
/// source" forever.
pub fn parse_source(value: &serde_json::Value) -> Result<McpSource, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("invalid upstream source: {e}"))
}

// ── source inference ────────────────────────────────────────────────────────

/// Lowercase command basename with any `.exe` suffix removed, so bare
/// `npx`, absolute `/…/fnm/…/bin/npx` and Windows `node.exe` all classify the
/// same way. (Task text says `command=="npx"`; matching on the basename is
/// the same rule for the common spelling and survives absolute paths, which
/// this repo's own agent configs use.)
fn command_tool(command: &str) -> String {
    let trimmed = command.trim();
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    last.strip_suffix(".exe").unwrap_or(last).to_lowercase()
}

/// Flags that consume the following argument as their value.
fn value_consuming_flag(arg: &str) -> bool {
    matches!(arg, "-p" | "--package" | "--from" | "--with")
}

/// First positional argument: skip boolean flags (`-y`, `--yes`, …) and the
/// values of [`value_consuming_flag`]s. `--pkg@1.2.3` style version specifiers
/// are trimmed to the bare name (`@scope/name` keeps its leading `@`).
fn first_positional(args: &[String]) -> Option<String> {
    let mut idx = 0;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if value_consuming_flag(arg) {
            idx += 2;
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(strip_version_spec(arg));
    }
    None
}

fn strip_version_spec(arg: &str) -> String {
    match arg.rfind('@') {
        Some(idx) if idx > 0 => arg[..idx].to_string(),
        _ => arg.to_string(),
    }
}

/// Value of the first occurrence of `flag` (`--from some-pkg` → `Some(pkg)`).
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|v| strip_version_spec(v))
}

/// Package id from a path like `…/node_modules/@scope/name/bin.mjs`: the one
/// (or two, for scopes) segments right after the *last* `node_modules`
/// occurrence — nested installs resolve to the innermost package.
fn package_from_node_modules_path(path: &str) -> Option<String> {
    let marker_pos = path
        .rfind("node_modules/")
        .or_else(|| path.rfind("node_modules\\"))?;
    let after_marker = marker_pos + "node_modules".len() + 1;
    let rest = path.get(after_marker..)?;
    let segments: Vec<&str> = rest.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    match (segments.first(), segments.get(1)) {
        (Some(first), Some(second)) if first.starts_with('@') => Some(format!("{first}/{second}")),
        (Some(first), _) if !first.starts_with('@') => Some(first.to_string()),
        _ => None,
    }
}

/// Best-effort upstream inference from the launch command line (Task 6 rules;
/// creation dialogs prefill from it and the user can override).
///
/// Git sources are not inferred from paths here even when the command points
/// inside `base_dir()/mcp-sources/`: the repo url cannot be recovered from the
/// command alone, so the explicit `source` tag (set on add) stays authoritative.
pub fn infer_source(command: Option<&str>, args: &[String]) -> McpSource {
    let Some(command) = command else {
        return McpSource::None;
    };
    match command_tool(command).as_str() {
        "npx" => first_positional(args)
            .map(|package| McpSource::Npx { package })
            .unwrap_or(McpSource::None),
        "uvx" => {
            // `uvx --from <pkg> <entrypoint>`: the package is the --from
            // value, not the entrypoint. Plain `uvx <pkg>` takes the first
            // positional.
            let package = flag_value(args, "--from").or_else(|| first_positional(args));
            package
                .map(|package| McpSource::PypiUvx { package })
                .unwrap_or(McpSource::None)
        }
        "node" | "nodejs" | "bun" => {
            // Only the absolute-install shape is recognized: the entry script
            // lives inside node_modules/<package> (the fnm direct-path pattern
            // from AGENTS.md; a bare `node server.js` stays None).
            let first = args.first().map(String::as_str).unwrap_or("");
            package_from_node_modules_path(first)
                .map(|package| McpSource::NpmGlobal { package })
                .unwrap_or(McpSource::None)
        }
        _ => {
            // The task rule is "command ends with `node`": absolute paths like
            // `~/.fnm/…/bin/node` end with the tool name, so re-check by suffix
            // when the basename matched nothing (e.g. `…/current/bin/node`).
            let lowered = command.trim().to_lowercase();
            let lowered = lowered.strip_suffix(".exe").unwrap_or(lowered.as_str());
            if lowered.ends_with("node") {
                let first = args.first().map(String::as_str).unwrap_or("");
                if let Some(package) = package_from_node_modules_path(first) {
                    return McpSource::NpmGlobal { package };
                }
            }
            McpSource::None
        }
    }
}

// ── version helpers ─────────────────────────────────────────────────────────

/// Numeric segments of a version: optional `v` prefix stripped, any
/// prerelease/build suffix after `-`/`+` dropped, unparseable segments as 0.
fn semver_tuple(version: &str) -> Vec<u64> {
    let trimmed = version.trim().trim_start_matches('v');
    let core = trimmed
        .split(['-', '+'])
        .next()
        .unwrap_or(trimmed);
    core.split('.')
        .map(|segment| segment.parse::<u64>().unwrap_or(0))
        .collect()
}

/// `a < b` over numeric semver cores; unequal lengths compare against zero
/// padding, prerelease/build suffixes are tolerated (stripped, not ranked —
/// the task asks for tolerance, not full semver ordering).
pub fn semver_less(a: &str, b: &str) -> bool {
    let (ta, tb) = (semver_tuple(a), semver_tuple(b));
    let width = ta.len().max(tb.len());
    for i in 0..width {
        let (x, y) = (ta.get(i).copied().unwrap_or(0), tb.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

/// `.version` of an npm registry `/latest` payload.
pub fn npm_latest_version(body: &serde_json::Value) -> Option<String> {
    body.get("version")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// `.info.version` of a PyPI JSON payload.
pub fn pypi_latest_version(body: &serde_json::Value) -> Option<String> {
    body.pointer("/info/version")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

// ── npx cache ───────────────────────────────────────────────────────────────

/// Every `~/.npm/_npx/<hash>/node_modules/<package>` directory for this
/// package. These are the stale nested installs an "upgrade" must delete
/// (AGENTS.md: npx caches pin old versions forever).
pub fn npx_cache_dirs(package: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    npx_cache_dirs_in(&home.join(".npm").join("_npx"), package)
}

/// Directory-injectable core of [`npx_cache_dirs`] so tests can point at a
/// fabricated cache layout. Sorted for a stable upgrade plan display.
pub(crate) fn npx_cache_dirs_in(root: &Path, package: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        // Scoped packages are two segments; PathBuf::join splits on '/'.
        let candidate = entry.path().join("node_modules").join(package);
        if candidate.is_dir() {
            found.push(candidate);
        }
    }
    found.sort();
    found
}

// ── upgrade plans ───────────────────────────────────────────────────────────

/// POSIX single-quote escaping: wrap in `'`, every inner `'` becomes `'\''`.
/// Commands go through `sh -lc` / `cmd /C`, so paths must not be splittable.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The exact commands the upgrade confirm dialog shows, verbatim, before
/// [`run_commands`] ever executes them (ADR-0006 clause 1).
///
/// These mutate the *global* environment (npm -g, npx caches, uv cache, git
/// clones) — which is precisely why they require confirmation.
pub fn upgrade_commands(record: &McpServerRecord) -> Result<Vec<String>, String> {
    let source = parse_source(&record.source)?;
    upgrade_commands_for_source(&source, None)
}

/// The shell commands that upgrade one inferred source to its latest.
/// `npx_root` injects the cache root for tests; `None` reads the real
/// `~/.npm/_npx`.
pub fn upgrade_commands_for_source(
    source: &McpSource,
    npx_root: Option<&Path>,
) -> Result<Vec<String>, String> {
    match source {
        McpSource::None => Err("no upstream source".to_string()),
        McpSource::NpmGlobal { package } => {
            Ok(vec![format!("npm i -g {}", shell_quote(&format!("{package}@latest")))])
        }
        McpSource::Npx { package } => {
            let dirs = match npx_root {
                Some(root) => npx_cache_dirs_in(root, package),
                None => npx_cache_dirs(package),
            };
            if dirs.is_empty() {
                // No cache entry means nothing stale to clear; refusing beats
                // showing an empty confirm dialog with a fake success path.
                return Err("no npx cache entries to clear".to_string());
            }
            Ok(dirs
                .iter()
                .map(|dir| format!("rm -rf {}", shell_quote(&dir.to_string_lossy())))
                .collect())
        }
        McpSource::PypiUvx { package } => {
            Ok(vec![format!("uv cache clean {}", shell_quote(package))])
        }
        McpSource::Git { clone_path, .. } => Ok(vec![format!(
            "git -C {} pull --ff-only",
            shell_quote(&clone_path.to_string_lossy())
        )]),
    }
}

/// Execute an approved plan: one login shell per command (fnm/nvm PATH,
/// ADR-0006 Consequences), stdout+stderr aggregated in order, stopping at the
/// first failure. `ChildGuard` kills the shell on timeout, so a wedged
/// upgrade cannot linger.
pub fn run_commands(cmds: &[String]) -> Result<String, String> {
    run_commands_timeout(cmds, COMMAND_TIMEOUT)
}

fn run_commands_timeout(cmds: &[String], timeout: Duration) -> Result<String, String> {
    let mut aggregated = String::new();
    for cmd in cmds {
        match run_one_command(cmd, timeout) {
            Ok(output) => aggregated.push_str(&output),
            Err(error) => {
                if !aggregated.is_empty() {
                    aggregated.push('\n');
                }
                aggregated.push_str(&error);
                return Err(aggregated);
            }
        }
    }
    Ok(aggregated)
}

fn run_one_command(cmd: &str, timeout: Duration) -> Result<String, String> {
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("sh");
        c.arg("-lc").arg(cmd);
        c
    };

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn `{cmd}`: {e}"))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let mut guard = ChildGuard::new(child);

    // Reader threads keep the pipes draining so a chatty command cannot
    // block on a full pipe buffer while we wait for EOF. Interleaving of the
    // two streams is nondeterministic; each stream arrives whole.
    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<Box<dyn Read + Send>> = vec![Box::new(stdout), Box::new(stderr)];
    for stream in readers {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(stream);
            let _ = reader.read_to_string(&mut buf);
            let _ = tx.send(buf);
        });
    }
    drop(tx);

    let deadline = Instant::now() + timeout;
    let mut output = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(chunk) => output.push_str(&chunk),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "timeout: `{cmd}` exceeded {}s",
                    timeout.as_secs_f64()
                ));
            }
        }
    }

    // Both pipes reached EOF; reap within the remaining budget (a daemon-ish
    // command may close its stdout yet keep running).
    let status = loop {
        let waited = guard
            .get_mut()
            .expect("guard owns the child until taken")
            .try_wait()
            .map_err(|e| format!("wait on `{cmd}` failed: {e}"))?;
        match waited {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timeout: `{cmd}` exceeded {}s",
                        timeout.as_secs_f64()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    if status.success() {
        Ok(output)
    } else {
        Err(format!(
            "`{cmd}` failed (exit {}):\n{}",
            status.code().unwrap_or(-1),
            output.trim_end()
        ))
    }
}

// ── git central-cache clones ────────────────────────────────────────────────

/// Directory name for one repo inside `mcp-sources/`: last two segments of
/// the canonical repo key joined by `-` (non-alphanumerics collapse to `-`),
/// falling back to the raw url for sources `canonical_repo_key` refuses.
pub(crate) fn clone_slug(repo_url: &str) -> String {
    let key = canonical_repo_key(repo_url).unwrap_or_else(|| repo_url.trim().to_string());
    let segments: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
    let tail = &segments[segments.len().saturating_sub(2)..];
    let slug: String = tail
        .join("-")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if slug.is_empty() {
        "mcp-source".to_string()
    } else {
        slug
    }
}

/// Apply the repo's proxy convention to git2 fetch options (mirrors
/// git_fetcher's inline `ProxyOptions::url` blocks; those helpers are private
/// to git_fetcher, so the minimal plumbing is replicated here).
fn apply_proxy(options: &mut git2::FetchOptions, proxy: Option<&str>) {
    if let Some(url) = proxy.filter(|p| !p.is_empty()) {
        let mut proxy_opts = git2::ProxyOptions::new();
        proxy_opts.url(url);
        options.proxy_options(proxy_opts);
    }
}

/// Ensure `base_dir()/mcp-sources/<slug>` holds a current clone of the repo
/// (ADR-0006 clause 1: "clone into the central cache + pull" only). Cloning a
/// git MCP source materializes it here; re-running the check pulls updates.
pub fn ensure_git_clone(
    repo_url: &str,
    branch: Option<&str>,
    proxy: Option<&str>,
) -> Result<PathBuf, String> {
    let root = super::central_repo::base_dir().join("mcp-sources");
    ensure_git_clone_at(&root, repo_url, branch, proxy)
}

/// Inject-path core of [`ensure_git_clone`]: `root` is the mcp-sources dir.
pub(crate) fn ensure_git_clone_at(
    root: &Path,
    repo_url: &str,
    branch: Option<&str>,
    proxy: Option<&str>,
) -> Result<PathBuf, String> {
    let target = root.join(clone_slug(repo_url));
    if target.join(".git").exists() {
        update_existing_clone(&target, branch, proxy)?;
        return Ok(target);
    }
    if target.exists() {
        return Err(format!(
            "{} exists but is not a git clone",
            target.display()
        ));
    }
    fresh_clone(root, &target, repo_url, branch, proxy)?;
    Ok(target)
}

fn fresh_clone(
    root: &Path,
    target: &Path,
    repo_url: &str,
    branch: Option<&str>,
    proxy: Option<&str>,
) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create {}: {e}", root.display()))?;
    let mut callbacks = git2::RemoteCallbacks::new();
    super::git_credentials::install_git2_credentials(&mut callbacks, repo_url);
    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);
    apply_proxy(&mut fetch_opts, proxy);

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_opts);
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        builder.branch(branch);
    }
    builder
        .clone(repo_url, target)
        .map(|_| ())
        .map_err(|e| format!("clone {repo_url} failed: {e}"))
}

/// fetch + hard reset to `origin/<branch>` — but only when the tracked
/// working tree is clean. A user who has been editing the clone gets an Err
/// instead of silently losing work (dirty ⇒ leave it alone, report).
fn update_existing_clone(path: &Path, branch: Option<&str>, proxy: Option<&str>) -> Result<(), String> {
    let repo = git2::Repository::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let branch = match branch.filter(|b| !b.is_empty()) {
        Some(b) => b.to_string(),
        None => current_branch(&repo).ok_or("clone HEAD is detached and no branch was given")?,
    };
    {
        let mut remote = repo
            .find_remote("origin")
            .map_err(|_| "clone has no origin remote".to_string())?;
        let mut callbacks = git2::RemoteCallbacks::new();
        super::git_credentials::install_git2_credentials(
            &mut callbacks,
            remote.url().unwrap_or_default(),
        );
        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);
        apply_proxy(&mut fetch_opts, proxy);
        remote
            .fetch(
                &["+refs/heads/*:refs/remotes/origin/*"],
                Some(&mut fetch_opts),
                None,
            )
            .map_err(|e| format!("fetch origin failed: {e}"))?;
    }

    if working_tree_is_dirty(&repo)? {
        return Err("clone has local changes".to_string());
    }

    let origin_head = repo
        .revparse_single(&format!("origin/{branch}"))
        .map_err(|e| format!("origin/{branch} not found after fetch: {e}"))?;
    // Reset moves HEAD, the index, and the working tree in one step.
    repo.reset(&origin_head, git2::ResetType::Hard, None)
        .map_err(|e| format!("reset --hard origin/{branch} failed: {e}"))
}

fn current_branch(repo: &git2::Repository) -> Option<String> {
    repo.head()
        .ok()
        .and_then(|head| head.shorthand().map(str::to_string))
        .filter(|name| name != "HEAD")
}

/// Tracked modifications or staged-but-uncommitted changes only: untracked
/// leftovers (an npm install inside the clone) do not block the pull —
/// `reset --hard` leaves them on disk anyway.
fn working_tree_is_dirty(repo: &git2::Repository) -> Result<bool, String> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| format!("status check failed: {e}"))?;
    let tracked_flags = git2::Status::INDEX_NEW
        | git2::Status::INDEX_MODIFIED
        | git2::Status::INDEX_DELETED
        | git2::Status::INDEX_RENAMED
        | git2::Status::INDEX_TYPECHANGE
        | git2::Status::WT_MODIFIED
        | git2::Status::WT_DELETED
        | git2::Status::WT_RENAMED
        | git2::Status::WT_TYPECHANGE;
    Ok(statuses
        .iter()
        .any(|entry| entry.status().intersects(tracked_flags)))
}

// ── version checks ──────────────────────────────────────────────────────────

/// Result of checking one definition against its upstream. `latest` carries
/// the remote version string (or the remote git oid); `behind` says whether
/// an upgrade would change anything.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CheckOutcome {
    pub latest: Option<String>,
    pub behind: bool,
    pub error: Option<String>,
}

/// One check, with the network and git probes injected as closures — this is
/// the decision table the tests pin down (no live network in the suite).
pub(crate) fn check_one(
    source: &McpSource,
    local_version: Option<&str>,
    http_json: &dyn Fn(&str) -> Result<serde_json::Value, String>,
    git_oids: &dyn Fn(&Path) -> Result<(String, String), String>,
) -> CheckOutcome {
    let from_registry = |latest: String| CheckOutcome {
        // No local version (uvx resolves fresh at launch, or the install
        // path was not readable): report latest without claiming "behind".
        behind: local_version.is_some_and(|local| semver_less(local, &latest)),
        latest: Some(latest),
        error: None,
    };
    let failed = |error: String| CheckOutcome {
        latest: None,
        behind: false,
        error: Some(error),
    };

    match source {
        McpSource::None => failed("no upstream source".to_string()),
        McpSource::NpmGlobal { package } | McpSource::Npx { package } => {
            // npm's HTTP API names scoped packages with an encoded `/`; the
            // leading '@' stays literal, matching registry docs.
            let url = format!(
                "https://registry.npmjs.org/{}/latest",
                package.replace('/', "%2F")
            );
            match http_json(&url) {
                Err(e) => failed(e),
                Ok(body) => match npm_latest_version(&body) {
                    Some(version) => from_registry(version),
                    None => failed(format!("npm registry response for {package} has no version")),
                },
            }
        }
        McpSource::PypiUvx { package } => {
            let url = format!("https://pypi.org/pypi/{package}/json");
            match http_json(&url) {
                Err(e) => failed(e),
                Ok(body) => match pypi_latest_version(&body) {
                    Some(version) => from_registry(version),
                    None => failed(format!("PyPI response for {package} has no info.version")),
                },
            }
        }
        McpSource::Git { clone_path, .. } => match git_oids(clone_path) {
            // `behind = differ` per Task 6: the remote head oid is the
            // "latest", any difference is worth a pull.
            Ok((local, remote)) => CheckOutcome {
                latest: Some(remote.clone()),
                behind: local != remote,
                error: None,
            },
            Err(e) => failed(e),
        },
    }
}

/// Local installed version, read from `package.json` where the layout makes
/// it discoverable: the global/npx `node_modules/<pkg>` directory for npm
/// sources. None otherwise (uvx, git — and any unreadable layout).
pub(crate) fn local_version(record: &McpServerRecord, source: &McpSource) -> Option<String> {
    match source {
        McpSource::NpmGlobal { package } => npm_global_dir(record, package)
            .as_deref()
            .and_then(package_json_version),
        McpSource::Npx { package } => npx_cache_dirs(package)
            .into_iter()
            .next()
            .as_deref()
            .and_then(package_json_version),
        _ => None,
    }
}

/// Recover the `…/node_modules/<package>` install dir from the launch line
/// (infer_source kept the package, the path lives on in the entry's args).
fn npm_global_dir(record: &McpServerRecord, package: &str) -> Option<PathBuf> {
    let candidates = record
        .command
        .iter()
        .chain(record.args.iter())
        .map(String::as_str);
    for arg in candidates {
        let Some(marker) = arg.rfind("node_modules/").or_else(|| arg.rfind("node_modules\\"))
        else {
            continue;
        };
        if package_from_node_modules_path(arg).as_deref() == Some(package) {
            let node_modules = &arg[..marker + "node_modules".len()];
            return Some(PathBuf::from(node_modules).join(package));
        }
    }
    None
}

fn package_json_version(package_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(package_dir.join("package.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// `(local head oid, remote head oid)` for one clone, over its `origin`.
/// The remote is asked for the clone's current branch, falling back to the
/// remote's default-branch HEAD entry.
fn local_remote_oids(path: &Path, proxy: Option<&str>) -> Result<(String, String), String> {
    let repo = git2::Repository::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let head = repo.head().map_err(|e| format!("read HEAD of {}: {e}", path.display()))?;
    let local = head
        .peel_to_commit()
        .map_err(|e| format!("peel HEAD: {e}"))?
        .id()
        .to_string();
    let branch = current_branch(&repo);

    let mut remote = repo
        .find_remote("origin")
        .map_err(|_| "clone has no origin remote".to_string())?;
    let mut callbacks = git2::RemoteCallbacks::new();
    super::git_credentials::install_git2_credentials(
        &mut callbacks,
        remote.url().unwrap_or_default(),
    );
    let mut proxy_opts = git2::ProxyOptions::new();
    if let Some(url) = proxy.filter(|p| !p.is_empty()) {
        proxy_opts.url(url);
    }
    remote
        .connect_auth(git2::Direction::Fetch, Some(callbacks), Some(proxy_opts))
        .map_err(|e| format!("connect origin: {e}"))?;
    let refs = remote.list().map_err(|e| format!("list origin refs: {e}"))?;

    let wanted = branch
        .map(|b| format!("refs/heads/{b}"))
        .unwrap_or_else(|| "HEAD".to_string());
    let head_ref = refs
        .iter()
        .find(|r| r.name() == wanted)
        .or_else(|| refs.iter().find(|r| r.name() == "HEAD"))
        .ok_or_else(|| format!("remote has no {wanted} or HEAD"))?;
    Ok((local, head_ref.oid().to_string()))
}

/// Check every managed definition against its upstream and persist the
/// results (mirrors the skills update round; Task 8 schedules it, Task 7 also
/// exposes it on demand). Network or parse failures land as `error` status —
/// this function never propagates panics or errors, by design.
pub fn check_latest(store: &SkillStore, proxy: Option<&str>) -> Vec<(String, CheckOutcome)> {
    let servers = match store.get_all_mcp_servers() {
        Ok(servers) => servers,
        Err(e) => {
            log::warn!("mcp check: could not load the library: {e}");
            return Vec::new();
        }
    };
    let client = super::skillssh_api::build_http_client(proxy, 8);
    let http_json = |url: &str| -> Result<serde_json::Value, String> {
        let response = client
            .get(url)
            .send()
            .map_err(|e| format!("{url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("{url}: HTTP {}", response.status().as_u16()));
        }
        response
            .json::<serde_json::Value>()
            .map_err(|e| format!("{url}: {e}"))
    };
    let git_oids = |path: &Path| -> Result<(String, String), String> {
        local_remote_oids(path, proxy)
    };
    check_servers(store, &servers, &http_json, &git_oids)
}

/// The write-through loop, factored out of [`check_latest`] so the dispatch
/// table and state persistence are testable without a network stack.
pub(crate) fn check_servers(
    store: &SkillStore,
    servers: &[McpServerRecord],
    http_json: &dyn Fn(&str) -> Result<serde_json::Value, String>,
    git_oids: &dyn Fn(&Path) -> Result<(String, String), String>,
) -> Vec<(String, CheckOutcome)> {
    servers
        .iter()
        .map(|record| {
            let outcome = match parse_source(&record.source) {
                Err(error) => CheckOutcome {
                    latest: None,
                    behind: false,
                    error: Some(error),
                },
                Ok(source) => check_one(
                    &source,
                    local_version(record, &source).as_deref(),
                    http_json,
                    git_oids,
                ),
            };
            let status = if outcome.error.is_some() {
                "error"
            } else if outcome.behind {
                "update_available"
            } else {
                "up_to_date"
            };
            if let Err(e) = store.set_mcp_check_state(
                &record.id,
                status,
                outcome.latest.as_deref(),
                outcome.error.as_deref(),
            ) {
                log::warn!("mcp check: could not persist state for {}: {e}", record.id);
            }
            (record.id.clone(), outcome)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn record_with(source: serde_json::Value) -> McpServerRecord {
        McpServerRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: "srv".to_string(),
            transport: "stdio".to_string(),
            command: None,
            args: Vec::new(),
            url: None,
            env: BTreeMap::new(),
            source,
            update_status: "unknown".to_string(),
            remote_version: None,
            last_checked_at: None,
            last_check_error: None,
            probe_status: "pending".to_string(),
            probe_message: None,
            probe_checked_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    // ── McpSource serde shape ───────────────────────────────────────────────

    #[test]
    fn source_serializes_with_kind_tag() {
        assert_eq!(
            serde_json::to_value(&McpSource::None).unwrap(),
            json!({ "kind": "none" })
        );
        assert_eq!(
            serde_json::to_value(McpSource::NpmGlobal {
                package: "@agentmemory/mcp".into()
            })
            .unwrap(),
            json!({ "kind": "npm_global", "package": "@agentmemory/mcp" })
        );
        assert_eq!(
            serde_json::to_value(McpSource::Git {
                repo_url: "https://github.com/o/r.git".into(),
                clone_path: PathBuf::from("/tmp/mcp-sources/o-r"),
            })
            .unwrap(),
            json!({
                "kind": "git",
                "repo_url": "https://github.com/o/r.git",
                "clone_path": "/tmp/mcp-sources/o-r",
            })
        );
        // The DB default column value round-trips.
        assert_eq!(parse_source(&json!({"kind":"none"})).unwrap(), McpSource::None);
        assert!(parse_source(&json!("garbage")).is_err());
    }

    // ── inference table ─────────────────────────────────────────────────────

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn infer_npx() {
        assert_eq!(
            infer_source(Some("npx"), &s(&["-y", "@scope/pkg"])),
            McpSource::Npx {
                package: "@scope/pkg".into()
            }
        );
        // Value-consuming flags skip both the flag and its value.
        assert_eq!(
            infer_source(Some("npx"), &s(&["--package", "@scope/tool", "run-tool"])),
            McpSource::Npx {
                package: "run-tool".into()
            }
        );
        // Version specifiers trim to the bare name…
        assert_eq!(
            infer_source(Some("npx"), &s(&["-y", "fig@latest"])),
            McpSource::Npx {
                package: "fig".into()
            }
        );
        // …absolute npx paths classify like the bare command…
        assert_eq!(
            infer_source(Some("/Users/x/.fnm/node-versions/v24/bin/npx"), &s(&["-y", "pkg"])),
            McpSource::Npx {
                package: "pkg".into()
            }
        );
        // …and no positional at all is not a source.
        assert_eq!(infer_source(Some("npx"), &s(&["-y"])), McpSource::None);
    }

    #[test]
    fn infer_npm_global_from_absolute_node_path() {
        assert_eq!(
            infer_source(
                Some("/Users/x/.local/share/fnm/node-versions/v24.15.0/installation/bin/node"),
                &s(&[
                    "/Users/x/.local/share/fnm/node-versions/v24.15.0/installation/lib/node_modules/@agentmemory/mcp/bin.mjs"
                ]),
            ),
            McpSource::NpmGlobal {
                package: "@agentmemory/mcp".into()
            }
        );
        // Unscoped and bun spellings.
        assert_eq!(
            infer_source(Some("node"), &s(&["/opt/app/node_modules/tool/dist/index.js"])),
            McpSource::NpmGlobal {
                package: "tool".into()
            }
        );
        assert_eq!(
            infer_source(Some("bun"), &s(&["./x/node_modules/pkg/main.js"])),
            McpSource::NpmGlobal {
                package: "pkg".into()
            }
        );
        // A bare script launch has no upstream we can name.
        assert_eq!(
            infer_source(Some("node"), &s(&["server.js"])),
            McpSource::None
        );
    }

    #[test]
    fn infer_uvx_and_rejects() {
        assert_eq!(
            infer_source(Some("uvx"), &s(&["mcp-server-fetch"])),
            McpSource::PypiUvx {
                package: "mcp-server-fetch".into()
            }
        );
        assert_eq!(
            infer_source(Some("uvx"), &s(&["--from", "some-pkg", "server-entry"])),
            McpSource::PypiUvx {
                package: "some-pkg".into()
            }
        );
        assert_eq!(infer_source(Some("python3"), &s(&["-m", "x"])), McpSource::None);
        assert_eq!(infer_source(None, &s(&[])), McpSource::None);
    }

    // ── semver ──────────────────────────────────────────────────────────────

    #[test]
    fn semver_less_table() {
        assert!(semver_less("1.9.20", "1.9.29"));
        assert!(!semver_less("1.9.29", "1.9.20"));
        assert!(!semver_less("1.9.29", "1.9.29"));
        assert!(semver_less("1.9.20", "1.10.0")); // numeric, not lexical
        assert!(!semver_less("v1.2.3", "1.2.3")); // v-prefix tolerated
        assert!(!semver_less("1.2", "1.2.0")); // zero padding
        assert!(semver_less("1.2", "1.2.1"));
        assert!(!semver_less("1.2.3-beta.1", "1.2.3")); // prerelease stripped
        assert!(semver_less("1.2.3+build", "1.2.4")); // build suffix stripped
        assert!(semver_less("", "1.0.0"));
        assert!(!semver_less("2.0.0", "1.9.9"));
    }

    // ── registry payload parsing ────────────────────────────────────────────

    #[test]
    fn registry_parsers_read_canned_payloads() {
        assert_eq!(
            npm_latest_version(&json!({"name": "p", "version": "0.9.30"})).as_deref(),
            Some("0.9.30")
        );
        assert_eq!(npm_latest_version(&json!({})), None);
        assert_eq!(npm_latest_version(&json!({"version": ""})), None);
        assert_eq!(
            pypi_latest_version(&json!({"info": {"version": "1.2.3", "name": "mcp"}}))
                .as_deref(),
            Some("1.2.3")
        );
        assert_eq!(pypi_latest_version(&json!({"info": {}})), None);
    }

    // ── npx cache scan ──────────────────────────────────────────────────────

    #[test]
    fn npx_cache_dirs_finds_scoped_and_unscoped_installs() {
        let root = tempdir().unwrap();
        let mk = |hash: &str, pkg: &str| {
            let dir = root.path().join(hash).join("node_modules").join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        let a = mk("aaa111", "@scope/pkg");
        let b = mk("bbb222", "other");
        let c = mk("ccc333", "@scope/pkg");
        std::fs::create_dir_all(root.path().join("ddd444")).unwrap(); // empty hash

        assert_eq!(
            npx_cache_dirs_in(root.path(), "@scope/pkg"),
            vec![a.clone(), c.clone()],
            "sorted by hash dir, scoped package spans two segments"
        );
        assert_eq!(npx_cache_dirs_in(root.path(), "other"), vec![b]);
        assert_eq!(npx_cache_dirs_in(root.path(), "missing"), Vec::<PathBuf>::new());
        // A non-existing root reads as "no caches", not as an error.
        assert_eq!(
            npx_cache_dirs_in(&root.path().join("nope"), "@scope/pkg"),
            Vec::<PathBuf>::new()
        );
    }

    // ── upgrade plans ───────────────────────────────────────────────────────

    #[test]
    fn upgrade_commands_per_source_kind() {
        let record = |source: serde_json::Value| record_with(source);

        assert_eq!(
            upgrade_commands(&record(json!({"kind":"npm_global","package":"@scope/pkg"}))).unwrap(),
            vec!["npm i -g '@scope/pkg@latest'".to_string()]
        );
        assert_eq!(
            upgrade_commands(&record(json!({"kind":"pypi_uvx","package":"mcp-server-fetch"}))).unwrap(),
            vec!["uv cache clean 'mcp-server-fetch'".to_string()]
        );
        assert_eq!(
            upgrade_commands(&record(json!({
                "kind": "git",
                "repo_url": "https://github.com/o/r.git",
                "clone_path": "/tmp/mcp sources/o-r",
            })))
            .unwrap(),
            vec!["git -C '/tmp/mcp sources/o-r' pull --ff-only".to_string()]
        );
        // None source refuses, and so does a corrupt tag (not silently "none").
        assert_eq!(
            upgrade_commands(&record(json!({"kind":"none"}))).unwrap_err(),
            "no upstream source"
        );
        assert!(upgrade_commands(&record(json!("garbage")))
            .unwrap_err()
            .contains("invalid upstream source"));
    }

    #[test]
    fn upgrade_commands_quote_escaping_is_posix_safe() {
        let cmds = upgrade_commands(&record_with(json!({
            "kind": "npm_global",
            "package": "we'ird",
        })))
        .unwrap();
        assert_eq!(cmds, vec!["npm i -g 'we'\\''ird@latest'".to_string()]);
    }

    #[test]
    fn upgrade_commands_npx_without_cache_entries_refuses() {
        // A package nobody has ever npx'd must not produce an empty plan.
        let err = upgrade_commands(&record_with(json!({
            "kind": "npx",
            "package": "skills-manager-test-package-does-not-exist",
        })))
        .unwrap_err();
        assert_eq!(err, "no npx cache entries to clear");
    }

    // ── check_one dispatch table (injected probes) ──────────────────────────

    fn no_git(_: &Path) -> Result<(String, String), String> {
        Err("git probe not expected".to_string())
    }

    #[test]
    fn check_one_npm_arms_hit_registry_url_and_compare_versions() {
        let seen = std::cell::RefCell::new(Vec::new());
        let http = |url: &str| {
            seen.borrow_mut().push(url.to_string());
            Ok(json!({ "version": "0.9.30" }))
        };
        for source in [
            McpSource::NpmGlobal {
                package: "@agentmemory/mcp".into(),
            },
            McpSource::Npx {
                package: "@agentmemory/mcp".into(),
            },
        ] {
            let outcome = check_one(&source, Some("0.9.29"), &http, &no_git);
            assert_eq!(
                seen.borrow().last().map(String::as_str),
                Some("https://registry.npmjs.org/@agentmemory%2Fmcp/latest"),
                "scoped name uses %2F, keeps the literal @ ({source:?})"
            );
            assert_eq!(outcome.latest.as_deref(), Some("0.9.30"));
            assert!(outcome.behind, "0.9.29 < 0.9.30");
            assert!(outcome.error.is_none());
        }
        // Equal and unknown-local cases never claim "behind".
        let outcome = check_one(
            &McpSource::Npx { package: "p".into() },
            Some("0.9.30"),
            &http,
            &no_git,
        );
        assert!(!outcome.behind);
        let outcome = check_one(&McpSource::Npx { package: "p".into() }, None, &http, &no_git);
        assert!(!outcome.behind);
        assert_eq!(outcome.latest.as_deref(), Some("0.9.30"));
    }

    #[test]
    fn check_one_pypi_arm_and_failures() {
        let seen = std::cell::RefCell::new(Vec::new());
        let http = |url: &str| {
            seen.borrow_mut().push(url.to_string());
            Ok(json!({ "info": { "version": "2025.1.1" } }))
        };
        let outcome = check_one(
            &McpSource::PypiUvx { package: "mcp-server-fetch".into() },
            None,
            &http,
            &no_git,
        );
        assert_eq!(
            seen.borrow().last().map(String::as_str),
            Some("https://pypi.org/pypi/mcp-server-fetch/json")
        );
        assert_eq!(outcome.latest.as_deref(), Some("2025.1.1"));

        // Error object in payload: no version → check error, never panic.
        let bad = |_: &str| Ok(json!({ "message": "not found" }));
        let outcome = check_one(
            &McpSource::Npx { package: "p".into() },
            None,
            &bad,
            &no_git,
        );
        assert!(outcome.error.is_some());
        assert!(outcome.latest.is_none());

        // Transport failure surfaces verbatim-ish as an error outcome.
        let down = |_: &str| Err("connection refused".to_string());
        let outcome = check_one(&McpSource::None, None, &down, &no_git);
        assert_eq!(outcome.error.as_deref(), Some("no upstream source"));
    }

    #[test]
    fn check_one_git_arm_behind_is_oid_difference() {
        let same = |_: &Path| Ok(("aaa".to_string(), "aaa".to_string()));
        let moved = |_: &Path| Ok(("aaa".to_string(), "bbb".to_string()));
        let outcome = check_one(
            &McpSource::Git {
                repo_url: "https://github.com/o/r.git".into(),
                clone_path: PathBuf::from("/tmp/x"),
            },
            None,
            &|_: &str| Err("no http expected".to_string()),
            &same,
        );
        assert!(!outcome.behind);
        assert_eq!(outcome.latest.as_deref(), Some("aaa"));
        let outcome = check_one(
            &McpSource::Git {
                repo_url: "https://github.com/o/r.git".into(),
                clone_path: PathBuf::from("/tmp/x"),
            },
            None,
            &|_: &str| Err("no http expected".to_string()),
            &moved,
        );
        assert!(outcome.behind);
        assert_eq!(outcome.latest.as_deref(), Some("bbb"));
    }

    #[test]
    fn local_version_reads_package_json_next_to_the_launch_path() {
        let root = tempdir().unwrap();
        let pkg_dir = root
            .path()
            .join("lib")
            .join("node_modules")
            .join("@scope")
            .join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            json!({ "name": "@scope/pkg", "version": "1.4.7" }).to_string(),
        )
        .unwrap();

        let mut record = record_with(json!({ "kind": "npm_global", "package": "@scope/pkg" }));
        record.command = Some("/opt/fnm/bin/node".to_string());
        record.args = vec![format!("{}/bin.mjs", pkg_dir.display())];
        assert_eq!(
            local_version(&record, &parse_source(&record.source).unwrap()).as_deref(),
            Some("1.4.7")
        );

        // Unreadable layout degrades to None, never an error.
        record.args = vec!["/somewhere/else/bin.mjs".to_string()];
        assert_eq!(
            local_version(&record, &parse_source(&record.source).unwrap()),
            None
        );
    }

    #[test]
    fn check_servers_persists_the_dispatch_results() {
        let tmp = tempdir().unwrap();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();

        let mut behind = record_with(json!({ "kind": "npx", "package": "sm-check-fixture-absent" }));
        behind.id = "s-behind".into();
        behind.name = "behind-srv".into();
        let mut none = record_with(json!({ "kind": "none" }));
        none.id = "s-none".into();
        none.name = "none-srv".into();
        for r in [&behind, &none] {
            store.insert_mcp_server(r).unwrap();
        }

        let http = |_: &str| Ok(json!({ "version": "2.0.0" }));
        let results = check_servers(
            &store,
            &[behind, none],
            &http,
            &|_: &Path| Err("unused".to_string()),
        );
        assert_eq!(results.len(), 2);

        // npx package with no cache entry anywhere: local unknown → latest
        // only, "up_to_date" (no false badge).
        let s = store.get_mcp_server_by_id("s-behind").unwrap().unwrap();
        assert_eq!(s.update_status, "up_to_date");
        assert_eq!(s.remote_version.as_deref(), Some("2.0.0"));
        assert!(s.last_check_error.is_none());
        assert!(s.last_checked_at.is_some());

        // Kind-none records report an error state, mirroring skills checks.
        let s = store.get_mcp_server_by_id("s-none").unwrap().unwrap();
        assert_eq!(s.update_status, "error");
        assert_eq!(s.last_check_error.as_deref(), Some("no upstream source"));
    }

    // ── git clones: slug + local file:// integration ────────────────────────

    #[test]
    fn clone_slug_uses_last_two_key_segments() {
        assert_eq!(
            clone_slug("https://github.com/agentmemory/mcp-server.git"),
            "agentmemory-mcp-server"
        );
        assert_eq!(clone_slug("git@github.com:o/r.git"), "o-r");
        // Refusals fall back to the raw url, sanitized.
        let odd = clone_slug("https://gitlab.example.com/team group/My.Repo");
        assert!(
            odd.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "slug is filesystem-safe: {odd}"
        );
        assert!(!odd.is_empty());
    }

    fn commit_and_push(up: &Upstream, version: &str) {
        let repo = git2::Repository::open(&up.work).unwrap();
        // Write in the working tree — repo.path() would be the .git directory.
        std::fs::write(up.work.join("server.py"), format!("print('{version}')")).unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("mcp-upstream-test", "test@example.invalid").unwrap();
        let parents: Vec<git2::Commit> = repo
            .head()
            .map(|h| vec![h.peel_to_commit().unwrap()])
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "update", &tree, &parent_refs)
            .unwrap();
        let branch = current_branch(&repo).unwrap();
        let mut remote = repo.find_remote("origin").unwrap();
        remote
            .push(
                &[format!("refs/heads/{branch}:refs/heads/{branch}").as_str()],
                None,
            )
            .unwrap();
    }

    struct Upstream {
        /// Held so the tempdir (and the bare repo inside) outlives the test.
        #[allow(dead_code)]
        dir: tempfile::TempDir,
        work: PathBuf,
        url: String,
    }

    fn init_upstream() -> Upstream {
        let dir = tempdir().unwrap();
        let bare = dir.path().join("upstream.git");
        git2::Repository::init_bare(&bare).unwrap();
        let work = dir.path().join("work");
        let repo = git2::Repository::init(&work).unwrap();
        std::fs::write(work.join("server.py"), "print('v1')").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("mcp-upstream-test", "test@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        let branch = current_branch(&repo).unwrap();
        {
            // Scope the Remote: it holds a borrow of repo until dropped.
            let mut remote = repo.remote("origin", &bare.to_string_lossy()).unwrap();
            remote
                .push(
                    &[format!("refs/heads/{branch}:refs/heads/{branch}").as_str()],
                    None,
                )
                .unwrap();
        }
        // repo (and the scoped Remote/Tree borrows) drops with this fn; the
        // later helpers reopen the work clone through fresh handles.
        Upstream {
            url: format!("file://{}", bare.display()),
            work,
            dir,
        }
    }

    #[test]
    fn ensure_git_clone_fresh_then_fast_forwards_on_clean_tree() {
        let up = init_upstream();
        let root = tempdir().unwrap();

        // Fresh clone materializes under the injected mcp-sources root.
        let path = ensure_git_clone_at(root.path(), &up.url, None, None).unwrap();
        assert!(path.join(".git").is_dir());
        assert_eq!(path.parent().unwrap(), root.path());
        assert!(std::fs::read_to_string(path.join("server.py"))
            .unwrap()
            .contains("v1"));

        // Upstream moves; the clean clone fast-forwards on the next call.
        commit_and_push(&up, "v2");
        let again = ensure_git_clone_at(root.path(), &up.url, None, None).unwrap();
        assert_eq!(again, path, "idempotent: same slug directory");
        assert!(std::fs::read_to_string(path.join("server.py"))
            .unwrap()
            .contains("v2"));
    }

    #[test]
    fn ensure_git_clone_refuses_dirty_trees_and_ignores_untracked() {
        let up = init_upstream();
        let root = tempdir().unwrap();
        let path = ensure_git_clone_at(root.path(), &up.url, None, None).unwrap();

        // Local edits: refuse, and leave the work untouched.
        std::fs::write(path.join("server.py"), "print('mine')").unwrap();
        let err = ensure_git_clone_at(root.path(), &up.url, None, None).unwrap_err();
        assert_eq!(err, "clone has local changes");
        assert!(std::fs::read_to_string(path.join("server.py"))
            .unwrap()
            .contains("mine"));

        // Untracked leftovers (an npm install in the clone) must not block.
        std::fs::write(path.join("server.py"), "print('v1')").unwrap(); // restore tracked
        std::fs::write(path.join("scratch.txt"), "untracked").unwrap();
        commit_and_push(&up, "v3");
        ensure_git_clone_at(root.path(), &up.url, None, None).unwrap();
        assert!(path.join("scratch.txt").exists(), "untracked files survive");
        assert!(std::fs::read_to_string(path.join("server.py"))
            .unwrap()
            .contains("v3"));

        // The remote/oid probe used by check_latest agrees with this clone.
        let (local, remote) = local_remote_oids(&path, None).unwrap();
        assert_eq!(local, remote, "clone is current");
    }

    // ── run_commands (unix login shell + timeout) ───────────────────────────

    #[cfg(unix)]
    #[test]
    fn run_commands_aggregates_output() {
        let out = run_commands(&s(&["echo first-part"])).unwrap();
        assert!(out.contains("first-part"), "stdout captured: {out}");

        let out = run_commands(&s(&["echo line-one", "echo line-two"])).unwrap();
        assert!(out.contains("line-one") && out.contains("line-two"));
    }

    #[cfg(unix)]
    #[test]
    fn run_commands_stops_at_first_failure_with_exit_code() {
        let err = run_commands(&s(&["echo before", "exit 7", "echo never"])).unwrap_err();
        assert!(err.contains("before"), "output up to the failure kept: {err}");
        assert!(err.contains("exit 7"), "nonzero code surfaced: {err}");
        assert!(!err.contains("never"), "later commands did not run");
    }

    #[cfg(unix)]
    #[test]
    fn run_commands_kills_and_reports_timeout() {
        let started = Instant::now();
        let err =
            run_commands_timeout(&s(&["sleep 30"]), Duration::from_millis(400)).unwrap_err();
        assert!(err.contains("timeout"), "message explains the kill: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the sleeper was killed, not waited out"
        );
    }
}
