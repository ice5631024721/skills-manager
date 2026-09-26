//! MCP management commands.
//!
//! v1 shipped the read-only inventory (design of record: `docs/adr/0005`).
//! This module adds the v2 definition library (`docs/adr/0006`): managed
//! servers stored in SQLite, deployed to agents by surgical text edits, with
//! drift confirmation before any human edit is overwritten, one-shot liveness
//! probes, and upstream-aware upgrade plans.
//!
//! Layout: every `#[tauri::command]` is a thin `State` wrapper over an
//! `*_internal(&SkillStore, ...)` function (the skills.rs pattern), and those
//! internals reach an agent's config through an injectable `AgentResolver` so
//! unit tests can point at tempdir files without touching $HOME.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tauri::State;

use crate::core::{
    audit_log::AuditDraft,
    error::AppError,
    mcp_inventory::{self, McpAgentStatus},
    mcp_probe,
    mcp_store::{McpBindingRecord, McpServerRecord},
    mcp_upstream::{self, McpSource},
    mcp_writers::{
        self, atomic_write_text, entry_fingerprint, AgentWriteError, McpEntryDef, McpWriter,
    },
    repo_lock::RepoLock,
    skill_store::SkillStore,
};

/// File writes are user-initiated; probe/upstream checks cap at 15 s so a
/// wedged server cannot hang the library page (ADR-0006 §2).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Valid starting point when an OpenCode config does not exist yet: the
/// writer seeds this text and the atomic write then really creates the file.
const OPENCODE_EMPTY_TEMPLATE: &str = "{\n  \"mcp\": {}\n}";

// ── DTOs ──

/// Wire form of [`McpEntryDef`]; identical field set, kept as its own type so
/// the frontend contract never couples to the writer's internal struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpEntryDefDto {
    pub name: String,
    pub transport: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl From<&McpEntryDefDto> for McpEntryDef {
    fn from(dto: &McpEntryDefDto) -> Self {
        McpEntryDef {
            name: dto.name.clone(),
            transport: dto.transport.clone(),
            command: dto.command.clone(),
            args: dto.args.clone(),
            url: dto.url.clone(),
            env: dto.env.clone(),
        }
    }
}

/// One binding row enriched with a LIVE drift verdict for its agent file.
#[derive(Debug, Clone, Serialize)]
pub struct McpBindingDto {
    pub agent_key: String,
    pub fingerprint: String,
    pub drift: bool,
    pub drift_reason: Option<String>,
}

/// A library definition plus its deployment ledger. The record serializes
/// flattened so the frontend sees every column (probe/update state included)
/// next to `bindings`, mirroring `McpServerRecord` one-to-one.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerDto {
    #[serde(flatten)]
    pub record: McpServerRecord,
    pub bindings: Vec<McpBindingDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpLibraryReport {
    pub servers: Vec<McpServerDto>,
    /// Per-agent availability/read status — reused from the inventory scan so
    /// both sections of the page agree on what agents exist.
    pub agents: Vec<McpAgentStatus>,
}

/// What a write refused to do until the user confirms: the file's current
/// text vs the text this operation would install.
#[derive(Debug, Clone, Serialize)]
pub struct PendingDriftDto {
    pub agent_key: String,
    pub token: String,
    pub current_text: String,
    pub planned_text: String,
}

/// Result of a multi-agent operation (edit/delete): which agents took the
/// write, and which are waiting on a drift confirmation.
#[derive(Debug, Clone, Serialize)]
pub struct EditOutcome {
    pub applied: Vec<String>,
    pub pending_drift: Vec<PendingDriftDto>,
}

/// Result of a single (server, agent) write.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WriteOutcome {
    Applied,
    PendingDrift(PendingDriftDto),
}

#[derive(Debug, Clone, Serialize)]
pub struct UpgradePlanDto {
    pub commands: Vec<String>,
    pub latest_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeStateDto {
    pub probe_status: String,
    pub probe_message: Option<String>,
}

/// The two write operations the library performs against an agent file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOp {
    Upsert,
    Remove,
}

/// Seam to the agent config files: production passes `resolve_agent_config`;
/// tests resolve agent keys onto tempdir paths.
pub(crate) type AgentResolver<'a> =
    &'a (dyn Fn(&str) -> Result<(PathBuf, &'static dyn McpWriter), AppError> + Send + Sync);

// ── small helpers ──

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Preserve the `AppError` classification (e.g. the duplicate-name
/// `invalid_input`) that `mcp_store` already put inside the `anyhow` error;
/// only genuinely unclassified failures degrade to `Database`.
fn store_err(e: anyhow::Error) -> AppError {
    match e.downcast::<AppError>() {
        Ok(app_error) => app_error,
        Err(e) => AppError::db(e),
    }
}

fn writer_err(err: AgentWriteError) -> AppError {
    match err {
        // A format the writer refuses to touch (comments, ambiguous blocks)
        // is an input-level refusal, never a silent whole-file rewrite
        // (ADR-0006 §4).
        AgentWriteError::Unsafe(message) => AppError::invalid_input(message),
        AgentWriteError::Io(message) => AppError::io(message),
    }
}

fn entry_from_record(record: &McpServerRecord) -> McpEntryDef {
    McpEntryDef {
        name: record.name.clone(),
        transport: record.transport.clone(),
        command: record.command.clone(),
        args: record.args.clone(),
        url: record.url.clone(),
        env: record.env.clone(),
    }
}

/// Name validation for add/edit: trimmed, non-empty, and free of the two
/// path separators that would break out of a config-key context.
fn validate_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AppError::invalid_input("MCP server name must not be empty"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(AppError::invalid_input(format!(
            "MCP server name \"{name}\" must not contain path separators"
        )));
    }
    Ok(name.to_string())
}

/// Reject control characters in any scalar that lands in an agent config file.
/// YAML single-quoted scalars pass raw newlines through: an env value
/// containing "\n- insert:" would put a block opener at column 0 of
/// cordis.patch.yml, where the writer's block scanner splits ranges by
/// column-0 markers — surgery would target the wrong range while the
/// re-parse check still passes (serde_yaml reads it as a folded scalar).
/// Tabs are legal inside scalars and never start a line, so they stay.
fn validate_entry_scalars(entry: &McpEntryDef) -> Result<(), AppError> {
    fn has_control(s: &str) -> bool {
        s.chars().any(|c| c.is_control() && c != '\t')
    }
    let mut fields = Vec::new();
    if has_control(&entry.name) {
        fields.push("name");
    }
    if entry.command.as_deref().is_some_and(has_control) {
        fields.push("command");
    }
    if entry.args.iter().any(|a| has_control(a)) {
        fields.push("args");
    }
    if entry.url.as_deref().is_some_and(has_control) {
        fields.push("url");
    }
    if entry
        .env
        .iter()
        .any(|(k, v)| has_control(k) || has_control(v))
    {
        fields.push("env");
    }
    if fields.is_empty() {
        return Ok(());
    }
    Err(AppError::invalid_input(format!(
        "MCP entry fields ({}) must not contain newlines or control characters",
        fields.join(", ")
    )))
}

