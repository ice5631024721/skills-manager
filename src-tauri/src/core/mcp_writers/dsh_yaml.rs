//! Surgical writer for DeepSeek Harness's `~/.dsh/cordis.patch.yml`
//! (ADR-0006 §4 clause 4: block-level surgery, never a wholesale rewrite).
//!
//! File shape (as read by [`crate::core::mcp_inventory::parse_dsh_config`]):
//! a top-level YAML list of `- insert:` plugin blocks. Plugins named
//! [`DSH_MCP_PLUGIN`] register MCP servers; comment lines and unrelated
//! plugin blocks (hooks, providers, …) interleave and must survive
//! byte-identically.
//!
//! Block model: a block starts at a column-0 line whose text begins
//! `- insert:` and runs to the next column-0 `- ` line (any top-level list
//! item, insert or not) or EOF. Within that raw range, trailing blank lines
//! and column-0 comments are the block's *tail*: replacement only rewrites
//! the content (so the blank-line structure around a block is preserved
//! automatically, e.g. "if the original block was followed by a blank line,
//! keep one"), and whole-block deletion eats exactly one such trailing blank
//! line. A block is attributed to a server by parsing its slice with the
//! same precedence `parse_dsh_config` uses (config `serverName`, falling
//! back to the plugin `id`).
//!
//! Accepted granularity: surgery is **block-level**. When one block hosts
//! several dsh-mcp-client plugins, upsert/remove rebuilds the whole block
//! from the parsed [`McpEntryDef`]s with the target replaced/removed, so the
//! co-resident plugins survive (their rendering normalizes to this writer's
//! template — config keys outside `McpEntryDef`, like `toolCallTimeoutMs`,
//! only normalize away for the *touched* block; untouched blocks stay
//! byte-identical). A block that mixes MCP-client plugins with foreign
//! plugins cannot be rebuilt without silently dropping the foreign plugin,
//! so it is refused with [`AgentWriteError::Unsafe`] — as is a target
//! declared by more than one block (ambiguous which to edit).
//!
//! Every operation first parses the whole text (invalid YAML or a non-list
//! root → `Unsafe`), and every produced text is re-parsed through
//! `parse_dsh_config` whose entry-name multiset must equal the expected set
//! — target present exactly once after upsert / gone after remove, all other
//! names unchanged (validate-inside-writer). A broken splice therefore
//! never reaches the caller, let alone the filesystem (ADR-0006 §4).

use std::collections::BTreeMap;

use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};

use super::{AgentWriteError, McpEntryDef, McpWriter};
use crate::core::mcp_inventory::{normalize_transport, parse_dsh_config, DSH_MCP_PLUGIN};

/// Writer for the DeepSeek Harness agent key (`writer_for_agent`).
pub struct DshYamlWriter;

fn unsafe_error(message: impl Into<String>) -> AgentWriteError {
    AgentWriteError::Unsafe(message.into())
}

/// Single-quote a YAML scalar with `''` escaping (the style the existing
/// cordis.patch.yml entries use for values containing special characters).
fn yaml_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Plugin `id` for a server name: lowercased, non-alphanumeric runs
/// collapsed to a single `-` (e.g. `zvec_grep` → `zvec-grep`).
fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash {
                slug.push('-');
            }
            slug.extend(ch.to_lowercase());
            pending_dash = false;
        } else if !slug.is_empty() {
            // Leading separators are dropped; interior runs become one dash.
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        "mcp".to_string()
    } else {
        slug
    }
}

/// Byte spans of one top-level `- insert:` block in the raw file text.
struct DshBlock {
    /// Offset of the `- insert:` line.
    start: usize,
    /// Offset just past the block's last content line (its `\n` included);
    /// `[content_end, raw_end)` are the trailing blank/comment lines.
    content_end: usize,
    /// The dsh-mcp-client plugins this block defines, in file order.
    defs: Vec<McpEntryDef>,
    /// True when the block also holds plugins the writer does not model —
    /// rebuilding it would lose them, so any edit targeting it is refused.
    foreign: bool,
}

impl DshYamlWriter {
    /// Strict gate shared by every operation: the whole text must parse and
    /// the root must be the plugin list. Returns the full entry definitions
    /// (serverName-or-id precedence, exactly `parse_dsh_config`'s).
    fn parse_defs(&self, file_text: &str) -> Result<Vec<McpEntryDef>, AgentWriteError> {
        let root: YamlValue = serde_yaml::from_str(file_text).map_err(|err| {
            unsafe_error(format!(
                "cordis.patch.yml is not valid YAML (refusing to edit it): {err}"
            ))
        })?;
        let Some(patches) = root.as_sequence() else {
            return Err(unsafe_error(
                "cordis.patch.yml top level is not a plugin list (refusing to edit it)",
            ));
        };
        Ok(defs_from_patches(patches))
    }

