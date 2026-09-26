//! Read-only MCP inventory: which MCP servers are configured in which local
//! agents. Design of record is `docs/adr/0005` — v1 deliberately stops at
//! detection; nothing in this module writes to an agent config file.
//!
//! Supported agents (ADR-0005): OpenCode (`~/.config/opencode/opencode.json`,
//! `mcp` key) and DeepSeek Harness (`~/.dsh/cordis.patch.yml`, plugins named
//! `@deepseek-ai/dsh-mcp-client`).
//!
//! Aggregation is by server name across agents (one entry per name, one
//! occurrence per agent that configures it). Env **values are never read** —
//! only key names leave this module, so secrets cannot reach the GUI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

/// DSH plugin name that registers an MCP client. Other plugins in the same
/// file (hooks, providers, …) are not MCP servers and must be skipped.
pub const DSH_MCP_PLUGIN: &str = "@deepseek-ai/dsh-mcp-client";

// ── DTOs ──

#[derive(Debug, Clone, Serialize)]
pub struct McpInventoryReport {
    pub servers: Vec<McpServerSummary>,
    pub agents: Vec<McpAgentStatus>,
    pub scanned_at_ms: u64,
}

/// Per-agent scan outcome. `error` is set when the config file exists but
/// cannot be read or parsed — the UI shows a "read failed + reason" message
/// for that agent instead of hiding it (ADR-0005 Q20).
#[derive(Debug, Clone, Serialize)]
pub struct McpAgentStatus {
    pub agent_key: String,
    pub display_name: String,
    pub installed: bool,
    pub config_path: String,
    pub config_exists: bool,
    pub server_count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerSummary {
    pub name: String,
    /// Transport of the first occurrence; per-agent transports live in `agents`.
    pub transport: String,
    pub command: Option<String>,
    pub url: Option<String>,
    /// Union of env key names across occurrences — never values.
    pub env_keys: Vec<String>,
    pub agents: Vec<McpServerOccurrence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerOccurrence {
    pub agent_key: String,
    pub agent_display_name: String,
    pub transport: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub env_keys: Vec<String>,
    /// OpenCode stores a native `enabled` flag; DSH has none, so `None` means
    /// "registered" (manager does not own DSH's enabled semantics).
    pub enabled: Option<bool>,
    /// DSH plugin id, when the agent exposes one.
    pub entry_id: Option<String>,
    pub config_path: String,
}

// ── Agent specs ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpConfigFormat {
    OpenCodeJson,
    DshYaml,
}

struct McpAgentSpec {
    key: &'static str,
    display_name: &'static str,
    detect_dir: &'static str,
    config_relative: &'static str,
    format: McpConfigFormat,
}

const MCP_AGENTS: &[McpAgentSpec] = &[
    McpAgentSpec {
        key: "opencode",
        display_name: "OpenCode",
        detect_dir: ".config/opencode",
        config_relative: ".config/opencode/opencode.json",
        format: McpConfigFormat::OpenCodeJson,
    },
    McpAgentSpec {
        key: "deepseek_harness",
        display_name: "DeepSeek Harness",
        detect_dir: ".dsh",
        config_relative: ".dsh/cordis.patch.yml",
        format: McpConfigFormat::DshYaml,
    },
];

// ── Parsed entry (agent-agnostic) ──

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawMcpEntry {
    name: String,
    transport: String,
    command: Option<String>,
    url: Option<String>,
    env_keys: Vec<String>,
    enabled: Option<bool>,
    entry_id: Option<String>,
}

struct AgentScan {
    status: McpAgentStatus,
    entries: Vec<RawMcpEntry>,
}

// ── Path resolution (mirrors ToolAdapter::candidate_paths) ──

fn candidate_paths(relative: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(relative));
    }

    if let Some(suffix) = relative.strip_prefix(".config/") {
        if let Some(config_dir) = dirs::config_dir() {
            let config_path = config_dir.join(suffix);
            if !candidates.contains(&config_path) {
                candidates.push(config_path);
            }
        }
    }

    candidates
}

