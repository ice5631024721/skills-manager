//! DB CRUD for the MCP definition library (`mcp_servers`) and the per-agent
//! deployment ledger (`mcp_bindings`) added by migration v10.
//!
//! Design of record: `docs/adr/0006`. These methods live on `SkillStore` (in
//! a sibling `impl` block reached through `SkillStore::conn`) rather than a
//! separate store struct because everything shares one SQLite file, one
//! migration runner, and one mutex-protected connection — a second pool would
//! only add lock-ordering hazards.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeMap;

use super::error::AppError;
use super::skill_store::SkillStore;

/// One Managed library entry (ADR-0006 §3: `name` is the identity).
///
/// `args`/`env`/`source` are typed here and JSON TEXT in the DB: sqlite has
/// no map/array type, and every consumer wants them whole anyway, so
/// (de)serializing once per row beats a side table per field. `env` is a
/// `BTreeMap` (not `HashMap`) so serialization is deterministic — two
/// otherwise-identical records produce byte-identical rows, which keeps
/// future fingerprint/diff logic honest.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpServerRecord {
    pub id: String,
    pub name: String,
    /// "stdio" | "http" | "streamable-http" (mcp_inventory's normalized set).
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Upstream source tag, e.g. `{"kind":"none"}` (see mcp_upstream).
    pub source: serde_json::Value,
    /// "unknown" | "up_to_date" | "update_available" | "error" — mirrors the
    /// skills update-check vocabulary so badges reuse one meaning.
    pub update_status: String,
    pub remote_version: Option<String>,
    pub last_checked_at: Option<i64>,
    pub last_check_error: Option<String>,
    /// "pending" | "ok" | "fail".
    pub probe_status: String,
    pub probe_message: Option<String>,
    pub probe_checked_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One (server, agent) deployment row: the entry exists in that agent's
/// config and `fingerprint` was true of it at `written_at` (drift basis).
#[derive(Debug, Clone)]
pub struct McpBindingRecord {
    pub id: String,
    pub server_id: String,
    pub agent_key: String,
    pub fingerprint: String,
    pub written_at: i64,
}

/// Column order shared by every SELECT so the mapper and the query strings
/// cannot drift apart silently.
const SERVER_COLUMNS: &str = "id, name, transport, command, args, url, env, source, \
     update_status, remote_version, last_checked_at, last_check_error, \
     probe_status, probe_message, probe_checked_at, created_at, updated_at";

const BINDING_COLUMNS: &str = "id, server_id, agent_key, fingerprint, written_at";