    /// Locate every `- insert:` block and parse its plugin list. A block
    /// slice that fails to parse on its own is marked foreign (never rebuilt)
    /// rather than crashing the operation — it simply cannot be attributed.
    fn find_blocks(&self, file_text: &str) -> Vec<DshBlock> {
        let lines = line_ranges(file_text);
        // CRLF files keep '\r' AND '\n' inside line ranges (line_ranges
        // includes the newline); trim both or a bare-dash top item slices as
        // "-\r\n" and escapes block-end detection, letting a block swallow
        // the following patch on Windows-authored files.
        let item_of = |i: usize| {
            let (s, e) = lines[i];
            file_text[s..e].trim_end_matches(['\r', '\n'])
        };
        let is_top_item = |i: usize| {
            let t = item_of(i);
            t.starts_with("- ") || t.starts_with("-\n") || t == "-"
        };
        let is_insert = |i: usize| item_of(i).starts_with("- insert:");

        let mut blocks = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if !is_insert(i) {
                i += 1;
                continue;
            }
            let start = lines[i].0;
            // Raw end: the next column-0 list item line, whatever it is,
            // or EOF. Continuation lines of this block are indented, so
            // column-0 detection cannot split it.
            let mut j = i + 1;
            while j < lines.len() && !is_top_item(j) {
                j += 1;
            }
            let raw_end = if j < lines.len() { lines[j].0 } else { file_text.len() };
            // Trim trailing blank/comment lines out of the content span.
            let mut content_end = raw_end;
            for k in (i + 1..j).rev() {
                let (ls, le) = lines[k];
                let body = file_text[ls..le].trim_end();
                if body.is_empty() || body.starts_with('#') {
                    content_end = ls;
                } else {
                    break;
                }
            }
            let (defs, foreign) = match serde_yaml::from_str::<YamlValue>(&file_text[start..raw_end]) {
                Ok(root) => match root.as_sequence() {
                    Some(patches) => (defs_from_patches(patches), has_foreign_plugin(patches)),
                    None => (Vec::new(), true),
                },
                // Unattributable slice: keep it out of reach of surgery.
                Err(_) => (Vec::new(), true),
            };
            blocks.push(DshBlock {
                start,
                content_end,
                defs,
                foreign,
            });
            i = j;
        }
        blocks
    }

    /// Multiset of server names `parse_dsh_config` reports for `text`;
    /// `Err` when the text is no longer a parseable plugin list.
    fn name_set(&self, text: &str) -> Result<BTreeMap<String, usize>, AgentWriteError> {
        let entries = parse_dsh_config(text)
            .map_err(|err| unsafe_error(format!("cordis.patch.yml failed validation after surgery: {err}")))?;
        let mut counts = BTreeMap::new();
        for entry in &entries {
            *counts.entry(entry.name.clone()).or_insert(0) += 1;
        }
        Ok(counts)
    }

    /// Validate-inside-writer: the full new text must re-parse through
    /// `parse_dsh_config` and its entry-name multiset must equal `expected`.
    fn validate(&self, new_text: &str, expected: &BTreeMap<String, usize>) -> Result<(), AgentWriteError> {
        let got = match self.name_set(new_text) {
            Ok(counts) => counts,
            Err(err) => {
                // Deleting the very last plugin block can legitimately
                // leave an empty or comment-only file: that parses to null
                // rather than a list but holds exactly zero servers.
                let leftover_blocks = new_text.lines().any(|line| line.starts_with("- "));
                if expected.is_empty() && !leftover_blocks {
                    BTreeMap::new()
                } else {
                    return Err(err);
                }
            }
        };
        if &got != expected {
            return Err(unsafe_error(format!(
                "internal error: cordis.patch.yml surgery touched the wrong entries (expected {expected:?}, got {got:?})"
            )));
        }
        Ok(())
    }
}

/// `(start, end)` of every line, `end` just past the `\n` (or EOF). A
/// trailing `\n` does not produce an extra empty line.
fn line_ranges(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            ranges.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        ranges.push((start, bytes.len()));
    }
    ranges
}

/// Walk a patch list (`- insert:` items) collecting every dsh-mcp-client
/// plugin as an [`McpEntryDef`].
fn defs_from_patches(patches: &[YamlValue]) -> Vec<McpEntryDef> {
    let mut defs = Vec::new();
    for plugin in plugins_of_patches(patches) {
        if let Some(def) = def_from_plugin(plugin) {
            defs.push(def);
        }
    }
    defs
}