/// Validate the incoming source JSON against [`McpSource`], and materialize a
/// git source into the central cache (ADR-0006 §1) so the stored record
/// carries a real `clone_path`. A failed clone aborts the caller's operation.
/// Always returns the canonical re-serialization, so equality checks against
/// the stored column don't hinge on frontend key ordering.
fn prepare_source(source: JsonValue, proxy: Option<&str>) -> Result<JsonValue, AppError> {
    let parsed = if source.is_null() {
        McpSource::None
    } else {
        mcp_upstream::parse_source(&source).map_err(AppError::invalid_input)?
    };
    let parsed = match parsed {
        McpSource::Git { repo_url, .. } => {
            let clone_path =
                mcp_upstream::ensure_git_clone(&repo_url, None, proxy).map_err(AppError::git)?;
            McpSource::Git { repo_url, clone_path }
        }
        other => other,
    };
    serde_json::to_value(&parsed).map_err(|e| AppError::internal(e))
}

fn load_record(store: &SkillStore, id: &str) -> Result<McpServerRecord, AppError> {
    store
        .get_mcp_server_by_id(id)
        .map_err(store_err)?
        .ok_or_else(|| AppError::not_found(format!("MCP server {id} not found")))
}

/// Resolve where an agent's MCP config lives and which writer edits it.
fn resolve_agent_config(agent_key: &str) -> Result<(PathBuf, &'static dyn McpWriter), AppError> {
    let writer = mcp_writers::writer_for_agent(agent_key).ok_or_else(|| {
        AppError::invalid_input(format!("no MCP config writer for agent '{agent_key}'"))
    })?;
    let path = mcp_inventory::agent_config_path(agent_key).ok_or_else(|| {
        AppError::invalid_input(format!("unknown MCP agent '{agent_key}'"))
    })?;
    Ok((path, writer))
}

/// Current stored definition of `name` in `text`, paired with its
/// fingerprint. `None` covers both "absent" and "file not parseable" — the
/// writer operation below turns a real parse failure into a hard refusal.
fn current_entry_and_fingerprint(
    writer: &dyn McpWriter,
    text: &str,
    name: &str,
) -> Option<(McpEntryDef, String)> {
    let entry = writer.read_entry(text, name).ok()??;
    let fingerprint = entry_fingerprint(&entry);
    Some((entry, fingerprint))
}

/// Commitment hash for one drift confirmation: bound to the agent, the
/// entry name, the fingerprint the ledger believes is on disk, and the
/// definition revision — so a token can never authorize a different or
/// newer pending change (same reasoning as `removal_approval` in skills.rs).
fn drift_token(
    agent_key: &str,
    name: &str,
    binding_fingerprint: &str,
    record_updated_at: i64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"mcp-drift-v1");
    for part in [
        agent_key,
        name,
        binding_fingerprint,
        &record_updated_at.to_string(),
    ] {
        hasher.update([0u8]);
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ── the single write funnel ──

/// Read-modify-write one entry in one agent file: read text (with the
/// per-agent missing-file policy), run the writer transform, gate on drift,
/// then atomic-write, re-validate, and reconcile the binding ledger.
///
/// The testable core; [`write_entry_to_agent`] is the production wrapper that
/// resolves the agent's real path.
pub(crate) fn write_entry_to_agent_at(
    store: &SkillStore,
    record: &McpServerRecord,
    agent_key: &str,
    path: &Path,
    writer: &dyn McpWriter,
    op: WriteOp,
    approved_drift: Option<&str>,
) -> Result<WriteOutcome, AppError> {
    // Serialize with the background sync/backup round across the agent-file
    // read-modify-write (ADR-0006 §4). DB steps take the store's own mutex,
    // and nothing in here touches the network.
    let _lock = RepoLock::acquire_foreground("mcp config write").map_err(AppError::db)?;

    let entry = entry_from_record(record);
    let bindings = store
        .get_mcp_bindings_for_server(&record.id)
        .map_err(store_err)?;
    let binding = bindings.into_iter().find(|b| b.agent_key == agent_key);

    let mut file_existed = true;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            // An absent OpenCode config is a valid empty slate the manager
            // may seed; the DSH patch file belongs to DSH, so creating it
            // blind is an Unsafe refusal (ADR-0006 §4).
            if agent_key == "opencode" {
                file_existed = false;
                OPENCODE_EMPTY_TEMPLATE.to_string()
            } else {
                return Err(AppError::invalid_input("DSH patch file not found"));
            }
        }
        Err(err) => return Err(AppError::io(err)),
    };

    // A Remove against a file that no longer exists can only fix the ledger.
    if !file_existed && op == WriteOp::Remove {
        store.delete_mcp_binding(&record.id, agent_key).ok();
        return Ok(WriteOutcome::Applied);
    }

    let current = current_entry_and_fingerprint(writer, &text, &entry.name);
    let planned_text = match op {
        WriteOp::Upsert => writer.upsert(&text, &entry),
        WriteOp::Remove => writer.remove(&text, &record.name),
    }
    .map_err(writer_err)?;

    // Foreign name collision (ADR-0005 §7): an entry we did not write.
    // Equal-to-planned is safe to adopt (the write changes no bytes of
    // meaning); anything else is refused with a takeover hint.
    if binding.is_none() {
        if let Some((_, current_fp)) = &current {
            let planned_fp = current_entry_and_fingerprint(writer, &planned_text, &record.name)
                .map(|(_, fp)| fp);
            let owns_it = matches!(&planned_fp, Some(fp) if fp == current_fp);
            if !owns_it {
                return Err(AppError::invalid_input(format!(
                    "agent '{agent_key}' already configures \"{}\" differently; take it over first or remove it there",
                    record.name
                )));
            }
        }
    }

    // Drift gate: the ledger claims we wrote `binding.fingerprint`, the disk
    // must agree (or be gone — a vanished entry still needs a confirmation
    // before we recreate or finalize its removal).
    if let Some(binding) = &binding {
        let drifted = match &current {
            Some((_, fp)) => fp != &binding.fingerprint,
            None => true,
        };
        if drifted {
            let token =
                drift_token(agent_key, &record.name, &binding.fingerprint, record.updated_at);
            if approved_drift != Some(token.as_str()) {
                return Ok(WriteOutcome::PendingDrift(PendingDriftDto {
                    agent_key: agent_key.to_string(),
                    token,
                    current_text: text,
                    planned_text,
                }));
            }
        }
    }

    if planned_text != text {
        atomic_write_text(path, &planned_text).map_err(writer_err)?;
    }

    // Re-read the file we just wrote and validate the entry state before the
    // ledger claims anything about it.
    let verify_text = std::fs::read_to_string(path).map_err(|err| {
        AppError::io(format!("re-read {} after write failed: {err}", path.display()))
    })?;
    let applied = current_entry_and_fingerprint(writer, &verify_text, &record.name);
    match op {
        WriteOp::Upsert => {
            let fingerprint = applied.map(|(_, fp)| fp).ok_or_else(|| {
                AppError::internal(format!(
                    "post-write validation failed: \"{}\" is absent after upsert",
                    record.name
                ))
            })?;
            store
                .upsert_mcp_binding(&McpBindingRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    server_id: record.id.clone(),
                    agent_key: agent_key.to_string(),
                    // The fingerprint of the entry as the file really reads
                    // back, not of the in-memory definition: lossy formats
                    // (opencode drops remote env / maps streamable-http to
                    // "http") would otherwise report phantom drift forever.
                    fingerprint,
                    written_at: now_ts(),
                })
                .map_err(store_err)?;
        }
        WriteOp::Remove => {
            if applied.is_some() {
                return Err(AppError::internal(format!(
                    "post-write validation failed: \"{}\" still present after removal",
                    record.name
                )));
            }
            store
                .delete_mcp_binding(&record.id, agent_key)
                .map_err(store_err)?;
        }
    }
    Ok(WriteOutcome::Applied)
}

