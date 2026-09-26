//! Surgical write support for agent MCP config files (ADR-0005 clauses 5/9,
//! ADR-0006 §4).
//!
//! Two layers share this module:
//!
//! * Here lives the common vocabulary — [`McpEntryDef`], the fingerprint used
//!   for drift detection, [`atomic_write_text`], and the agent→writer
//!   registry.
//! * Each `mcp_writers::<format>` module implements [`McpWriter`] as a **pure
//!   text→text transform** over one config format. Writers never touch the
//!   filesystem: resolving which file an agent's config lives in stays in the
//!   command layer (Task 7), which keeps every writer testable without
//!   touching $HOME. (Deviation from the plan sketch: the `config_path()`
//!   method was dropped for exactly that reason.)
//!
//! The write recipe enforced by callers is fixed by ADR-0006 §4: surgical
//! text edit → re-parse validate → [`atomic_write_text`], all under
//! `RepoLock::acquire_foreground`. A format that cannot be parsed or whose
//! target member cannot be located must refuse with
//! [`AgentWriteError::Unsafe`] — never rewrite the file wholesale.

pub mod dsh_yaml;
pub mod json_span;
pub mod opencode_json;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Agent-neutral view of one MCP server definition, in the transport
/// vocabulary normalized by `mcp_inventory` (`"stdio" | "http" |
/// "streamable-http"`). Writers render this into their format; readers hand
/// back **full env values** (unlike the inventory, which only ever exposes
/// env key names, this is the pre-write drift/takeover path).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpEntryDef {
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// Failure of a writer operation on an agent config text.
#[derive(Debug)]
pub enum AgentWriteError {
    /// The file text is unparseable (e.g. JSONC comments) or the target
    /// member cannot be located: the caller must refuse this agent
    /// (ADR-0006 §4), never fall back to a whole-file rewrite.
    Unsafe(String),
    /// I/O failure while replacing the file atomically.
    Io(String),
}

impl std::fmt::Display for AgentWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsafe(message) | Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AgentWriteError {}

impl From<std::io::Error> for AgentWriteError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

/// Stable sha256 hex fingerprint of an entry definition, taken over the
/// canonical serde_json encoding. `McpEntryDef` serializes its fields in
/// declaration order and `env` is a `BTreeMap`, so the byte stream is
/// already canonical: map insertion order and any source-file formatting
/// are irrelevant, and the digest flips iff any field changes.
pub fn entry_fingerprint(entry: &McpEntryDef) -> String {
    let bytes = serde_json::to_vec(entry).expect("McpEntryDef always serializes");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Replace `path` with `content` atomically: sibling tmp file with a uuid
/// suffix → write_all → sync_all → fs::rename → best-effort parent-dir sync.
/// Mirrors `sync_metadata::atomic_write_json` (sync_metadata.rs:602-647);
/// the content here is opaque text, so there is no canonical-JSON step.
pub fn atomic_write_text(path: &Path, content: &str) -> Result<(), AgentWriteError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::now_v7()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Flushing the parent directory makes the rename durable; a failure here
    // does not invalidate the already-visible rename, so it is best-effort.
    let _ = sync_parent_dir(path);
    Ok(())
}

fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Windows requires FILE_FLAG_BACKUP_SEMANTICS to open a directory
        // handle, and FlushFileBuffers needs write access (see
        // sync_metadata::sync_parent_dir for the full rationale).
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

        let dir = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(parent)?;
        dir.sync_all()?;
        Ok(())
    }

    #[cfg(not(windows))]
    {
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()?;
        Ok(())
    }
}