/// True when any plugin in the patch list is not a dsh-mcp-client (or has
/// no name at all) — the block is not purely ours to rebuild.
fn has_foreign_plugin(patches: &[YamlValue]) -> bool {
    plugins_of_patches(patches).into_iter().any(|plugin| {
        match plugin
            .as_mapping()
            .and_then(|m| str_field(m, "name"))
        {
            Some(name) => name != DSH_MCP_PLUGIN,
            None => true,
        }
    })
}

/// All plugin values under each patch item's `insert:` list, in file order.
fn plugins_of_patches(patches: &[YamlValue]) -> Vec<&YamlValue> {
    let mut plugins = Vec::new();
    for patch in patches {
        let Some(inserts) = patch
            .as_mapping()
            .and_then(|m| m.get(YamlValue::String("insert".to_string())))
            .and_then(|value| value.as_sequence())
        else {
            continue;
        };
        plugins.extend(inserts.iter());
    }
    plugins
}

fn str_field(map: &YamlMapping, key: &str) -> Option<String> {
    map.get(YamlValue::String(key.to_string()))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Env values are strings in practice; if a hand-written file stores a
/// non-string scalar, keep its debug form rather than losing the key.
fn yaml_string_or_debug(value: &YamlValue) -> String {
    match value.as_str() {
        Some(text) => text.to_string(),
        None => format!("{value:?}"),
    }
}

fn str_seq_field(map: &YamlMapping, key: &str) -> Vec<String> {
    map.get(YamlValue::String(key.to_string()))
        .and_then(|value| value.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One dsh-mcp-client plugin map → full entry definition. Mirrors
/// `parse_dsh_config`'s precedence (serverName, falling back to the plugin
/// id; skipped without config) but keeps command and args **separate** and
/// env **values** in full — this feeds drift checks and takeover, unlike
/// the inventory's merged display string.
fn def_from_plugin(plugin: &YamlValue) -> Option<McpEntryDef> {
    let plugin = plugin.as_mapping()?;
    if str_field(plugin, "name").as_deref() != Some(DSH_MCP_PLUGIN) {
        return None;
    }
    let entry_id = str_field(plugin, "id");
    let config = plugin
        .get(YamlValue::String("config".to_string()))?
        .as_mapping()?;
    let name = str_field(config, "serverName").or(entry_id)?;
    let command = str_field(config, "command");
    let args = str_seq_field(config, "args");
    let url = str_field(config, "url");
    let env = config
        .get(YamlValue::String("env".to_string()))
        .and_then(|value| value.as_mapping())
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    Some((key.as_str()?.to_string(), yaml_string_or_debug(value)))
                })
                .collect()
        })
        .unwrap_or_default();

    Some(McpEntryDef {
        name,
        transport: normalize_transport(
            str_field(config, "transport").as_deref(),
            url.is_some(),
            command.is_some() || !args.is_empty(),
        ),
        command,
        args,
        url,
        env,
    })
}

/// One dsh-mcp-client plugin list item, 4-space nested style matching the
/// existing cordis.patch.yml entries. Never emits agent-native `enabled`
/// semantics (ADR-0005 clause 9).
fn render_plugin_item(entry: &McpEntryDef) -> String {
    let mut s = format!("    - id: {}\n", yaml_quote(&slugify(&entry.name)));
    s += &format!("      name: {}\n", yaml_quote(DSH_MCP_PLUGIN));
    s += "      config:\n";
    if entry.transport == "stdio" {
        s += "        transport: stdio\n";
        s += &format!("        serverName: {}\n", yaml_quote(&entry.name));
        if let Some(command) = &entry.command {
            s += &format!("        command: {}\n", yaml_quote(command));
        }
        if !entry.args.is_empty() {
            let args: Vec<String> = entry.args.iter().map(|a| yaml_quote(a)).collect();
            s += &format!("        args: [{}]\n", args.join(", "));
        }
    } else {
        // "streamable-http" / "http": quoted like every other scalar we
        // emit — an unquoted transport was a YAML injection vector
        // ("streamable-http\n- insert:" plants a column-0 block opener).
        s += &format!("        transport: {}\n", yaml_quote(&entry.transport));
        s += &format!("        serverName: {}\n", yaml_quote(&entry.name));
        s += &format!("        url: {}\n", yaml_quote(&entry.url.clone().unwrap_or_default()));
    }
    if !entry.env.is_empty() {
        s += "        env:\n";
        for (key, value) in &entry.env {
            s += &format!("          {}: {}\n", yaml_quote(key), yaml_quote(value));
        }
    }
    s
}