fn write_with_resolver(
    store: &SkillStore,
    record: &McpServerRecord,
    agent_key: &str,
    op: WriteOp,
    approved_drift: Option<&str>,
    resolve: AgentResolver,
) -> Result<WriteOutcome, AppError> {
    let (path, writer) = resolve(agent_key)?;
    write_entry_to_agent_at(store, record, agent_key, &path, writer, op, approved_drift)
}

/// Production entry point for one (server, agent) write (Task 7 contract).
pub(crate) fn write_entry_to_agent(
    store: &SkillStore,
    record: &McpServerRecord,
    agent_key: &str,
    op: WriteOp,
    approved_drift: Option<&str>,
) -> Result<WriteOutcome, AppError> {
    write_with_resolver(store, record, agent_key, op, approved_drift, &resolve_agent_config)
}

// ── library read ──

/// Every managed definition with every binding's LIVE drift verdict. Each
/// bound agent file is read once (bindings grouped by agent), never once per
/// binding.
pub(crate) fn get_mcp_library_internal(
    store: &SkillStore,
    resolve: AgentResolver,
) -> Result<McpLibraryReport, AppError> {
    let servers = store.get_all_mcp_servers().map_err(store_err)?;
    let bindings = store.get_all_mcp_bindings().map_err(store_err)?;
    let names: BTreeMap<&str, &str> = servers
        .iter()
        .map(|server| (server.id.as_str(), server.name.as_str()))
        .collect();

    // agent_key -> resolve outcome; the file text is loaded lazily per agent
    // and shared by all of that agent's bindings.
    struct AgentState {
        writer: &'static dyn McpWriter,
        text: Option<String>,
    }
    let mut states: BTreeMap<String, Result<AgentState, String>> = BTreeMap::new();
    let mut by_server: BTreeMap<String, Vec<McpBindingDto>> = BTreeMap::new();

    for binding in &bindings {
        let state = states
            .entry(binding.agent_key.clone())
            .or_insert_with(|| match resolve(&binding.agent_key) {
                Ok((path, writer)) => {
                    let text: Result<Option<String>, String> = match std::fs::read_to_string(&path) {
                        Ok(text) => Ok(Some(text)),
                        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
                        Err(err) => {
                            Err(format!("config file unreadable: {err}"))
                        }
                    };
                    match text {
                        Ok(text) => Ok(AgentState { writer, text }),
                        Err(message) => Err(message),
                    }
                }
                Err(err) => Err(err.message),
            });

        let server_name = names
            .get(binding.server_id.as_str())
            .copied()
            .unwrap_or("<deleted>");
        let (drift, drift_reason) = match state {
            Err(message) => (true, Some(message.clone())),
            Ok(state) => match &state.text {
                // The file is gone: nothing matches what we wrote.
                None => (true, Some("config file not found".to_string())),
                Some(text) => match state.writer.read_entry(text, server_name) {
                    Ok(Some(entry)) if entry_fingerprint(&entry) == binding.fingerprint => {
                        (false, None)
                    }
                    Ok(Some(_)) => (
                        true,
                        Some("entry was hand-edited since the last write".to_string()),
                    ),
                    Ok(None) => (true, Some("entry is no longer in the config file".to_string())),
                    Err(err) => (true, Some(format!("config file not parseable: {err}"))),
                },
            },
        };

        by_server
            .entry(binding.server_id.clone())
            .or_default()
            .push(McpBindingDto {
                agent_key: binding.agent_key.clone(),
                fingerprint: binding.fingerprint.clone(),
                drift,
                drift_reason,
            });
    }

    Ok(McpLibraryReport {
        servers: servers
            .into_iter()
            .map(|record| McpServerDto {
                bindings: by_server.remove(&record.id).unwrap_or_default(),
                record,
            })
            .collect(),
        agents: mcp_inventory::scan_mcp_inventory().agents,
    })
}

// ── library write operations ──

pub(crate) fn add_mcp_server_internal(
    store: &SkillStore,
    entry: McpEntryDefDto,
    source: JsonValue,
    agents: &[String],
    resolve: AgentResolver,
) -> Result<String, AppError> {
    let name = validate_name(&entry.name)?;
    if store
        .get_mcp_server_by_name(&name)
        .map_err(store_err)?
        .is_some()
    {
        return Err(AppError::invalid_input(format!(
            "a managed MCP server named \"{name}\" already exists"
        )));
    }
    if entry.transport.trim().is_empty() {
        return Err(AppError::invalid_input("MCP transport must not be empty"));
    }
    // Network step first, outside any lock: a failed clone aborts before a
    // record exists (ADR-0006 §1).
    let source = prepare_source(source, store.proxy_url().as_deref())?;

    let now = now_ts();
    let record = McpServerRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        transport: entry.transport,
        command: entry.command,
        args: entry.args,
        url: entry.url,
        env: entry.env,
        source,
        update_status: "unknown".to_string(),
        remote_version: None,
        last_checked_at: None,
        last_check_error: None,
        probe_status: "pending".to_string(),
        probe_message: None,
        probe_checked_at: None,
        created_at: now,
        updated_at: now,
    };
    // Every persisted record is validated, takeover included: a stored entry
    // can be re-emitted to DSH by a later sync (ADR-0006 §4's surgery must
    // never see a scalar that can fake a column-0 block opener).
    validate_entry_scalars(&entry_from_record(&record))?;
    store.insert_mcp_server(&record).map_err(store_err)?;

    // Fresh add: no bindings exist yet, so drift is structurally impossible;
    // only foreign-collision/resolver errors can fail an agent. A partial add
    // stays partial — the record and every successful binding are kept.
    let mut failures: Vec<String> = Vec::new();
    for agent in agents {
        match write_with_resolver(store, &record, agent, WriteOp::Upsert, None, resolve) {
            Ok(WriteOutcome::Applied) => {}
            Ok(WriteOutcome::PendingDrift(drift)) => failures.push(format!(
                "{}: refused, an unmanaged same-name entry needs takeover",
                drift.agent_key
            )),
            Err(err) => failures.push(format!("{agent}: {}", err.message)),
        }
    }
    let detail = format!("agents: {}", agents.join(", "));
    if failures.is_empty() {
        store.log_audit(
            AuditDraft::new("mcp_add")
                .skill(record.id.clone(), name)
                .detail(detail)
                .ok(),
        );
    } else {
        let joined = failures.join("; ");
        log::warn!("mcp add: {name} kept, but some agents not updated: {joined}");
        store.log_audit(
            AuditDraft::new("mcp_add")
                .skill(record.id.clone(), name)
                .detail(format!("{detail}; {joined}"))
                .fail(joined),
        );
    }
    Ok(record.id)
}