impl SkillStore {
    /// Insert a definition. The UNIQUE(name) constraint is the authoritative
    /// identity check (ADR-0006 §3); a violation is mapped to a checkable
    /// `AppError::invalid_input` so callers can both pre-check with
    /// `get_mcp_server_by_name` (nice message) and rely on this (race-safe).
    pub fn insert_mcp_server(&self, record: &McpServerRecord) -> Result<()> {
        let conn = self.conn();
        let args = to_json_text("mcp_servers.args", &record.args)?;
        let env = to_json_text("mcp_servers.env", &record.env)?;
        let source = to_json_text("mcp_servers.source", &record.source)?;
        let result = conn.execute(
            "INSERT INTO mcp_servers (
                id, name, transport, command, args, url, env, source,
                update_status, remote_version, last_checked_at, last_check_error,
                probe_status, probe_message, probe_checked_at, created_at, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                record.id,
                record.name,
                record.transport,
                record.command,
                args,
                record.url,
                env,
                source,
                record.update_status,
                record.remote_version,
                record.last_checked_at,
                record.last_check_error,
                record.probe_status,
                record.probe_message,
                record.probe_checked_at,
                record.created_at,
                record.updated_at,
            ],
        );
        result.map_err(|e| map_unique_name_error(e, &record.name))?;
        Ok(())
    }

    pub fn get_mcp_server_by_id(&self, id: &str) -> Result<Option<McpServerRecord>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                &format!("SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE id = ?1"),
                params![id],
                map_server_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Name lookup doubles as the pre-check for add/edit dialogs; `name` is
    /// UNIQUE, so at most one row can match.
    pub fn get_mcp_server_by_name(&self, name: &str) -> Result<Option<McpServerRecord>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                &format!("SELECT {SERVER_COLUMNS} FROM mcp_servers WHERE name = ?1"),
                params![name],
                map_server_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_all_mcp_servers(&self) -> Result<Vec<McpServerRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SERVER_COLUMNS} FROM mcp_servers ORDER BY name"
        ))?;
        // Unlike the skills list (where a bad row may be skipped), a corrupt
        // JSON column must surface: silently hiding a definition makes it look
        // deleted, and this table is small enough that strictness is free.
        let rows = stmt
            .query_map([], map_server_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Overwrite the mutable columns of a definition by id. `created_at` is
    /// owned by the insert and never moves; `updated_at` travels with the
    /// record so the caller controls the version stamp (Task 7 derives drift
    /// tokens from it).
    pub fn update_mcp_server(&self, record: &McpServerRecord) -> Result<()> {
        let conn = self.conn();
        let args = to_json_text("mcp_servers.args", &record.args)?;
        let env = to_json_text("mcp_servers.env", &record.env)?;
        let source = to_json_text("mcp_servers.source", &record.source)?;
        let changed = conn
            .execute(
                "UPDATE mcp_servers SET
                    name = ?2, transport = ?3, command = ?4, args = ?5, url = ?6,
                    env = ?7, source = ?8, update_status = ?9, remote_version = ?10,
                    last_checked_at = ?11, last_check_error = ?12, probe_status = ?13,
                    probe_message = ?14, probe_checked_at = ?15, updated_at = ?16
                 WHERE id = ?1",
                params![
                    record.id,
                    record.name,
                    record.transport,
                    record.command,
                    args,
                    record.url,
                    env,
                    source,
                    record.update_status,
                    record.remote_version,
                    record.last_checked_at,
                    record.last_check_error,
                    record.probe_status,
                    record.probe_message,
                    record.probe_checked_at,
                    record.updated_at,
                ],
            )
            .map_err(|e| map_unique_name_error(e, &record.name))?;
        if changed == 0 {
            return Err(AppError::not_found(format!(
                "MCP server {} not found",
                record.id
            ))
            .into());
        }
        Ok(())
    }

    /// Delete a definition. `ON DELETE CASCADE` drops its binding ledger; the
    /// agent config files are NOT touched here — callers must unsync/write
    /// them back first (ADR-0006 §4), which is why bindings are a ledger of
    /// what the manager has actually written, not a wish list.
    pub fn delete_mcp_server(&self, id: &str) -> Result<()> {
        let conn = self.conn();
        let changed = conn.execute("DELETE FROM mcp_servers WHERE id = ?1", params![id])?;
        if changed == 0 {
            return Err(AppError::not_found(format!("MCP server {id} not found")).into());
        }
        Ok(())
    }

    /// Record the result of an upstream version check. Bumps
    /// `last_checked_at` but NOT `updated_at`: a check result is metadata
    /// about the definition, not a change to it — bumping the version stamp
    /// here would churn every drift token derived from `updated_at`.
    pub fn set_mcp_check_state(
        &self,
        id: &str,
        update_status: &str,
        remote_version: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        let changed = conn.execute(
            "UPDATE mcp_servers
             SET update_status = ?2, remote_version = ?3, last_checked_at = ?4, last_check_error = ?5
             WHERE id = ?1",
            params![id, update_status, remote_version, now, error],
        )?;
        if changed == 0 {
            return Err(AppError::not_found(format!("MCP server {id} not found")).into());
        }
        Ok(())
    }

    /// Record the result of a liveness probe. Same `updated_at` rule as
    /// `set_mcp_check_state`.
    pub fn set_mcp_probe_state(&self, id: &str, status: &str, message: Option<&str>) -> Result<()> {
        let conn = self.conn();
        let now = chrono::Utc::now().timestamp();
        let changed = conn.execute(
            "UPDATE mcp_servers
             SET probe_status = ?2, probe_message = ?3, probe_checked_at = ?4
             WHERE id = ?1",
            params![id, status, message, now],
        )?;
        if changed == 0 {
            return Err(AppError::not_found(format!("MCP server {id} not found")).into());
        }
        Ok(())
    }

    /// Insert-or-replace keyed on (server_id, agent_key): the ledger holds the
    /// *current* deployment per agent, so a resync overwrites fingerprint/id
    /// in place (INSERT OR REPLACE is the plan's chosen vehicle; the conflict
    /// can only ever hit this table's own UNIQUE index).
    pub fn upsert_mcp_binding(&self, record: &McpBindingRecord) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR REPLACE INTO mcp_bindings
                 (id, server_id, agent_key, fingerprint, written_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.id,
                record.server_id,
                record.agent_key,
                record.fingerprint,
                record.written_at,
            ],
        )?;
        Ok(())
    }

    pub fn delete_mcp_binding(&self, server_id: &str, agent_key: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM mcp_bindings WHERE server_id = ?1 AND agent_key = ?2",
            params![server_id, agent_key],
        )?;
        Ok(())
    }

    pub fn get_mcp_bindings_for_server(&self, server_id: &str) -> Result<Vec<McpBindingRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {BINDING_COLUMNS} FROM mcp_bindings WHERE server_id = ?1 ORDER BY agent_key"
        ))?;
        let rows = stmt
            .query_map(params![server_id], map_binding_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Ordered deterministically (server, then agent) so callers can diff or
    /// render without re-sorting.
    pub fn get_all_mcp_bindings(&self) -> Result<Vec<McpBindingRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {BINDING_COLUMNS} FROM mcp_bindings ORDER BY server_id, agent_key"
        ))?;
        let rows = stmt
            .query_map([], map_binding_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Translate the DB-level identity guardrail into the checkable error the UI
/// branches on. Anything that is not *this table's* UNIQUE(name) violation
/// passes through untouched — swallowing other constraint errors (FK, NOT
/// NULL) would hide real bugs.
fn map_unique_name_error(e: rusqlite::Error, name: &str) -> anyhow::Error {
    let is_name_clash = matches!(
        &e,
        rusqlite::Error::SqliteFailure(f, Some(msg))
            if f.code == rusqlite::ErrorCode::ConstraintViolation && msg.contains("mcp_servers.name")
    );
    if is_name_clash {
        return AppError::invalid_input(format!(
            "MCP server name \"{name}\" already exists"
        ))
        .into();
    }
    e.into()
}

/// Serialize a typed field into its JSON TEXT column, tagging failures with
/// the column name (a bare serde error would not say which row field broke).
fn to_json_text<T: serde::Serialize>(col: &str, value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| json_column_error(col, &e))
}