/// A complete single-purpose `- insert:` block for `entries` (1 = the normal
/// case; > 1 only when rebuilding a co-resident block).
fn render_block(entries: &[McpEntryDef]) -> String {
    let mut s = String::from("- insert:\n");
    for entry in entries {
        s += &render_plugin_item(entry);
    }
    s
}

impl McpWriter for DshYamlWriter {
    fn upsert(&self, file_text: &str, entry: &McpEntryDef) -> Result<String, AgentWriteError> {
        let old_defs = self.parse_defs(file_text)?;

        // Expected multiset after surgery: target exactly once, everything
        // else with its old multiplicity.
        let mut expected: BTreeMap<String, usize> = BTreeMap::new();
        for def in old_defs.iter().filter(|d| d.name != entry.name) {
            *expected.entry(def.name.clone()).or_insert(0) += 1;
        }
        expected.insert(entry.name.clone(), 1);

        let blocks = self.find_blocks(file_text);
        let hits: Vec<&DshBlock> = blocks
            .iter()
            .filter(|b| b.defs.iter().any(|d| d.name == entry.name))
            .collect();
        let new_text = match hits.as_slice() {
            // Not found: append a fresh block at EOF, one blank line of
            // separation; every preceding byte (comments included) is kept.
            [] => {
                let mut text = file_text.to_string();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                format!("{text}\n{}", render_block(std::slice::from_ref(entry)))
            }
            [block] => {
                if block.foreign {
                    return Err(unsafe_error(format!(
                        "server `{}` lives in a block with mixed foreign plugins; refusing block surgery",
                        entry.name
                    )));
                }
                // Block-level rebuild: replace the target (first occurrence,
                // later duplicates dropped) and re-render the co-resident
                // plugins from their parsed defs so they survive.
                let mut rebuilt: Vec<McpEntryDef> = Vec::new();
                let mut replaced = false;
                for def in &block.defs {
                    if def.name == entry.name {
                        if !replaced {
                            rebuilt.push(entry.clone());
                            replaced = true;
                        }
                    } else {
                        rebuilt.push(def.clone());
                    }
                }
                if !replaced {
                    rebuilt.push(entry.clone());
                }
                format!(
                    "{}{}{}",
                    &file_text[..block.start],
                    render_block(&rebuilt),
                    &file_text[block.content_end..]
                )
            }
            many => {
                return Err(unsafe_error(format!(
                    "server `{}` is declared by {} separate `- insert:` blocks; refusing ambiguous surgery",
                    entry.name,
                    many.len()
                )))
            }
        };

        self.validate(&new_text, &expected)?;
        Ok(new_text)
    }

    fn remove(&self, file_text: &str, name: &str) -> Result<String, AgentWriteError> {
        let old_defs = self.parse_defs(file_text)?;
        if !old_defs.iter().any(|def| def.name == name) {
            // Idempotent: absent server (or non-MCP plugin name) leaves the
            // text byte-identical.
            return Ok(file_text.to_string());
        }

        let expected: BTreeMap<String, usize> = old_defs
            .iter()
            .filter(|def| def.name != name)
            .fold(BTreeMap::new(), |mut acc, def| {
                *acc.entry(def.name.clone()).or_insert(0) += 1;
                acc
            });

        let blocks = self.find_blocks(file_text);
        let hits: Vec<&DshBlock> = blocks
            .iter()
            .filter(|b| b.defs.iter().any(|d| d.name == name))
            .collect();
        let [block] = hits.as_slice() else {
            return Err(unsafe_error(format!(
                "server `{name}` is declared by {} separate `- insert:` blocks; refusing ambiguous surgery",
                hits.len()
            )));
        };
        if block.foreign {
            return Err(unsafe_error(format!(
                "server `{name}` lives in a block with mixed foreign plugins; refusing block surgery"
            )));
        }

        let remaining: Vec<McpEntryDef> = block
            .defs
            .iter()
            .filter(|def| def.name != name)
            .cloned()
            .collect();
        let cut_end = if remaining.is_empty() {
            // Whole block goes away, plus one trailing blank line so block
            // separation does not drift.
            if file_text.as_bytes().get(block.content_end) == Some(&b'\n') {
                block.content_end + 1
            } else {
                block.content_end
            }
        } else {
            // Co-resident plugins survive: rebuild the block without `name`.
            block.content_end
        };
        let replacement = if remaining.is_empty() {
            String::new()
        } else {
            render_block(&remaining)
        };
        let new_text = format!(
            "{}{}{}",
            &file_text[..block.start],
            replacement,
            &file_text[cut_end..]
        );

        self.validate(&new_text, &expected)?;
        Ok(new_text)
    }