pub(crate) fn edit_mcp_server_internal(
    store: &SkillStore,
    id: &str,
    entry: McpEntryDefDto,
    source: JsonValue,
    approved_drift: Option<&str>,
    resolve: AgentResolver,
) -> Result<EditOutcome, AppError> {
    let current = load_record(store, id)?;
    let name = validate_name(&entry.name)?;
    let renamed = name != current.name;
    if renamed
        && store
            .get_mcp_server_by_name(&name)
            .map_err(store_err)?
            .is_some()
    {
        return Err(AppError::invalid_input(format!(
            "a managed MCP server named \"{name}\" already exists"
        )));
    }
    let source = prepare_source(source, store.proxy_url().as_deref())?;
    let bindings = store
        .get_mcp_bindings_for_server(id)
        .map_err(store_err)?;

    let mut updated = current.clone();
    updated.name = name.clone();
    updated.transport = entry.transport;
    updated.command = entry.command;
    updated.args = entry.args;
    updated.url = entry.url;
    updated.env = entry.env;
    updated.source = source;
    validate_entry_scalars(&entry_from_record(&updated))?;
    let content_changed = current.name != updated.name
        || current.transport != updated.transport
        || current.command != updated.command
        || current.args != updated.args
        || current.url != updated.url
        || current.env != updated.env
        || current.source != updated.source;

    let mut outcome = EditOutcome {
        applied: Vec::new(),
        pending_drift: Vec::new(),
    };

    if renamed {
        // Rename = remove-old + upsert-new per bound agent (ADR-0006 §3:
        // agent configs key entries by name, so the old name must vanish).
        //
        // Ordering keeps drift tokens replayable: the DB commit bumps
        // `updated_at` (which the token binds), so it happens only after
        // every bound agent has accepted the removal. A retry of a
        // declined removal recomputes the same token from the still-old
        // record; already-removed agents replay the Remove as a no-op.
        let mut removed_agents: Vec<String> = Vec::new();
        for binding in &bindings {
            match write_with_resolver(
                store,
                &current,
                &binding.agent_key,
                WriteOp::Remove,
                approved_drift,
                resolve,
            )? {
                WriteOutcome::Applied => removed_agents.push(binding.agent_key.clone()),
                WriteOutcome::PendingDrift(drift) => outcome.pending_drift.push(drift),
            }
        }
        if !outcome.pending_drift.is_empty() {
            // Definition stays on the old name in the library too, so the
            // next attempt sees the same rename and the same tokens.
            return Ok(outcome);
        }
        updated.updated_at = now_ts();
        store.update_mcp_server(&updated).map_err(store_err)?;
        for agent in removed_agents {
            match write_with_resolver(store, &updated, &agent, WriteOp::Upsert, None, resolve)? {
                WriteOutcome::Applied => outcome.applied.push(agent),
                // No binding exists at this point, so a drift here would be
                // a foreign collision the funnel already vetted; keep it
                // visible rather than invent a second outcome lane.
                WriteOutcome::PendingDrift(drift) => outcome.pending_drift.push(drift),
            }
        }
        return Ok(outcome);
    }

    // Same-name edit. Only bump the revision when the content actually
    // changed: a drift-retry carries the token minted from the *already
    // committed* revision, so re-stamping it would invalidate that token.
    if content_changed {
        updated.updated_at = now_ts();
        store.update_mcp_server(&updated).map_err(store_err)?;
    } else {
        updated = current.clone();
    }
    for binding in &bindings {
        match write_with_resolver(
            store,
            &updated,
            &binding.agent_key,
            WriteOp::Upsert,
            approved_drift,
            resolve,
        )? {
            WriteOutcome::Applied => outcome.applied.push(binding.agent_key.clone()),
            WriteOutcome::PendingDrift(drift) => outcome.pending_drift.push(drift),
        }
    }
    Ok(outcome)
}

pub(crate) fn delete_mcp_server_internal(
    store: &SkillStore,
    id: &str,
    approved_drift: Option<&str>,
    resolve: AgentResolver,
) -> Result<EditOutcome, AppError> {
    let record = load_record(store, id)?;
    let bindings = store
        .get_mcp_bindings_for_server(id)
        .map_err(store_err)?;

    let mut outcome = EditOutcome {
        applied: Vec::new(),
        pending_drift: Vec::new(),
    };
    for binding in &bindings {
        match write_with_resolver(
            store,
            &record,
            &binding.agent_key,
            WriteOp::Remove,
            approved_drift,
            resolve,
        )? {
            WriteOutcome::Applied => outcome.applied.push(binding.agent_key.clone()),
            WriteOutcome::PendingDrift(drift) => outcome.pending_drift.push(drift),
        }
    }

    // The row (and its binding ledger via CASCADE) goes only when no agent
    // is still waiting on a confirmation; otherwise the record stays and the
    // caller re-invokes with the token. Already-applied agents keep their
    // file cleanup — the DB never claims a write that was rolled back.
    if outcome.pending_drift.is_empty() {
        store.delete_mcp_server(id).map_err(store_err)?;
        store.log_audit(
            AuditDraft::new("mcp_delete")
                .skill(id.to_string(), record.name.clone())
                .detail(format!("removed from: {}", outcome.applied.join(", ")))
                .ok(),
        );
    } else {
        store.log_audit(
            AuditDraft::new("mcp_delete")
                .skill(id.to_string(), record.name.clone())
                .detail("kept: drift confirmation pending")
                .fail("pending drift confirmation"),
        );
    }
    Ok(outcome)
}

pub(crate) fn sync_mcp_to_agent_internal(
    store: &SkillStore,
    id: &str,
    agent_key: &str,
    approved_drift: Option<&str>,
    resolve: AgentResolver,
) -> Result<WriteOutcome, AppError> {
    let record = load_record(store, id)?;
    write_with_resolver(store, &record, agent_key, WriteOp::Upsert, approved_drift, resolve)
}

pub(crate) fn unsync_mcp_from_agent_internal(
    store: &SkillStore,
    id: &str,
    agent_key: &str,
    approved_drift: Option<&str>,
    resolve: AgentResolver,
) -> Result<WriteOutcome, AppError> {
    let record = load_record(store, id)?;
    let managed = store
        .get_mcp_bindings_for_server(id)
        .map_err(store_err)?
        .iter()
        .any(|b| b.agent_key == agent_key);
    if !managed {
        return Err(AppError::not_found(format!(
            "\"{}\" is not synced to agent '{agent_key}' by this library",
            record.name
        )));
    }
    write_with_resolver(store, &record, agent_key, WriteOp::Remove, approved_drift, resolve)
}