/// One surgical editor per agent config format. All three operations are
/// pure `text → text` (or `text → entry`) transforms; see the module doc for
/// why file resolution deliberately lives outside this trait.
pub trait McpWriter: Sync {
    /// Return the full new file text with `entry` present in the managed
    /// section, inserted or replacing an existing same-name entry. Must
    /// refuse with `Unsafe` when `file_text` is unparseable.
    fn upsert(&self, file_text: &str, entry: &McpEntryDef) -> Result<String, AgentWriteError>;
    /// Return the full new file text without the named entry. Removing an
    /// absent name is idempotent: `Ok` with the original text unchanged.
    fn remove(&self, file_text: &str, name: &str) -> Result<String, AgentWriteError>;
    /// Read the current stored definition of `name`, or `Ok(None)` when the
    /// file simply does not configure it. Env values are returned in full.
    fn read_entry(&self, file_text: &str, name: &str) -> Result<Option<McpEntryDef>, AgentWriteError>;
    /// A minimal, valid, entry-free document in this format. Scratch space
    /// for asking "what would this definition read back as here" (format
    /// normalization: opencode maps streamable-http→http, drops remote env).
    fn empty_doc(&self) -> &'static str;
}

/// Look up the surgical writer for an agent key (same keys `mcp_inventory`
/// reports). One arm per supported agent: adding a writer means adding a
/// module and flipping one line here; unknown agents get `None` and the
/// caller refuses with a clear error.
pub fn writer_for_agent(agent_key: &str) -> Option<&'static dyn McpWriter> {
    match agent_key {
        "opencode" => Some(&opencode_json::OpenCodeWriter),
        "deepseek_harness" => Some(&dsh_yaml::DshYamlWriter),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> McpEntryDef {
        McpEntryDef {
            name: "agentmemory".to_string(),
            transport: "stdio".to_string(),
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "@agentmemory/mcp".to_string()],
            url: None,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn fingerprint_ignores_env_insertion_order_and_whitespace() {
        let mut a = sample_entry();
        a.env.insert("Z".to_string(), "1".to_string());
        a.env.insert("A".to_string(), "2".to_string());

        let mut b = sample_entry();
        b.env.insert("A".to_string(), "2".to_string());
        b.env.insert("Z".to_string(), "1".to_string());

        assert_eq!(entry_fingerprint(&a), entry_fingerprint(&b));
        // The digest is hex sha256.
        assert_eq!(entry_fingerprint(&a).len(), 64);
        assert!(entry_fingerprint(&a).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_changes_for_every_field() {
        let base = entry_fingerprint(&sample_entry());

        let mut changed = sample_entry();
        changed.name = "other".to_string();
        assert_ne!(base, entry_fingerprint(&changed));

        let mut changed = sample_entry();
        changed.transport = "http".to_string();
        assert_ne!(base, entry_fingerprint(&changed));

        let mut changed = sample_entry();
        changed.command = Some("pnpx".to_string());
        assert_ne!(base, entry_fingerprint(&changed));

        let mut changed = sample_entry();
        changed.args.push("--flag".to_string());
        assert_ne!(base, entry_fingerprint(&changed));

        let mut changed = sample_entry();
        changed.url = Some("http://x/y".to_string());
        assert_ne!(base, entry_fingerprint(&changed));

        let mut changed = sample_entry();
        changed.env.insert("K".to_string(), "v".to_string());
        assert_ne!(base, entry_fingerprint(&changed));

        let mut changed = sample_entry();
        changed.env.insert("K".to_string(), "other".to_string());
        assert_ne!(base, entry_fingerprint(&changed));
    }

    #[test]
    fn atomic_write_text_roundtrip_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        // A nested target proves the parent-dir creation step.
        let path = dir.path().join("sub").join("agent.json");

        atomic_write_text(&path, "{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");

        // Overwriting works and still leaves exactly one file behind.
        atomic_write_text(&path, "second pass ✓").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second pass ✓");

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["agent.json".to_string()]);
    }

    #[test]
    fn writer_registry_resolves_registered_and_rejects_unknown_agents() {
        assert!(writer_for_agent("opencode").is_some());
        assert!(writer_for_agent("deepseek_harness").is_some());
        // Unknown keys must stay None so callers refuse instead of guessing
        // at a format for an agent we have not audited.
        assert!(writer_for_agent("nonexistent-agent").is_none());
        assert!(writer_for_agent("").is_none());
    }
}