fn resolve_config_path(spec: &McpAgentSpec) -> PathBuf {
    let candidates = candidate_paths(spec.config_relative);
    candidates
        .iter()
        .find(|path| path.exists())
        .cloned()
        .or_else(|| candidates.into_iter().next())
        .unwrap_or_else(|| PathBuf::from(spec.config_relative))
}

fn agent_installed(spec: &McpAgentSpec) -> bool {
    candidate_paths(spec.detect_dir).iter().any(|path| path.exists())
}

// ── Scan ──

/// Scan every supported agent. Never fails: per-agent problems land in
/// `McpAgentStatus::error` so one broken config cannot hide the others.
pub fn scan_mcp_inventory() -> McpInventoryReport {
    let scans: Vec<AgentScan> = MCP_AGENTS.iter().map(scan_agent).collect();
    build_report(&scans, now_ms())
}

fn scan_agent(spec: &McpAgentSpec) -> AgentScan {
    let config_path = resolve_config_path(spec);
    let config_exists = config_path.exists();
    let mut entries = Vec::new();
    let mut error = None;

    if config_exists {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => {
                let parsed = match spec.format {
                    McpConfigFormat::OpenCodeJson => parse_opencode_config(&content),
                    McpConfigFormat::DshYaml => parse_dsh_config(&content),
                };
                match parsed {
                    Ok(found) => entries = found,
                    Err(message) => error = Some(message),
                }
            }
            Err(err) => error = Some(format!("读取失败：{err}")),
        }
    }

    AgentScan {
        status: McpAgentStatus {
            agent_key: spec.key.to_string(),
            display_name: spec.display_name.to_string(),
            installed: agent_installed(spec),
            config_path: config_path.to_string_lossy().to_string(),
            config_exists,
            server_count: entries.len(),
            error,
        },
        entries,
    }
}