fn from_json_text<T: serde::de::DeserializeOwned>(col: &str, text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|e| json_column_error(col, &e))
}

/// rusqlite has no free-form error variant; `FromSqlConversionFailure` is the
/// designated carrier for "this TEXT is not the type the column promises".
/// The index is a placeholder (we raise it ourselves, not from a row read).
fn json_column_error(col: &str, source: &dyn std::error::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(format!("column {col}: {source}"))),
    )
}

fn map_server_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpServerRecord> {
    // Positional: indices must match SERVER_COLUMNS above.
    Ok(McpServerRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        transport: row.get(2)?,
        command: row.get(3)?,
        args: from_json_text("mcp_servers.args", &row.get::<_, String>(4)?)?,
        url: row.get(5)?,
        // env/source default to valid JSON in the schema, so a parse failure
        // here means genuine corruption — let it propagate (see
        // get_all_mcp_servers).
        env: from_json_text("mcp_servers.env", &row.get::<_, String>(6)?)?,
        source: from_json_text("mcp_servers.source", &row.get::<_, String>(7)?)?,
        update_status: row.get(8)?,
        remote_version: row.get(9)?,
        last_checked_at: row.get(10)?,
        last_check_error: row.get(11)?,
        probe_status: row.get(12)?,
        probe_message: row.get(13)?,
        probe_checked_at: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn map_binding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<McpBindingRecord> {
    Ok(McpBindingRecord {
        id: row.get(0)?,
        server_id: row.get(1)?,
        agent_key: row.get(2)?,
        fingerprint: row.get(3)?,
        written_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::ErrorKind;
    use serde_json::json;
    use tempfile::tempdir;

    fn temp_db_dir() -> tempfile::TempDir {
        tempdir().unwrap()
    }

    fn sample_server(id: &str, name: &str) -> McpServerRecord {
        McpServerRecord {
            id: id.to_string(),
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "@agentmemory/mcp".to_string()],
            url: None,
            env: BTreeMap::from([
                ("ZEBRA".to_string(), "1".to_string()),
                ("alpha".to_string(), "2".to_string()),
            ]),
            source: json!({"kind": "npx", "package": "@agentmemory/mcp"}),
            update_status: "unknown".to_string(),
            remote_version: None,
            last_checked_at: None,
            last_check_error: None,
            probe_status: "pending".to_string(),
            probe_message: None,
            probe_checked_at: None,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn sample_binding(server_id: &str, agent_key: &str, fp: &str) -> McpBindingRecord {
        McpBindingRecord {
            id: uuid::Uuid::new_v4().to_string(),
            server_id: server_id.to_string(),
            agent_key: agent_key.to_string(),
            fingerprint: fp.to_string(),
            written_at: 200,
        }
    }

    /// args/env/source must survive the JSON TEXT round trip with their typed
    /// shapes, and the read APIs (by id / by name / sorted list) must agree.
    #[test]
    fn insert_server_round_trips_json_fields() {
        let tmp = temp_db_dir();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();
        let record = sample_server("s1", "agentmemory");

        store.insert_mcp_server(&record).unwrap();

        let loaded = store.get_mcp_server_by_id("s1").unwrap().unwrap();
        assert_eq!(loaded, record, "typed fields must survive the JSON TEXT round trip");

        let by_name = store.get_mcp_server_by_name("agentmemory").unwrap().unwrap();
        assert_eq!(by_name.id, "s1");

        // A second server that sorts before the first proves ORDER BY name.
        store.insert_mcp_server(&sample_server("s2", "aaa-zvec")).unwrap();
        let all = store.get_all_mcp_servers().unwrap();
        assert_eq!(
            all.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["aaa-zvec", "agentmemory"]
        );
    }

    /// Name is the library identity (ADR-0006 §3): a duplicate insert is
    /// rejected with a *checkable* error, not a raw sqlite message dump.
    #[test]
    fn duplicate_name_is_rejected() {
        let tmp = temp_db_dir();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();
        store.insert_mcp_server(&sample_server("s1", "dup")).unwrap();

        let err = store
            .insert_mcp_server(&sample_server("s2", "dup"))
            .unwrap_err();
        let app_err = err.downcast_ref::<AppError>().expect("must be an AppError");
        assert_eq!(app_err.kind, ErrorKind::InvalidInput);
        assert!(app_err.message.contains("dup"), "message names the clash: {}", app_err.message);

        // The first row is untouched by the failed insert.
        assert_eq!(store.get_all_mcp_servers().unwrap().len(), 1);
    }

    /// Re-binding the same (server, agent) pair replaces the fingerprint in
    /// place — it is a ledger of current deployments, not history.
    #[test]
    fn binding_upsert_replaces_fingerprint() {
        let tmp = temp_db_dir();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();
        store.insert_mcp_server(&sample_server("s1", "mem")).unwrap();

        store
            .upsert_mcp_binding(&sample_binding("s1", "opencode", "fp-v1"))
            .unwrap();
        // Same (server, agent) pair, different id and fingerprint.
        store
            .upsert_mcp_binding(&sample_binding("s1", "opencode", "fp-v2"))
            .unwrap();
        // A different agent for the same server must not be collapsed.
        store
            .upsert_mcp_binding(&sample_binding("s1", "deepseek_harness", "fp-x"))
            .unwrap();

        let bindings = store.get_mcp_bindings_for_server("s1").unwrap();
        assert_eq!(bindings.len(), 2, "UNIQUE(server_id, agent_key) upserts per agent");
        let oc = bindings
            .iter()
            .find(|b| b.agent_key == "opencode")
            .expect("opencode binding");
        assert_eq!(oc.fingerprint, "fp-v2", "re-upsert replaced the fingerprint");
        assert_eq!(store.get_all_mcp_bindings().unwrap().len(), 2);

        // Deleting one agent's row leaves the other.
        store.delete_mcp_binding("s1", "opencode").unwrap();
        let rest = store.get_mcp_bindings_for_server("s1").unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].agent_key, "deepseek_harness");
    }

    /// Deleting a definition drops its binding ledger via ON DELETE CASCADE
    /// (the agent config files themselves are cleaned by the writers before
    /// this call — the DB never claims a write it did not do).
    #[test]
    fn delete_server_cascades_bindings() {
        let tmp = temp_db_dir();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();
        store.insert_mcp_server(&sample_server("s1", "gone")).unwrap();
        store.insert_mcp_server(&sample_server("s2", "stays")).unwrap();
        store
            .upsert_mcp_binding(&sample_binding("s1", "opencode", "fp1"))
            .unwrap();
        store
            .upsert_mcp_binding(&sample_binding("s1", "deepseek_harness", "fp1"))
            .unwrap();
        store
            .upsert_mcp_binding(&sample_binding("s2", "opencode", "fp2"))
            .unwrap();

        store.delete_mcp_server("s1").unwrap();

        assert!(store.get_mcp_server_by_id("s1").unwrap().is_none());
        assert!(store.get_mcp_bindings_for_server("s1").unwrap().is_empty());
        let all = store.get_all_mcp_bindings().unwrap();
        assert_eq!(all.len(), 1, "only the surviving server's binding remains");
        assert_eq!(all[0].server_id, "s2");
    }

    /// Check-state and probe-state updates persist, and `None` arguments
    /// *clear* the column (a stale error/badge must not haunt a passing
    /// re-check — matches the skills update-check semantics).
    #[test]
    fn set_check_state_and_probe_state_persist() {
        let tmp = temp_db_dir();
        let store = SkillStore::new(&tmp.path().join("test.db")).unwrap();
        store.insert_mcp_server(&sample_server("s1", "mem")).unwrap();

        store
            .set_mcp_check_state("s1", "update_available", Some("0.9.30"), None)
            .unwrap();
        let r = store.get_mcp_server_by_id("s1").unwrap().unwrap();
        assert_eq!(r.update_status, "update_available");
        assert_eq!(r.remote_version.as_deref(), Some("0.9.30"));
        assert!(r.last_checked_at.is_some(), "a check just happened; stamp it");

        store
            .set_mcp_check_state("s1", "error", None, Some("registry timeout"))
            .unwrap();
        let r = store.get_mcp_server_by_id("s1").unwrap().unwrap();
        assert_eq!(r.update_status, "error");
        assert!(
            r.remote_version.is_none(),
            "None clears: no stale version from the previous check"
        );
        assert_eq!(r.last_check_error.as_deref(), Some("registry timeout"));

        store.set_mcp_check_state("s1", "up_to_date", Some("0.9.29"), None).unwrap();
        let r = store.get_mcp_server_by_id("s1").unwrap().unwrap();
        assert_eq!(r.update_status, "up_to_date");
        assert!(
            r.last_check_error.is_none(),
            "None clears: the old error must not haunt a passing check"
        );

        store.set_mcp_probe_state("s1", "fail", Some("handshake timeout")).unwrap();
        let r = store.get_mcp_server_by_id("s1").unwrap().unwrap();
        assert_eq!(r.probe_status, "fail");
        assert_eq!(r.probe_message.as_deref(), Some("handshake timeout"));
        assert!(r.probe_checked_at.is_some());

        store.set_mcp_probe_state("s1", "ok", None).unwrap();
        let r = store.get_mcp_server_by_id("s1").unwrap().unwrap();
        assert_eq!(r.probe_status, "ok");
        assert!(r.probe_message.is_none(), "None clears the old failure message");
    }
}