    fn read_entry(&self, file_text: &str, name: &str) -> Result<Option<McpEntryDef>, AgentWriteError> {
        Ok(self
            .parse_defs(file_text)?
            .into_iter()
            .find(|def| def.name == name))
    }

    fn empty_doc(&self) -> &'static str {
        "[]"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp_inventory::parse_dsh_config;

    /// DSH_FIXTURE from `mcp_inventory` (the real read-path fixture) extended
    /// with a leading header comment and a trailing comment; both must
    /// survive every operation verbatim, exactly like the interleaved
    /// non-MCP `dsh-hooks-claude-code` block.
    const FIXTURE: &str = r#"# skills-manager manages this file — keep header comment verbatim
# zvec-grep comments must not break parsing
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
# trailing comment: keep me verbatim
"#;

    // Foreign regions of FIXTURE, used as byte-exactness anchors.
    const HEADER: &str = "# skills-manager manages this file — keep header comment verbatim\n# zvec-grep comments must not break parsing\n";
    const AGENTMEMORY_ORIG: &str = r#"- insert:
    - id: agentmemory
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: agentmemory
        command: npx
        args: ['-y', '@agentmemory/mcp']
        env:
          AGENTMEMORY_URL: http://localhost:3111
"#;
    const ZVEC_ORIG: &str = r#"- insert:
    - id: mcp-zvec-grep
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: streamable-http
        serverName: zvec_grep
        url: http://127.0.0.1:7999/mcp
        toolCallTimeoutMs: 60000
"#;
    const HOOKS_ORIG: &str = r#"- insert:
    - id: agentmemory-hooks
      name: '@deepseek-ai/dsh-hooks-claude-code'
      config:
        configPath: "/Users/me/.dsh/agentmemory.hooks.json"
"#;
    const TRAILING: &str = "# trailing comment: keep me verbatim\n";