/// Adopt a foreign agent entry: read its full definition (env values and
/// all), store it as a managed definition with an inferred upstream source,
/// and bind it at the CURRENT fingerprint — takeover never rewrites the
/// file, it mirrors reality into the ledger.
pub(crate) fn takeover_mcp_entry_internal(
    store: &SkillStore,
    agent_key: &str,
    server_name: &str,
    resolve: AgentResolver,
) -> Result<String, AppError> {
    let (path, writer) = resolve(agent_key)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| AppError::io(format!("read {} failed: {err}", path.display())))?;
    if store
        .get_mcp_server_by_name(server_name)
        .map_err(store_err)?
        .is_some()
    {
        return Err(AppError::invalid_input(format!(
            "a managed MCP server named \"{server_name}\" already exists; takeover would collide"
        )));
    }
    let entry = writer
        .read_entry(&text, server_name)
        .map_err(writer_err)?
        .ok_or_else(|| {
            AppError::not_found(format!(
                "no MCP entry \"{server_name}\" in agent '{agent_key}' config"
            ))
        })?;

    let source = serde_json::to_value(mcp_upstream::infer_source(
        entry.command.as_deref(),
        &entry.args,
    ))
    .map_err(|e| AppError::internal(e))?;
    let now = now_ts();
    let record = McpServerRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: entry.name.clone(),
        transport: entry.transport.clone(),
        command: entry.command.clone(),
        args: entry.args.clone(),
        url: entry.url.clone(),
        env: entry.env.clone(),
        source,
        update_status: "unknown".to_string(),
        remote_version: None,
        last_checked_at: None,
        last_check_error: None,
        probe_status: "pending".to_string(),
        probe_message: None,
        probe_checked_at: None,
        created_at: now,
        updated_at: now,
    };
    // Every persisted record is validated, takeover included: a stored entry
    // can be re-emitted to DSH by a later sync (ADR-0006 §4's surgery must
    // never see a scalar that can fake a column-0 block opener).
    validate_entry_scalars(&entry_from_record(&record))?;
    store.insert_mcp_server(&record).map_err(store_err)?;
    store
        .upsert_mcp_binding(&McpBindingRecord {
            id: uuid::Uuid::new_v4().to_string(),
            server_id: record.id.clone(),
            agent_key: agent_key.to_string(),
            fingerprint: entry_fingerprint(&entry),
            written_at: now,
        })
        .map_err(store_err)?;
    store.log_audit(
        AuditDraft::new("mcp_takeover")
            .skill(record.id.clone(), record.name.clone())
            .detail(format!("adopted from agent '{agent_key}'"))
            .ok(),
    );
    Ok(record.id)
}

// ── probe & upstream ──

pub(crate) fn probe_mcp_server_internal(
    store: &SkillStore,
    id: &str,
) -> Result<ProbeStateDto, AppError> {
    let record = load_record(store, id)?;
    let entry = entry_from_record(&record);
    let proxy = store.proxy_url();
    let (probe_status, probe_message) = match mcp_probe::probe_entry(
        &entry,
        proxy.as_deref(),
        PROBE_TIMEOUT,
    ) {
        Ok(info) => {
            let described = match (info.server_name, info.server_version) {
                (Some(name), Some(version)) => format!("{name} {version}"),
                (Some(name), None) => name,
                (None, Some(version)) => version,
                (None, None) => "handshake ok".to_string(),
            };
            ("ok".to_string(), Some(described))
        }
        Err(message) => ("fail".to_string(), Some(message)),
    };
    store
        .set_mcp_probe_state(id, &probe_status, probe_message.as_deref())
        .map_err(store_err)?;
    Ok(ProbeStateDto {
        probe_status,
        probe_message,
    })
}

/// Run the whole-library upstream check and report how many definitions were
/// examined. `force` mirrors the skills command signature; nothing extra is
/// bypassed yet (checks are cheap and stateless).
pub(crate) fn check_mcp_updates_internal(store: &SkillStore, force: Option<bool>) -> u32 {
    let _force = force.unwrap_or(false);
    let proxy = store.proxy_url();
    mcp_upstream::check_latest(store, proxy.as_deref()).len() as u32
}

pub(crate) fn get_mcp_upgrade_plan_internal(
    store: &SkillStore,
    id: &str,
) -> Result<UpgradePlanDto, AppError> {
    let record = load_record(store, id)?;
    let commands = mcp_upstream::upgrade_commands(&record).map_err(AppError::invalid_input)?;
    Ok(UpgradePlanDto {
        commands,
        latest_version: record.remote_version,
    })
}

/// Execute the upgrade for one definition. The command list is re-derived
/// from the stored record — a client-side list could be stale or hostile
/// (ADR-0006 §1's confirm-then-run-exactly-what-was-shown contract).
pub(crate) fn apply_mcp_upgrade_internal(store: &SkillStore, id: &str) -> Result<String, AppError> {
    let record = load_record(store, id)?;
    let name = record.name.clone();
    let commands = mcp_upstream::upgrade_commands(&record).map_err(AppError::invalid_input)?;
    let output = match mcp_upstream::run_commands(&commands) {
        Ok(output) => output,
        Err(err) => {
            // Format before the audit moves `err`: the user-facing message and
            // the audit trail must say the same thing.
            let message = format!("MCP upgrade failed: {err}");
            store.log_audit(
                AuditDraft::new("mcp_upgrade")
                    .skill(id.to_string(), name)
                    .fail(err),
            );
            return Err(AppError::internal(message));
        }
    };

    // Refresh version state (the check persists per-server results), then
    // re-probe so the card shows the post-upgrade reality.
    let proxy = store.proxy_url();
    let check_note = mcp_upstream::check_latest(store, proxy.as_deref())
        .into_iter()
        .find(|(server_id, _)| server_id == id)
        .map(|(_, outcome)| match (&outcome.error, outcome.behind, &outcome.latest) {
            (Some(err), _, _) => format!("check failed: {err}"),
            (None, true, Some(latest)) => format!("still behind latest {latest}"),
            (None, _, Some(latest)) => format!("latest is {latest}"),
            (None, _, None) => "checked, no remote version".to_string(),
        })
        .unwrap_or_else(|| "check skipped".to_string());
    let probe = probe_mcp_server_internal(store, id)?;
    let probe_note = format!(
        "probe { }",
        probe.probe_status,
    ) + &probe
        .probe_message
        .map(|message| format!(": {message}"))
        .unwrap_or_default();
    store.log_audit(
        AuditDraft::new("mcp_upgrade")
            .skill(id.to_string(), name)
            .detail(format!("{check_note}; {probe_note}"))
            .ok(),
    );
    Ok(format!(
        "{output}\n\nversion check: {check_note}\n{liveness}: {probe_note}",
        liveness = "probe"
    ))
}

// ── v1 read-only inventory (ADR-0005, unchanged) ──

/// Scan supported agents (OpenCode, DeepSeek Harness) for configured MCP
/// servers. Never fails: per-agent read/parse problems are reported inside the
/// payload so one broken config cannot hide the others.
#[tauri::command]
pub async fn get_mcp_inventory() -> mcp_inventory::McpInventoryReport {
    mcp_inventory::scan_mcp_inventory()
}

// ── v2 commands (thin State wrappers) ──

#[tauri::command]
pub async fn get_mcp_library(
    store: State<'_, Arc<SkillStore>>,
) -> Result<McpLibraryReport, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        get_mcp_library_internal(&store, &resolve_agent_config)
    })
    .await?
}

#[tauri::command]
pub async fn add_mcp_server(
    entry: McpEntryDefDto,
    source: JsonValue,
    agents: Vec<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<String, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        add_mcp_server_internal(&store, entry, source, &agents, &resolve_agent_config)
    })
    .await?
}