fn build_report(scans: &[AgentScan], scanned_at_ms: u64) -> McpInventoryReport {
    let mut grouped: BTreeMap<String, Vec<McpServerOccurrence>> = BTreeMap::new();

    for scan in scans {
        for entry in &scan.entries {
            grouped
                .entry(entry.name.clone())
                .or_default()
                .push(McpServerOccurrence {
                    agent_key: scan.status.agent_key.clone(),
                    agent_display_name: scan.status.display_name.clone(),
                    transport: entry.transport.clone(),
                    command: entry.command.clone(),
                    url: entry.url.clone(),
                    env_keys: entry.env_keys.clone(),
                    enabled: entry.enabled,
                    entry_id: entry.entry_id.clone(),
                    config_path: scan.status.config_path.clone(),
                });
        }
    }

    let servers = grouped
        .into_iter()
        .map(|(name, agents)| {
            let mut env_keys: Vec<String> = Vec::new();
            for occurrence in &agents {
                for key in &occurrence.env_keys {
                    if !env_keys.contains(key) {
                        env_keys.push(key.clone());
                    }
                }
            }
            env_keys.sort();

            McpServerSummary {
                name,
                transport: agents[0].transport.clone(),
                command: agents[0].command.clone(),
                url: agents[0].url.clone(),
                env_keys,
                agents,
            }
        })
        .collect();

    McpInventoryReport {
        servers,
        agents: scans.iter().map(|scan| scan.status.clone()).collect(),
        scanned_at_ms,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── OpenCode: JSON with a top-level `mcp` map ──

/// Entry shape: `{ "type": "local"|"remote", "command": [..], "environment": {..},
/// "enabled": bool, "url": ".." }`. A file without an `mcp` key is not an
/// error — it simply has no servers.
fn parse_opencode_config(content: &str) -> Result<Vec<RawMcpEntry>, String> {
    let root: JsonValue =
        serde_json::from_str(content).map_err(|err| format!("JSON 解析失败：{err}"))?;

    let mcp = match root.get("mcp") {
        Some(value) => value,
        None => return Ok(Vec::new()),
    };
    let map = mcp
        .as_object()
        .ok_or_else(|| "mcp 字段不是对象".to_string())?;

    let mut entries = Vec::new();

    for (name, entry) in map {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let raw_type = object.get("type").and_then(|value| value.as_str());
        let url = object
            .get("url")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let command = json_command_line(object.get("command")).filter(|line| !line.is_empty());
        let env_keys = object
            .get("environment")
            .and_then(|value| value.as_object())
            .map(sorted_object_keys)
            .unwrap_or_default();
        let enabled = object.get("enabled").and_then(|value| value.as_bool());

        entries.push(RawMcpEntry {
            name: name.clone(),
            transport: normalize_transport(raw_type, url.is_some(), command.is_some()),
            command,
            url,
            env_keys,
            enabled,
            entry_id: None,
        });
    }

    Ok(entries)
}

fn json_command_line(value: Option<&JsonValue>) -> Option<String> {
    match value {
        // OpenCode merges executable and args into one array.
        Some(JsonValue::Array(parts)) => Some(
            parts
                .iter()
                .filter_map(|part| part.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        Some(JsonValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn sorted_object_keys(map: &serde_json::Map<String, JsonValue>) -> Vec<String> {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

// ── DSH: YAML patch file with `- insert:` plugin blocks ──

/// Only plugins named [`DSH_MCP_PLUGIN`] count as MCP servers; the same file
/// also carries hooks and provider plugins. Returns an error when the file is
/// not the expected top-level list, so the UI can report a parse failure.
fn parse_dsh_config(content: &str) -> Result<Vec<RawMcpEntry>, String> {
    let root: YamlValue =
        serde_yaml::from_str(content).map_err(|err| format!("YAML 解析失败：{err}"))?;

    let patches = root
        .as_sequence()
        .ok_or_else(|| "顶层不是插件列表".to_string())?;

    let mut entries = Vec::new();

    for patch in patches {
        let Some(patch_map) = patch.as_mapping() else {
            continue;
        };
        let Some(plugins) = patch_map
            .get(YamlValue::String("insert".to_string()))
            .and_then(|value| value.as_sequence())
        else {
            continue;
        };

        for plugin in plugins {
            let Some(plugin_map) = plugin.as_mapping() else {
                continue;
            };
            let plugin_name = yaml_string(plugin_map.get(YamlValue::String("name".to_string())));
            if plugin_name.as_deref() != Some(DSH_MCP_PLUGIN) {
                continue;
            }

            let entry_id = yaml_string(plugin_map.get(YamlValue::String("id".to_string())));
            let Some(config) = plugin_map
                .get(YamlValue::String("config".to_string()))
                .and_then(|value| value.as_mapping())
            else {
                continue;
            };

            let transport_raw = yaml_string(config.get(YamlValue::String("transport".to_string())));
            let Some(name) = yaml_string(config.get(YamlValue::String("serverName".to_string())))
                .or_else(|| entry_id.clone())
            else {
                continue;
            };

            let args: Vec<String> = config
                .get(YamlValue::String("args".to_string()))
                .and_then(|value| value.as_sequence())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let command =
                merge_command(yaml_string(config.get(YamlValue::String("command".to_string()))), &args);
            let url = yaml_string(config.get(YamlValue::String("url".to_string())));
            let env_keys = config
                .get(YamlValue::String("env".to_string()))
                .and_then(|value| value.as_mapping())
                .map(|map| {
                    let mut keys: Vec<String> = map
                        .keys()
                        .filter_map(|key| key.as_str().map(str::to_string))
                        .collect();
                    keys.sort();
                    keys
                })
                .unwrap_or_default();

            entries.push(RawMcpEntry {
                name,
                transport: normalize_transport(transport_raw.as_deref(), url.is_some(), command.is_some()),
                command,
                url,
                env_keys,
                enabled: None,
                entry_id,
            });
        }
    }

    Ok(entries)
}

fn yaml_string(value: Option<&YamlValue>) -> Option<String> {
    value.and_then(|value| value.as_str()).map(str::to_string)
}

fn merge_command(command: Option<String>, args: &[String]) -> Option<String> {
    match (command, args.is_empty()) {
        (Some(command), true) => Some(command),
        (Some(command), false) => Some(format!("{} {}", command, args.join(" "))),
        (None, false) => Some(args.join(" ")),
        (None, true) => None,
    }
}

// ── Shared helpers ──

/// Shared transport vocabulary — also consumed by `mcp_writers` so the
/// inventory report and a writer's `read_entry` can never disagree.
pub(crate) fn normalize_transport(raw: Option<&str>, has_url: bool, has_command: bool) -> String {
    match raw.map(|value| value.trim().to_ascii_lowercase()) {
        Some(kind) if kind == "local" || kind == "stdio" => "stdio".to_string(),
        Some(kind) if kind == "streamable-http" => "streamable-http".to_string(),
        Some(kind) if kind == "remote" || kind == "http" || kind == "sse" => "http".to_string(),
        Some(kind) if !kind.is_empty() => kind,
        _ if has_url => "http".to_string(),
        _ if has_command => "stdio".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENCODE_FIXTURE: &str = r#"{
      "$schema": "https://opencode.ai/config.json",
      "mcp": {
        "godot": {
          "type": "local",
          "command": ["/Users/me/.local/bin/godot-mcp", "serve", "-project", "/tmp/game"],
          "enabled": true
        },
        "github": {
          "type": "local",
          "command": ["/Users/me/.local/bin/github-mcp-server", "stdio"],
          "environment": { "GITHUB_TOKEN": "secret", "GITHUB_HOST": "github.com" },
          "enabled": false
        },
        "zvec-grep-remote": {
          "type": "remote",
          "url": "http://127.0.0.1:7999/mcp"
        }
      },
      "provider": { "openai": { "apiKey": "nope" } }
    }"#;

    const DSH_FIXTURE: &str = r#"# zvec-grep comments must not break parsing
- insert:
    - id: agentmemory
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: agentmemory
        command: npx
        args: ['-y', '@agentmemory/mcp']
        env:
          AGENTMEMORY_URL: http://localhost:3111

- insert:
    - id: mcp-zvec-grep
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: streamable-http
        serverName: zvec_grep
        url: http://127.0.0.1:7999/mcp
        toolCallTimeoutMs: 60000

- insert:
    - id: agentmemory-hooks
      name: '@deepseek-ai/dsh-hooks-claude-code'
      config:
        configPath: "/Users/me/.dsh/agentmemory.hooks.json"
"#;

    fn occurrence(name: &str, command: &str, env: &[&str]) -> RawMcpEntry {
        RawMcpEntry {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            url: None,
            env_keys: env.iter().map(|key| key.to_string()).collect(),
            enabled: None,
            entry_id: None,
        }
    }

    fn scan_of(key: &str, display: &str, entries: Vec<RawMcpEntry>) -> AgentScan {
        AgentScan {
            status: McpAgentStatus {
                agent_key: key.to_string(),
                display_name: display.to_string(),
                installed: true,
                config_path: format!("/tmp/{key}.json"),
                config_exists: true,
                server_count: entries.len(),
                error: None,
            },
            entries,
        }
    }

    #[test]
    fn opencode_reads_local_and_remote_entries() {
        let entries = parse_opencode_config(OPENCODE_FIXTURE).expect("fixture parses");

        assert_eq!(entries.len(), 3);

        let godot = entries.iter().find(|entry| entry.name == "godot").unwrap();
        assert_eq!(godot.transport, "stdio");
        assert_eq!(
            godot.command.as_deref(),
            Some("/Users/me/.local/bin/godot-mcp serve -project /tmp/game")
        );
        assert_eq!(godot.enabled, Some(true));
        assert!(godot.env_keys.is_empty());

        let github = entries.iter().find(|entry| entry.name == "github").unwrap();
        assert_eq!(github.enabled, Some(false));
        assert_eq!(github.env_keys, vec!["GITHUB_HOST", "GITHUB_TOKEN"]);

        let remote = entries
            .iter()
            .find(|entry| entry.name == "zvec-grep-remote")
            .unwrap();
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("http://127.0.0.1:7999/mcp"));
        assert_eq!(remote.command, None);
    }

    #[test]
    fn opencode_without_mcp_key_has_no_servers() {
        let entries = parse_opencode_config(r#"{ "$schema": "x", "provider": {} }"#)
            .expect("file without mcp key is valid");
        assert!(entries.is_empty());
    }

    #[test]
    fn opencode_broken_json_reports_error() {
        let error = parse_opencode_config("{ not json").expect_err("invalid JSON must fail");
        assert!(error.contains("JSON 解析失败"), "unexpected: {error}");
    }

    #[test]
    fn dsh_reads_only_mcp_client_plugins() {
        let entries = parse_dsh_config(DSH_FIXTURE).expect("fixture parses");

        assert_eq!(entries.len(), 2, "hooks plugin must be filtered out");

        let agentmemory = entries.iter().find(|entry| entry.name == "agentmemory").unwrap();
        assert_eq!(agentmemory.transport, "stdio");
        assert_eq!(agentmemory.command.as_deref(), Some("npx -y @agentmemory/mcp"));
        assert_eq!(agentmemory.env_keys, vec!["AGENTMEMORY_URL"]);
        assert_eq!(agentmemory.entry_id.as_deref(), Some("agentmemory"));
        assert_eq!(agentmemory.enabled, None);

        let zvec = entries.iter().find(|entry| entry.name == "zvec_grep").unwrap();
        assert_eq!(zvec.transport, "streamable-http");
        assert_eq!(zvec.url.as_deref(), Some("http://127.0.0.1:7999/mcp"));
        assert_eq!(zvec.command, None);
        assert_eq!(zvec.entry_id.as_deref(), Some("mcp-zvec-grep"));
    }

    #[test]
    fn dsh_broken_yaml_reports_error() {
        let error = parse_dsh_config("- insert: [ {name: 'x' ").expect_err("invalid YAML must fail");
        assert!(error.contains("YAML 解析失败"), "unexpected: {error}");
    }

    #[test]
    fn dsh_rejects_unexpected_top_level_shape() {
        let error = parse_dsh_config("insert: []").expect_err("mapping top level must fail");
        assert!(error.contains("顶层不是插件列表"), "unexpected: {error}");
    }

    #[test]
    fn report_aggregates_same_name_across_agents() {
        let opencode = scan_of(
            "opencode",
            "OpenCode",
            vec![
                occurrence("agentmemory", "npx -y @agentmemory/mcp", &[]),
                occurrence("godot", "godot-mcp serve", &[]),
            ],
        );
        let dsh = scan_of(
            "deepseek_harness",
            "DeepSeek Harness",
            vec![
                occurrence(
                    "agentmemory",
                    "npx -y @agentmemory/mcp",
                    &["AGENTMEMORY_URL"],
                ),
                occurrence("zvec_grep", "zvec serve", &[]),
            ],
        );

        let report = build_report(&[opencode, dsh], 42);

        assert_eq!(report.scanned_at_ms, 42);
        let names: Vec<&str> = report.servers.iter().map(|server| server.name.as_str()).collect();
        assert_eq!(names, vec!["agentmemory", "godot", "zvec_grep"]);

        let agentmemory = &report.servers[0];
        assert_eq!(agentmemory.agents.len(), 2);
        assert_eq!(agentmemory.env_keys, vec!["AGENTMEMORY_URL"]);
        assert_eq!(agentmemory.agents[0].agent_key, "opencode");
        assert_eq!(agentmemory.agents[1].agent_key, "deepseek_harness");
        assert_eq!(agentmemory.agents[1].env_keys, vec!["AGENTMEMORY_URL"]);

        let godot = &report.servers[1];
        assert_eq!(godot.agents.len(), 1);
        assert_eq!(godot.agents[0].agent_display_name, "OpenCode");

        assert_eq!(report.agents.len(), 2);
        assert_eq!(report.agents[0].server_count, 2);
    }

    #[test]
    fn report_keeps_agent_error_without_servers() {
        let mut broken = scan_of("opencode", "OpenCode", Vec::new());
        broken.status.error = Some("JSON 解析失败：expected value".to_string());
        broken.status.server_count = 0;

        let report = build_report(&[broken], 1);

        assert!(report.servers.is_empty());
        assert_eq!(
            report.agents[0].error.as_deref(),
            Some("JSON 解析失败：expected value")
        );
    }

    #[test]
    fn transport_falls_back_when_agent_omits_type() {
        assert_eq!(normalize_transport(None, true, false), "http");
        assert_eq!(normalize_transport(None, false, true), "stdio");
        assert_eq!(normalize_transport(None, false, false), "unknown");
        assert_eq!(normalize_transport(Some("LOCAL"), false, true), "stdio");
    }
}