    fn entry(
        name: &str,
        transport: &str,
        command: Option<&str>,
        args: &[&str],
        url: Option<&str>,
        env: &[(&str, &str)],
    ) -> McpEntryDef {
        McpEntryDef {
            name: name.to_string(),
            transport: transport.to_string(),
            command: command.map(str::to_string),
            args: args.iter().map(|a| a.to_string()).collect(),
            url: url.map(str::to_string),
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    /// The serverName set `parse_dsh_config` sees — the inventory's exact view
    /// (hooks/provider plugins filtered out, id fallback applied).
    fn names(text: &str) -> Vec<String> {
        let mut v: Vec<String> = parse_dsh_config(text)
            .expect("writer output must re-parse as a plugin list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn fixture_composition_guard() {
        // The byte-exactness consts below must really tile FIXTURE, or every
        // equality assertion in this module is meaningless.
        assert_eq!(
            FIXTURE,
            format!("{HEADER}{AGENTMEMORY_ORIG}\n{ZVEC_ORIG}\n{HOOKS_ORIG}{TRAILING}")
        );
        assert_eq!(names(FIXTURE), vec!["agentmemory", "zvec_grep"]);
    }

    #[test]
    fn upsert_appends_new_block_at_eof_preserving_all_preceding_bytes() {
        let new = entry(
            "agentx",
            "stdio",
            Some("node"),
            &["/opt/servers/agentx/index.js"],
            None,
            &[("API_KEY", "s3cr3t'quoted")],
        );
        let out = DshYamlWriter.upsert(FIXTURE, &new).unwrap();

        const RENDERED_AGENTX: &str = "- insert:
    - id: 'agentx'
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: 'agentx'
        command: 'node'
        args: ['/opt/servers/agentx/index.js']
        env:
          'API_KEY': 's3cr3t''quoted'
";
        // Append at EOF: the whole original file (comments, agentmemory,
        // zvec, hooks block) is a byte-identical prefix, separated from the
        // new block by one blank line.
        assert_eq!(out, format!("{FIXTURE}\n{RENDERED_AGENTX}"));
        assert_eq!(names(&out), vec!["agentmemory", "agentx", "zvec_grep"]);
    }

    #[test]
    fn upsert_existing_replaces_only_that_block_and_keeps_blank_structure() {
        let updated = entry(
            "agentmemory",
            "stdio",
            Some("npx"),
            &["-y", "@agentmemory/mcp"],
            None,
            &[("AGENTMEMORY_URL", "http://localhost:9999")],
        );
        let out = DshYamlWriter.upsert(FIXTURE, &updated).unwrap();

        const RENDERED_UPDATED: &str = "- insert:
    - id: 'agentmemory'
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: 'agentmemory'
        command: 'npx'
        args: ['-y', '@agentmemory/mcp']
        env:
          'AGENTMEMORY_URL': 'http://localhost:9999'
";
        // Only the agentmemory block changed; its trailing blank line and
        // every other region (header, zvec, hooks, trailing comment) are
        // byte-identical.
        assert_eq!(
            out,
            format!("{HEADER}{RENDERED_UPDATED}\n{ZVEC_ORIG}\n{HOOKS_ORIG}{TRAILING}")
        );
        assert!(!out.contains("http://localhost:3111"));
        assert_eq!(names(&out), vec!["agentmemory", "zvec_grep"]);
    }

    #[test]
    fn remove_deletes_block_plus_one_trailing_blank_line() {
        let out = DshYamlWriter.remove(FIXTURE, "agentmemory").unwrap();
        assert_eq!(out, format!("{HEADER}{ZVEC_ORIG}\n{HOOKS_ORIG}{TRAILING}"));
        // Equivalent formulation: the block and one following blank line are
        // simply gone; nothing else moved.
        assert_eq!(
            out,
            FIXTURE.replacen(&format!("{AGENTMEMORY_ORIG}\n"), "", 1)
        );
        assert_eq!(names(&out), vec!["zvec_grep"]);
        // The non-MCP hooks block survives untouched, comment and all.
        assert!(out.contains(HOOKS_ORIG));
        assert!(out.ends_with(TRAILING));

        let out2 = DshYamlWriter.remove(FIXTURE, "zvec_grep").unwrap();
        assert_eq!(
            out2,
            format!("{HEADER}{AGENTMEMORY_ORIG}\n{HOOKS_ORIG}{TRAILING}")
        );
        assert_eq!(names(&out2), vec!["agentmemory"]);
    }

    #[test]
    fn remove_absent_name_is_idempotent_ok() {
        assert_eq!(DshYamlWriter.remove(FIXTURE, "ghost").unwrap(), FIXTURE);
        // A non-MCP plugin name (present in the file but not a server from
        // the writer's point of view) counts as absent too.
        assert_eq!(
            DshYamlWriter.remove(FIXTURE, "agentmemory-hooks").unwrap(),
            FIXTURE
        );
    }

    #[test]
    /// CRLF-authored patch files: a bare-dash top-level item ("-\r\n") must
    /// still terminate the preceding block, or the block swallows the next
    /// patch and a co-resident rebuild drops it — the exact bug the first
    /// CRLF attempt claimed to fix while trimming only '\r' off a slice
    /// that ends in '\n' (review round 3 caught the no-op).
    #[test]
    fn crlf_bare_dash_items_still_end_blocks() {
        let crlf = "# header\r\n- insert:\r\n    - id: target\r\n      name: '@deepseek-ai/dsh-mcp-client'\r\n      config:\r\n        transport: stdio\r\n        serverName: target\r\n        command: old\r\n-\r\n- insert:\r\n    - id: keeper\r\n      name: '@deepseek-ai/dsh-hooks-claude-code'\r\n      config:\r\n        configPath: x\r\n";
        let out = DshYamlWriter
            .upsert(crlf, &entry("target", "stdio", Some("new"), &[], None, &[]))
            .unwrap();
        assert!(out.contains("keeper"), "next patch must survive: {out}");
        assert!(out.contains("configPath: x"), "keeper body intact: {out}");
        assert!(out.contains("command: 'new'"), "target replaced: {out}");
        assert!(!out.contains("command: old"), "old command gone: {out}");
    }

    #[test]
    fn read_entry_keeps_command_args_separate_and_env_values_full() {
        let am = DshYamlWriter
            .read_entry(FIXTURE, "agentmemory")
            .unwrap()
            .expect("present");
        assert_eq!(am.name, "agentmemory");
        assert_eq!(am.transport, "stdio");
        assert_eq!(am.command.as_deref(), Some("npx"));
        // NOT merged into a display string like the inventory's `command`.
        assert_eq!(am.args, ["-y", "@agentmemory/mcp"]);
        assert_eq!(am.url, None);
        assert_eq!(am.env.len(), 1);
        assert_eq!(
            am.env.get("AGENTMEMORY_URL").map(String::as_str),
            Some("http://localhost:3111")
        );

        let zvec = DshYamlWriter
            .read_entry(FIXTURE, "zvec_grep")
            .unwrap()
            .expect("present");
        assert_eq!(zvec.transport, "streamable-http");
        assert_eq!(zvec.url.as_deref(), Some("http://127.0.0.1:7999/mcp"));
        assert_eq!(zvec.command, None);
        assert!(zvec.args.is_empty());
        assert!(zvec.env.is_empty());
    }

    #[test]
    fn read_entry_name_falls_back_to_plugin_id_like_the_inventory() {
        // Mirrors parse_dsh_config precedence: serverName first, plugin id
        // second. This block carries no serverName at all.
        const ID_FALLBACK: &str = r#"- insert:
    - id: legacy-oauth
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        command: 'node'
        args: ['legacy.js']
"#;
        let legacy = DshYamlWriter
            .read_entry(ID_FALLBACK, "legacy-oauth")
            .unwrap()
            .expect("id fallback");
        assert_eq!(legacy.name, "legacy-oauth");
        assert_eq!(legacy.transport, "stdio");
        assert_eq!(legacy.command.as_deref(), Some("node"));
        assert_eq!(legacy.args, ["legacy.js"]);
        assert!(legacy.env.is_empty());

        // The hooks plugin is not an MCP client: absent, not an error.
        assert_eq!(DshYamlWriter.read_entry(FIXTURE, "agentmemory-hooks").unwrap(), None);
        assert_eq!(DshYamlWriter.read_entry(FIXTURE, "ghost").unwrap(), None);
    }

    #[test]
    fn render_then_read_roundtrips_stdio_env_and_streamable_http() {
        let quirky = entry(
            "My Server_1",
            "stdio",
            Some("docker"),
            &["run", "-e", "MSG=it's a 'test'"],
            None,
            &[("TOKEN", "a'b'c"), ("UNICODE", "多字节 ✓")],
        );
        let out = DshYamlWriter.upsert(FIXTURE, &quirky).unwrap();
        // id slug: lowercased, non-alnum runs collapse to a single dash.
        assert!(out.contains("    - id: 'my-server-1'"), "{out}");
        assert_eq!(
            DshYamlWriter.read_entry(&out, "My Server_1").unwrap(),
            Some(quirky)
        );

        let remote = entry(
            "Remote Srv",
            "streamable-http",
            None,
            &[],
            Some("https://example.com/mcp?k='v'"),
            &[],
        );
        let out2 = DshYamlWriter.upsert(FIXTURE, &remote).unwrap();
        // Transport is emitted verbatim (plain scalar), url is quoted.
        assert!(out2.contains("        transport: streamable-http\n"));
        assert!(out2.contains(r#"        url: 'https://example.com/mcp?k=''v'''"#));
        assert_eq!(
            DshYamlWriter.read_entry(&out2, "Remote Srv").unwrap(),
            Some(remote)
        );
    }

    #[test]
    fn non_list_or_unparseable_top_level_refuses_every_op() {
        let probe = entry("ghost", "stdio", Some("x"), &[], None, &[]);
        for src in ["mcp:\n  x: 1\n", "just some prose", "[]\n- insert:\n", ""] {
            let up = DshYamlWriter.upsert(src, &probe).unwrap_err();
            assert!(matches!(up, AgentWriteError::Unsafe(_)), "upsert {src:?}");
            let rm = DshYamlWriter.remove(src, "ghost").unwrap_err();
            assert!(matches!(rm, AgentWriteError::Unsafe(_)), "remove {src:?}");
            let rd = DshYamlWriter.read_entry(src, "ghost").unwrap_err();
            assert!(matches!(rd, AgentWriteError::Unsafe(_)), "read {src:?}");
        }
        let err = DshYamlWriter
            .upsert("mcp:\n  x: 1\n", &probe)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a plugin list"), "unclear refusal: {err}");
    }

    #[test]
    fn upsert_rebuilds_the_whole_block_when_plugins_are_co_resident() {
        // Accepted granularity is BLOCK-level surgery: a block holding two
        // dsh-mcp-client plugins is rewritten in full when either is the
        // target. To not lose the co-resident plugin, the writer re-renders
        // every MCP plugin of the block from its parsed McpEntryDef, with
        // the target replaced. (Config keys outside the McpEntryDef model —
        // e.g. toolCallTimeoutMs — normalize away for the touched block
        // only; blocks we do not touch stay byte-untouched.)
        const TWO_IN_ONE: &str = r#"- insert:
    - id: alpha
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: alpha
        command: 'a-bin'
    - id: beta
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: beta
        command: 'b-bin'
        env:
          BETA_KEY: 'b-val'
"#;
        const RENDERED_TWO: &str = "- insert:
    - id: 'alpha'
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: 'alpha'
        command: 'a2-bin'
    - id: 'beta'
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: 'beta'
        command: 'b-bin'
        env:
          'BETA_KEY': 'b-val'
";
        let out = DshYamlWriter
            .upsert(TWO_IN_ONE, &entry("alpha", "stdio", Some("a2-bin"), &[], None, &[]))
            .unwrap();
        assert_eq!(out, RENDERED_TWO);
        // The block was rebuilt in place, not appended after itself.
        assert_eq!(out.matches("- insert:").count(), 1);
        assert_eq!(names(&out), vec!["alpha", "beta"]);

        let beta = DshYamlWriter
            .read_entry(&out, "beta")
            .unwrap()
            .expect("co-resident plugin survives");
        assert_eq!(beta.command.as_deref(), Some("b-bin"));
        assert_eq!(beta.env.get("BETA_KEY").map(String::as_str), Some("b-val"));
        let alpha = DshYamlWriter.read_entry(&out, "alpha").unwrap().expect("updated");
        assert_eq!(alpha.command.as_deref(), Some("a2-bin"));
    }

    #[test]
    fn remove_from_shared_block_keeps_other_plugin_until_block_empties() {
        const TWO_IN_ONE: &str = r#"- insert:
    - id: alpha
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: alpha
        command: 'a-bin'
    - id: beta
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: beta
        command: 'b-bin'
        env:
          BETA_KEY: 'b-val'
"#;
        const RENDERED_ALPHA_ONLY: &str = "- insert:
    - id: 'alpha'
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: 'alpha'
        command: 'a-bin'
";
        const RENDERED_BETA_ONLY: &str = "- insert:
    - id: 'beta'
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: 'beta'
        command: 'b-bin'
        env:
          'BETA_KEY': 'b-val'
";
        let only_alpha = DshYamlWriter.remove(TWO_IN_ONE, "beta").unwrap();
        assert_eq!(only_alpha, RENDERED_ALPHA_ONLY);
        assert_eq!(names(&only_alpha), vec!["alpha"]);

        let only_beta = DshYamlWriter.remove(TWO_IN_ONE, "alpha").unwrap();
        assert_eq!(only_beta, RENDERED_BETA_ONLY);
        assert_eq!(names(&only_beta), vec!["beta"]);

        // Removing the last server of the block deletes the whole block —
        // an empty patch file is a legal outcome of pure deletion.
        assert_eq!(DshYamlWriter.remove(&only_alpha, "alpha").unwrap(), "");
    }

    #[test]
    fn mixed_blocks_with_foreign_plugins_are_refused() {
        // Block surgery can only rebuild plugins the writer understands.
        // A block mixing the target MCP plugin with a non-MCP plugin is
        // refused instead of silently dropping the foreign plugin.
        const MIXED: &str = r#"- insert:
    - id: solo
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: solo
        command: 'c'
    - id: hooks-inline
      name: '@deepseek-ai/dsh-hooks-claude-code'
      config:
        configPath: "/x.json"
"#;
        let up = DshYamlWriter
            .upsert(MIXED, &entry("solo", "stdio", Some("c2"), &[], None, &[]))
            .unwrap_err();
        assert!(matches!(up, AgentWriteError::Unsafe(_)), "{up:?}");
        assert!(up.to_string().contains("mixed"), "unclear refusal: {up}");
        let rm = DshYamlWriter.remove(MIXED, "solo").unwrap_err();
        assert!(matches!(rm, AgentWriteError::Unsafe(_)), "{rm:?}");
        // Reading is unaffected — it is a pure inventory walk.
        assert!(DshYamlWriter.read_entry(MIXED, "solo").unwrap().is_some());
    }

    #[test]
    fn target_declared_in_multiple_blocks_is_refused() {
        const DUP: &str = r#"- insert:
    - id: dup-a
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: dup
        command: 'a'

- insert:
    - id: dup-b
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        transport: stdio
        serverName: dup
        command: 'b'
"#;
        let up = DshYamlWriter
            .upsert(DUP, &entry("dup", "stdio", Some("x"), &[], None, &[]))
            .unwrap_err();
        assert!(matches!(up, AgentWriteError::Unsafe(_)), "{up:?}");
        assert!(up.to_string().contains("refusing"), "unclear refusal: {up}");
        let rm = DshYamlWriter.remove(DUP, "dup").unwrap_err();
        assert!(matches!(rm, AgentWriteError::Unsafe(_)), "{rm:?}");
    }

    #[test]
    fn writer_is_registered_for_deepseek_harness() {
        let writer = super::super::writer_for_agent("deepseek_harness").expect("registered");
        let out = writer
            .upsert(
                FIXTURE,
                &entry("agentx", "stdio", Some("node"), &["x.js"], None, &[]),
            )
            .unwrap();
        assert!(out.starts_with(FIXTURE));
        assert_eq!(names(&out), vec!["agentmemory", "agentx", "zvec_grep"]);
    }
}