#[tauri::command]
pub async fn edit_mcp_server(
    id: String,
    entry: McpEntryDefDto,
    source: JsonValue,
    approved_drift: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<EditOutcome, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        edit_mcp_server_internal(
            &store,
            &id,
            entry,
            source,
            approved_drift.as_deref(),
            &resolve_agent_config,
        )
    })
    .await?
}

#[tauri::command]
pub async fn delete_mcp_server(
    id: String,
    approved_drift: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<EditOutcome, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        delete_mcp_server_internal(
            &store,
            &id,
            approved_drift.as_deref(),
            &resolve_agent_config,
        )
    })
    .await?
}

#[tauri::command]
pub async fn sync_mcp_to_agent(
    id: String,
    agent: String,
    approved_drift: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<WriteOutcome, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        sync_mcp_to_agent_internal(
            &store,
            &id,
            &agent,
            approved_drift.as_deref(),
            &resolve_agent_config,
        )
    })
    .await?
}

#[tauri::command]
pub async fn unsync_mcp_from_agent(
    id: String,
    agent: String,
    approved_drift: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<WriteOutcome, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        unsync_mcp_from_agent_internal(
            &store,
            &id,
            &agent,
            approved_drift.as_deref(),
            &resolve_agent_config,
        )
    })
    .await?
}

#[tauri::command]
pub async fn takeover_mcp_entry(
    agent_key: String,
    server_name: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<String, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        takeover_mcp_entry_internal(&store, &agent_key, &server_name, &resolve_agent_config)
    })
    .await?
}

#[tauri::command]
pub async fn probe_mcp_server(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<ProbeStateDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || probe_mcp_server_internal(&store, &id)).await?
}

#[tauri::command]
pub async fn check_mcp_updates(
    force: Option<bool>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<u32, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        Ok(check_mcp_updates_internal(&store, force))
    })
    .await?
}

#[tauri::command]
pub async fn get_mcp_upgrade_plan(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<UpgradePlanDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || get_mcp_upgrade_plan_internal(&store, &id)).await?
}

#[tauri::command]
pub async fn apply_mcp_upgrade(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<String, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || apply_mcp_upgrade_internal(&store, &id)).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp_writers::writer_for_agent;
    use serde_json::json;
    use tempfile::{tempdir, TempDir};

    fn record(name: &str, command: &str, args: &[&str]) -> McpServerRecord {
        McpServerRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args: args.iter().map(|a| a.to_string()).collect(),
            url: None,
            env: BTreeMap::new(),
            source: json!({"kind": "none"}),
            update_status: "unknown".to_string(),
            remote_version: None,
            last_checked_at: None,
            last_check_error: None,
            probe_status: "pending".to_string(),
            probe_message: None,
            probe_checked_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    struct Harness {
        _dir: TempDir,
        store: SkillStore,
        opencode: PathBuf,
        resolver: Box<dyn Fn(&str) -> Result<(PathBuf, &'static dyn McpWriter), AppError> + Send + Sync>,
    }

    fn harness() -> Harness {
        let dir = tempdir().unwrap();
        let store = SkillStore::new(&dir.path().join("test.db")).unwrap();
        let opencode = dir.path().join("opencode.json");
        let resolver_path = opencode.clone();
        Harness {
            _dir: dir,
            store,
            opencode,
            resolver: Box::new(move |key: &str| {
                let writer = writer_for_agent(key).ok_or_else(|| {
                    AppError::invalid_input(format!("unknown agent {key}"))
                })?;
                Ok((resolver_path.clone(), writer))
            }),
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// A complete opencode document holding one local entry — the shape the
    /// writer actually reads. (The old fixture omitted the `mcp` wrapper and
    /// nested the args array, so "foreign"/"tampered" scenarios silently
    /// became "entry absent" scenarios.)
    fn entry_json(name: &str, command: &str, args: &[&str]) -> serde_json::Value {
        let mut cmd = vec![command.to_string()];
        cmd.extend(args.iter().map(|a| a.to_string()));
        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), serde_json::Value::String("local".into()));
        entry.insert(
            "command".into(),
            serde_json::Value::Array(cmd.into_iter().map(serde_json::Value::String).collect()),
        );
        let mut mcp = serde_json::Map::new();
        mcp.insert(name.to_string(), serde_json::Value::Object(entry));
        let mut root = serde_json::Map::new();
        root.insert("mcp".into(), serde_json::Value::Object(mcp));
        serde_json::Value::Object(root)
    }

    #[test]
    fn drift_token_is_stable_and_bound_to_every_input() {
        let base = drift_token("opencode", "x", "fp1", 7);
        assert_eq!(base, drift_token("opencode", "x", "fp1", 7));
        assert_eq!(base.len(), 64);
        assert_ne!(base, drift_token("deepseek_harness", "x", "fp1", 7));
        assert_ne!(base, drift_token("opencode", "y", "fp1", 7));
        assert_ne!(base, drift_token("opencode", "x", "fp2", 7));
        assert_ne!(base, drift_token("opencode", "x", "fp1", 8));
        // NUL-separated hashing keeps field-boundary shifts distinct.
        assert_ne!(
            drift_token("a", "bc", "fp", 1),
            drift_token("ab", "c", "fp", 1)
        );
    }

    #[test]
    fn upsert_seeds_missing_opencode_file_from_template_and_binds() {
        let h = harness();
        let rec = record("zeta", "npx", &["-y", "@o/zeta"]);
        h.store.insert_mcp_server(&rec).unwrap();
        assert!(!h.opencode.exists());

        let out = sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver)
            .unwrap();
        assert!(matches!(out, WriteOutcome::Applied));

        let text = read(&h.opencode);
        assert!(text.contains("\"zeta\""), "seeded file: {text}");
        let bindings = h.store.get_mcp_bindings_for_server(&rec.id).unwrap();
        assert_eq!(bindings.len(), 1);
        // The ledger fingerprint must match what a future drift read
        // recomputes from the file (not the in-memory definition).
        let writer = writer_for_agent("opencode").unwrap();
        let entry = writer.read_entry(&text, "zeta").unwrap().unwrap();
        assert_eq!(bindings[0].fingerprint, entry_fingerprint(&entry));
    }

    #[test]
    fn missing_dsh_patch_file_is_refused_not_created() {
        let dir = tempdir().unwrap();
        let store = SkillStore::new(&dir.path().join("test.db")).unwrap();
        let rec = record("dshy", "npx", &["-y", "@o/x"]);
        store.insert_mcp_server(&rec).unwrap();
        let path = dir.path().join("cordis.patch.yml");
        let writer = writer_for_agent("deepseek_harness").unwrap();

        let err = write_entry_to_agent_at(
            &store,
            &rec,
            "deepseek_harness",
            &path,
            writer,
            WriteOp::Upsert,
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, crate::core::error::ErrorKind::InvalidInput);
        assert_eq!(err.message, "DSH patch file not found");
        assert!(!path.exists(), "refusal must not create the file");
    }

    #[test]
    fn drift_gate_blocks_write_then_matching_token_unblocks_it() {
        let h = harness();
        let rec = record("mem", "npx", &["-y", "@o/mem"]);
        h.store.insert_mcp_server(&rec).unwrap();
        sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver).unwrap();
        let written = read(&h.opencode);

        // Human edits the file behind the ledger's back.
        let edited = writer_for_agent("opencode")
            .unwrap()
            .upsert(
                &written,
                &McpEntryDef {
                    name: "mem".into(),
                    transport: "stdio".into(),
                    command: Some("pnpm".into()),
                    args: vec!["dlx".into(), "@o/mem".into()],
                    url: None,
                    env: BTreeMap::new(),
                },
            )
            .unwrap();
        std::fs::write(&h.opencode, &edited).unwrap();

        let old_fp = h.store.get_mcp_bindings_for_server(&rec.id).unwrap()[0]
            .fingerprint
            .clone();

        let pending = match sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver)
            .unwrap()
        {
            WriteOutcome::PendingDrift(drift) => drift,
            other => panic!("drift must block, got {other:?}"),
        };
        assert_eq!(pending.agent_key, "opencode");
        assert_eq!(
            pending.token,
            drift_token("opencode", "mem", &old_fp, rec.updated_at)
        );
        assert_eq!(pending.current_text, edited);
        assert!(
            pending.planned_text.contains("npx"),
            "plan restores the library definition"
        );
        assert_eq!(read(&h.opencode), edited, "blocked write must not touch the file");

        // A wrong token stays pending...
        assert!(matches!(
            sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", Some("bogus"), &h.resolver)
                .unwrap(),
            WriteOutcome::PendingDrift(_)
        ));

        // ...the right one applies.
        let out = sync_mcp_to_agent_internal(
            &h.store,
            &rec.id,
            "opencode",
            Some(&pending.token),
            &h.resolver,
        )
        .unwrap();
        assert!(matches!(out, WriteOutcome::Applied));
        assert!(read(&h.opencode).contains("npx"));
    }

    #[test]
    fn library_report_computes_live_drift_per_agent_once() {
        let h = harness();
        let a = record("aa", "npx", &["-y", "@o/aa"]);
        let b = record("bb", "npx", &["-y", "@o/bb"]);
        h.store.insert_mcp_server(&a).unwrap();
        h.store.insert_mcp_server(&b).unwrap();
        sync_mcp_to_agent_internal(&h.store, &a.id, "opencode", None, &h.resolver).unwrap();
        sync_mcp_to_agent_internal(&h.store, &b.id, "opencode", None, &h.resolver).unwrap();

        let report = get_mcp_library_internal(&h.store, &h.resolver).unwrap();
        assert_eq!(report.servers.len(), 2);
        for server in &report.servers {
            assert_eq!(server.bindings.len(), 1);
            assert!(!server.bindings[0].drift, "{} clean", server.record.name);
        }

        // Wipe the whole file: every binding flips to drifted with a reason.
        std::fs::remove_file(&h.opencode).unwrap();
        let report = get_mcp_library_internal(&h.store, &h.resolver).unwrap();
        let drifted: Vec<_> = report
            .servers
            .iter()
            .flat_map(|s| &s.bindings)
            .filter(|binding| binding.drift)
            .collect();
        assert_eq!(drifted.len(), 2);
        assert!(drifted.iter().all(|binding| {
            binding
                .drift_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("config file not found"))
        }));
    }

    #[test]
    fn takeover_adopts_fingerprint_without_touching_the_file() {
        let h = harness();
        // A foreign entry, written by the user: full env included.
        let foreign = r#"{
  "mcp": {
    "gem": {
      "type": "local",
      "command": ["npx", "-y", "@mcp/gem"],
      "environment": { "GEM_KEY": "s3cret" }
    }
  }
}"#;
        std::fs::write(&h.opencode, foreign).unwrap();

        let id =
            takeover_mcp_entry_internal(&h.store, "opencode", "gem", &h.resolver).unwrap();
        assert_eq!(read(&h.opencode), foreign, "takeover never rewrites the file");

        let rec = h.store.get_mcp_server_by_id(&id).unwrap().unwrap();
        assert_eq!(rec.env.get("GEM_KEY").map(String::as_str), Some("s3cret"));
        assert_eq!(rec.source["kind"], "npx");
        assert_eq!(rec.source["package"], "@mcp/gem");

        let bindings = h.store.get_mcp_bindings_for_server(&id).unwrap();
        let entry = writer_for_agent("opencode")
            .unwrap()
            .read_entry(foreign, "gem")
            .unwrap()
            .unwrap();
        assert_eq!(bindings[0].fingerprint, entry_fingerprint(&entry));

        // Second takeover of the same name hits the managed-name collision guard.
        let err = takeover_mcp_entry_internal(&h.store, "opencode", "gem", &h.resolver)
            .unwrap_err();
        assert_eq!(err.kind, crate::core::error::ErrorKind::InvalidInput);
    }

    #[test]
    fn sync_over_foreign_same_name_is_refused() {
        let h = harness();
        std::fs::write(
            &h.opencode,
            entry_json("x", "/foreign/x", &["--other"]).to_string(),
        )
        .unwrap();
        let rec = record("x", "npx", &["-y", "@o/x"]);
        h.store.insert_mcp_server(&rec).unwrap();

        let err =
            sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver)
                .unwrap_err();
        assert_eq!(err.kind, crate::core::error::ErrorKind::InvalidInput);
        assert!(err.message.contains("take it over"), "got: {}", err.message);
    }

    /// A YAML single-quoted scalar passes raw newlines through, so a value
    /// like "ok\n- insert:" would land a block opener at column 0 of
    /// cordis.patch.yml and fool the writer's block scanner mid-surgery —
    /// while the re-parse validation still passes. The only safe gate is at
    /// the door: no control characters in any scalar that can be persisted.
    #[test]
    fn newline_in_a_scalar_is_refused_before_it_can_fake_a_block() {
        let h = harness();
        let mut env = BTreeMap::new();
        env.insert("EVIL".to_string(), "ok\n- insert:\n  fake".to_string());
        let entry = McpEntryDefDto {
            name: "nv".to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "@o/nv".to_string()],
            url: None,
            env,
        };
        let err =
            add_mcp_server_internal(&h.store, entry, json!({"kind": "none"}), &[], &h.resolver)
                .unwrap_err();
        assert_eq!(err.kind, crate::core::error::ErrorKind::InvalidInput);
        assert!(err.message.contains("env"), "got: {}", err.message);
        assert!(
            h.store.get_mcp_server_by_name("nv").unwrap().is_none(),
            "rejected add must persist nothing"
        );
    }

    #[test]
    fn edit_rename_removes_old_name_and_writes_new() {
        let h = harness();
        let rec = record("alpha", "npx", &["-y", "@o/alpha"]);
        h.store.insert_mcp_server(&rec).unwrap();
        sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver).unwrap();

        let dto = McpEntryDefDto {
            name: " beta ".to_string(),
            transport: "stdio".to_string(),
            command: Some("node".to_string()),
            args: vec!["/opt/beta/index.js".to_string()],
            url: None,
            env: BTreeMap::new(),
        };
        let outcome = edit_mcp_server_internal(
            &h.store,
            &rec.id,
            dto,
            json!({"kind": "none"}),
            None,
            &h.resolver,
        )
        .unwrap();
        assert_eq!(outcome.applied, vec!["opencode".to_string()]);
        assert!(outcome.pending_drift.is_empty());

        let text = read(&h.opencode);
        assert!(!text.contains("\"alpha\""), "old name must be gone: {text}");
        assert!(text.contains("\"beta\""), "new name must be present: {text}");
        assert!(text.contains("index.js"));

        let updated = h.store.get_mcp_server_by_id(&rec.id).unwrap().unwrap();
        assert_eq!(updated.name, "beta");
        let bindings = h.store.get_mcp_bindings_for_server(&rec.id).unwrap();
        assert_eq!(bindings.len(), 1);
        let entry = writer_for_agent("opencode")
            .unwrap()
            .read_entry(&text, "beta")
            .unwrap()
            .unwrap();
        assert_eq!(bindings[0].fingerprint, entry_fingerprint(&entry));
    }

    #[test]
    fn delete_defers_the_record_until_drift_is_approved() {
        let h = harness();
        let rec = record("del", "npx", &["-y", "@o/del"]);
        h.store.insert_mcp_server(&rec).unwrap();
        sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver).unwrap();

        // Hand-edit → removal is gated.
        std::fs::write(
            &h.opencode,
            entry_json("del", "tampered", &["--flag"]).to_string(),
        )
        .unwrap();
        let outcome =
            delete_mcp_server_internal(&h.store, &rec.id, None, &h.resolver).unwrap();
        assert!(outcome.applied.is_empty());
        assert_eq!(outcome.pending_drift.len(), 1);
        // Nothing deleted: record and binding both survive, file untouched.
        assert!(h.store.get_mcp_server_by_id(&rec.id).unwrap().is_some());
        assert_eq!(
            h.store.get_mcp_bindings_for_server(&rec.id).unwrap().len(),
            1
        );
        assert!(read(&h.opencode).contains("tampered"));

        let token = outcome.pending_drift[0].token.clone();
        let outcome = delete_mcp_server_internal(&h.store, &rec.id, Some(&token), &h.resolver)
            .unwrap();
        assert_eq!(outcome.applied, vec!["opencode".to_string()]);
        assert!(h.store.get_mcp_server_by_id(&rec.id).unwrap().is_none());
        assert!(h.store.get_all_mcp_bindings().unwrap().is_empty());
        assert!(!read(&h.opencode).contains("\"del\""));
    }

    #[test]
    fn add_reports_partial_agent_failures_but_keeps_record_and_bindings() {
        let dir = tempdir().unwrap();
        let store = SkillStore::new(&dir.path().join("test.db")).unwrap();
        let opencode = dir.path().join("opencode.json");
        let oc_path = opencode.clone();
        let resolver: Box<dyn Fn(&str) -> Result<(PathBuf, &'static dyn McpWriter), AppError> + Send + Sync> =
            Box::new(move |key: &str| {
                if key == "opencode" {
                    Ok((oc_path.clone(), writer_for_agent("opencode").unwrap()))
                } else {
                    Err(AppError::invalid_input(format!("no writer for {key}")))
                }
            });

        let entry = McpEntryDefDto {
            name: " new-one ".to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "@o/new".to_string()],
            url: None,
            env: BTreeMap::new(),
        };
        let id = add_mcp_server_internal(
            &store,
            entry,
            json!({"kind": "none"}),
            &["opencode".to_string(), "cursor".to_string()],
            &resolver,
        )
        .unwrap();

        // Record kept with the trimmed name; opencode bound; cursor failure
        // only reaches the audit log (partial add is honest).
        let rec = store.get_mcp_server_by_id(&id).unwrap().unwrap();
        assert_eq!(rec.name, "new-one");
        assert_eq!(rec.probe_status, "pending");
        assert_eq!(rec.update_status, "unknown");
        let bindings = store.get_mcp_bindings_for_server(&id).unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].agent_key, "opencode");
        assert!(read(&opencode).contains("\"new-one\""));

        // Duplicate names are rejected with a friendly message.
        let dup = McpEntryDefDto {
            name: "new-one".to_string(),
            transport: "stdio".to_string(),
            command: Some("true".to_string()),
            args: vec![],
            url: None,
            env: BTreeMap::new(),
        };
        let err =
            add_mcp_server_internal(&store, dup, json!({"kind": "none"}), &[], &resolver)
                .unwrap_err();
        assert!(err.message.contains("already exists"), "got: {}", err.message);
    }

    #[test]
    fn edit_retry_reuses_the_same_drift_token() {
        let h = harness();
        let rec = record("rt", "npx", &["-y", "@o/rt"]);
        h.store.insert_mcp_server(&rec).unwrap();
        sync_mcp_to_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver).unwrap();
        std::fs::write(&h.opencode, entry_json("rt", "tampered", &["x"]).to_string()).unwrap();

        let dto = McpEntryDefDto {
            name: "rt".to_string(),
            transport: "stdio".to_string(),
            command: Some("node".to_string()),
            args: vec!["/srv/rt/index.js".to_string()],
            url: None,
            env: BTreeMap::new(),
        };
        let first = edit_mcp_server_internal(
            &h.store,
            &rec.id,
            dto.clone(),
            json!({"kind": "none"}),
            None,
            &h.resolver,
        )
        .unwrap();
        assert_eq!(first.pending_drift.len(), 1);
        let token = first.pending_drift[0].token.clone();

        // Same payload + the returned token: the committed revision must not
        // move, or this retry could never authorize the first dialog.
        let second = edit_mcp_server_internal(
            &h.store,
            &rec.id,
            dto,
            json!({"kind": "none"}),
            Some(&token),
            &h.resolver,
        )
        .unwrap();
        assert!(second.pending_drift.is_empty());
        assert_eq!(second.applied, vec!["opencode".to_string()]);
        assert!(read(&h.opencode).contains("index.js"));
    }

    #[test]
    fn unsync_requires_an_existing_binding() {
        let h = harness();
        let rec = record("un", "npx", &["-y", "@o/un"]);
        h.store.insert_mcp_server(&rec).unwrap();
        let err = unsync_mcp_from_agent_internal(&h.store, &rec.id, "opencode", None, &h.resolver)
            .unwrap_err();
        assert_eq!(err.kind, crate::core::error::ErrorKind::NotFound);
    }

    #[test]
    fn upgrade_plan_derives_commands_from_the_stored_record() {
        let h = harness();
        let mut rec = record("up", "node", &["/n/globals/3/node_modules/@o/up/index.js"]);
        rec.source = json!({"kind": "npm_global", "package": "@o/up"});
        h.store.insert_mcp_server(&rec).unwrap();

        let plan = get_mcp_upgrade_plan_internal(&h.store, &rec.id).unwrap();
        assert_eq!(plan.commands, vec!["npm i -g '@o/up@latest'".to_string()]);
        assert_eq!(plan.latest_version, None);

        let mut none = record("plain", "true", &[]);
        none.source = json!({"kind": "none"});
        h.store.insert_mcp_server(&none).unwrap();
        let err = get_mcp_upgrade_plan_internal(&h.store, &none.id).unwrap_err();
        assert_eq!(err.kind, crate::core::error::ErrorKind::InvalidInput);
    }
}
