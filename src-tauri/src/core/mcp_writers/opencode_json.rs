//! Surgical writer for OpenCode's `opencode.json` — the top-level `mcp`
//! object of an otherwise unrelated config file (ADR-0005 clause 5: never
//! rewrite the whole file, `provider`/`$schema`/unknown keys stay byte-exact).
//!
//! Format facts, mirroring the read path in [`crate::core::mcp_inventory`]:
//!
//! * Entries live in `"mcp": { <name>: { … } }`.
//! * `type:"local"` carries a merged `command` array (program + args) and an
//!   optional `environment` object; `type:"remote"` carries `url`. The
//!   transport vocabulary is `mcp_inventory::normalize_transport`, so remote
//!   entries read back as "http" (a "streamable-http" definition therefore
//!   renders as remote and reads back as "http" — opencode has no separate
//!   label, the lossy roundtrip is accepted deliberately).
//! * The native `enabled` flag is never emitted (ADR-0005 clause 9): sync
//!   state is entry presence alone.
//!
//! Every operation strict-parses the input first: JSONC (comments, trailing
//! commas) is refused with [`AgentWriteError::Unsafe`] instead of being
//! silently normalized away, which would destroy user comments and formatting
//! (ADR-0006 §4). Every produced text is re-parsed and asserted to carry the
//! expected member state before it is returned (validate-inside-writer), so a
//! broken splice can never reach the filesystem.

use serde_json::Value as JsonValue;

use super::json_span;
use super::{AgentWriteError, McpEntryDef, McpWriter};
use crate::core::mcp_inventory::normalize_transport;

/// Writer for the OpenCode agent key (`mcp_writers::writer_for_agent`).
/// A minimal valid opencode.json with an empty managed section — the seed
/// for a missing file and the scratch for render-normalization questions.
pub const EMPTY_DOC: &str = "{\n  \"mcp\": {}\n}";

pub struct OpenCodeWriter;

fn unsafe_error(message: impl Into<String>) -> AgentWriteError {
    AgentWriteError::Unsafe(message.into())
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).expect("strings always serialize")
}

impl OpenCodeWriter {
    /// Strict JSON gate. `serde_json` accepts no comments and no trailing
    /// commas, so anything that is not plain JSON fails here and the file
    /// text is left untouched by the caller.
    fn parse_strict(&self, text: &str) -> Result<JsonValue, AgentWriteError> {
        serde_json::from_str(text).map_err(|err| {
            unsafe_error(format!(
                "opencode.json contains comments or invalid JSON (refusing to rewrite it): {err}"
            ))
        })
    }

    /// The object value stored under `"mcp": { <name>: … }` for `entry`.
    fn render_value(entry: &McpEntryDef) -> JsonValue {
        let mut object = serde_json::Map::new();
        if entry.transport == "stdio" {
            object.insert("type".into(), "local".into());
            let mut command: Vec<JsonValue> = Vec::new();
            if let Some(program) = &entry.command {
                command.push(program.clone().into());
            }
            command.extend(entry.args.iter().map(|arg| arg.clone().into()));
            object.insert("command".into(), JsonValue::Array(command));
            if !entry.env.is_empty() {
                object.insert(
                    "environment".into(),
                    serde_json::to_value(&entry.env)
                        .expect("BTreeMap<String, String> always serializes"),
                );
            }
        } else {
            // "http" and "streamable-http" both deploy as opencode "remote".
            object.insert("type".into(), "remote".into());
            object.insert(
                "url".into(),
                entry.url.clone().unwrap_or_default().into(),
            );
        }
        JsonValue::Object(object)
    }

    /// Full `"name": { … }` member text, compact value form.
    fn render_member(entry: &McpEntryDef) -> String {
        format!(
            "{}: {}",
            json_string(&entry.name),
            Self::render_value(entry)
        )
    }

    /// Byte offset of the root object's `{`, refusing non-object roots.
    fn root_object_start(&self, file_text: &str) -> Result<usize, AgentWriteError> {
        json_span::object_start(file_text, 0)
            .ok_or_else(|| unsafe_error("opencode.json root is not a JSON object"))
    }

    /// Byte offset of the `mcp` section's `{`, refusing a non-object section.
    fn mcp_object_start(
        &self,
        file_text: &str,
        mcp: json_span::MemberSpan,
    ) -> Result<usize, AgentWriteError> {
        json_span::object_start(file_text, mcp.value_start)
            .ok_or_else(|| unsafe_error("opencode.json `mcp` section is not a JSON object"))
    }

    fn validate_upsert(
        &self,
        new_text: &str,
        entry: &McpEntryDef,
    ) -> Result<(), AgentWriteError> {
        let doc = self.parse_strict(new_text)?;
        let stored = doc.get("mcp").and_then(|mcp| mcp.get(&entry.name));
        let expected = Self::render_value(entry);
        if stored != Some(&expected) {
            return Err(unsafe_error(format!(
                "internal error: opencode.json did not validate after upsert of `{}`",
                entry.name
            )));
        }
        Ok(())
    }

    fn validate_remove(&self, new_text: &str, name: &str) -> Result<(), AgentWriteError> {
        let doc = self.parse_strict(new_text)?;
        if let Some(mcp) = doc.get("mcp") {
            if mcp.get(name).is_some() {
                return Err(unsafe_error(format!(
                    "internal error: opencode.json still contains `{name}` after remove"
                )));
            }
        }
        Ok(())
    }
}

impl McpWriter for OpenCodeWriter {
    fn upsert(&self, file_text: &str, entry: &McpEntryDef) -> Result<String, AgentWriteError> {
        let root = self.parse_strict(file_text)?;
        if !root.is_object() {
            return Err(unsafe_error("opencode.json root is not a JSON object"));
        }
        let root_obj = self.root_object_start(file_text)?;
        let member = Self::render_member(entry);

        let new_text = match json_span::find_member(file_text, root_obj, "mcp") {
            // No `mcp` key yet: create the section as a new first top-level
            // member, comma-correct for empty and non-empty roots.
            None => {
                let section = format!("\"mcp\": {{ {member} }}");
                if root.as_object().is_some_and(|map| map.is_empty()) {
                    format!(
                        "{} {} {}",
                        &file_text[..root_obj + 1],
                        section,
                        &file_text[root_obj + 1..]
                    )
                } else {
                    format!(
                        "{}\n  {section},{}",
                        &file_text[..root_obj + 1],
                        &file_text[root_obj + 1..]
                    )
                }
            }
            Some(mcp) => {
                let mcp_obj = self.mcp_object_start(file_text, mcp)?;
                match json_span::find_member(file_text, mcp_obj, &entry.name) {
                    // Existing managed entry: replace exactly its value span.
                    Some(existing) => format!(
                        "{}{}{}",
                        &file_text[..existing.value_start],
                        Self::render_value(entry),
                        &file_text[existing.value_end..]
                    ),
                    // New entry: append as the last `mcp` member.
                    None => {
                        let members = json_span::collect_members(file_text, mcp_obj)
                            .ok_or_else(|| unsafe_error("cannot parse `mcp` object members"))?;
                        match members.last() {
                            Some(last) => format!(
                                "{}, {}{}",
                                &file_text[..last.member_end],
                                member,
                                &file_text[last.member_end..]
                            ),
                            None => format!(
                                "{} {} {}",
                                &file_text[..mcp_obj + 1],
                                member,
                                &file_text[mcp_obj + 1..]
                            ),
                        }
                    }
                }
            }
        };

        self.validate_upsert(&new_text, entry)?;
        Ok(new_text)
    }

    fn remove(&self, file_text: &str, name: &str) -> Result<String, AgentWriteError> {
        // Strict-parse gate; the splice itself is purely span-based.
        self.parse_strict(file_text)?;
        let root_obj = self.root_object_start(file_text)?;

        // Absent section or absent member: idempotent success, byte-identical.
        let Some(mcp) = json_span::find_member(file_text, root_obj, "mcp") else {
            return Ok(file_text.to_string());
        };
        let mcp_obj = self.mcp_object_start(file_text, mcp)?;
        let Some(member) = json_span::find_member(file_text, mcp_obj, name) else {
            return Ok(file_text.to_string());
        };

        // Cut the member plus one adjacent comma separator: prefer the
        // trailing comma, fall back to the leading one when `name` is the
        // last member. The only member has no comma at all, which leaves the
        // `mcp` object empty (`{ … }` with the original inner whitespace).
        let bytes = file_text.as_bytes();
        let mut cut_start = member.member_start;
        let mut cut_end = member.member_end;
        let after = json_span::skip_ws(file_text, member.member_end);
        if bytes.get(after) == Some(&b',') {
            cut_end = after + 1;
        } else {
            let mut before = member.member_start;
            while before > 0 && matches!(bytes[before - 1], b' ' | b'\t' | b'\n' | b'\r') {
                before -= 1;
            }
            if before > 0 && bytes[before - 1] == b',' {
                cut_start = before - 1;
            }
        }
        let new_text = format!("{}{}", &file_text[..cut_start], &file_text[cut_end..]);

        self.validate_remove(&new_text, name)?;
        Ok(new_text)
    }

    fn read_entry(&self, file_text: &str, name: &str) -> Result<Option<McpEntryDef>, AgentWriteError> {
        self.parse_strict(file_text)?;
        let root_obj = self.root_object_start(file_text)?;
        let Some(mcp) = json_span::find_member(file_text, root_obj, "mcp") else {
            return Ok(None);
        };
        let mcp_obj = self.mcp_object_start(file_text, mcp)?;
        let Some(member) = json_span::find_member(file_text, mcp_obj, name) else {
            return Ok(None);
        };
        let value = self.parse_strict(&file_text[member.value_start..member.value_end])?;
        let object = value
            .as_object()
            .ok_or_else(|| unsafe_error(format!("opencode.json `{name}` is not an object")))?;

        let raw_type = object.get("type").and_then(JsonValue::as_str);
        let url = object
            .get("url")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        let mut command = None;
        let mut args = Vec::new();
        match object.get("command") {
            // OpenCode merges program and args into one array: undo that.
            Some(JsonValue::Array(parts)) => {
                let mut parts = parts.iter().filter_map(JsonValue::as_str);
                command = parts.next().map(str::to_string);
                args = parts.map(str::to_string).collect();
            }
            Some(JsonValue::String(text)) => command = Some(text.clone()),
            _ => {}
        }
        // Full env values — unlike the inventory, this feeds drift diffs and
        // takeover, so nothing is masked or key-name-only here.
        let env = object
            .get("environment")
            .and_then(JsonValue::as_object)
            .map(|map| {
                map.iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            value
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| value.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Some(McpEntryDef {
            name: name.to_string(),
            transport: normalize_transport(raw_type, url.is_some(), command.is_some()),
            command,
            args,
            url,
            env,
        }))
    }

    fn empty_doc(&self) -> &'static str {
        EMPTY_DOC
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const FIXTURE: &str = r#"{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "agentmemory": { "type": "local", "command": ["npx","-y","@agentmemory/mcp"], "environment": { "AGENTMEMORY_URL": "http://localhost:3111" } },
    "zvec-grep-remote": { "type": "remote", "url": "http://127.0.0.1:7999/mcp" }
  },
  "provider": { "openai": { "apiKey": "nope" } }
}"#;

    /// Foreign text that must survive every operation byte-for-byte.
    const PROVIDER_MEMBER: &str = r#""provider": { "openai": { "apiKey": "nope" } }"#;
    const AGENTMEMORY_MEMBER: &str = r#""agentmemory": { "type": "local", "command": ["npx","-y","@agentmemory/mcp"], "environment": { "AGENTMEMORY_URL": "http://localhost:3111" } }"#;
    const ZVEC_MEMBER: &str = r#""zvec-grep-remote": { "type": "remote", "url": "http://127.0.0.1:7999/mcp" }"#;

    const JSONC: &str = r#"{
  // hand-written note: keep me
  "mcp": {}
}"#;

    fn stdio_entry(name: &str) -> McpEntryDef {
        let mut env = BTreeMap::new();
        env.insert("API_TOKEN".to_string(), "sekrit-value".to_string());
        McpEntryDef {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some("node".to_string()),
            args: vec!["/opt/servers/index.js".to_string(), "--stdio".to_string()],
            url: None,
            env,
        }
    }

    fn remote_entry(name: &str) -> McpEntryDef {
        McpEntryDef {
            name: name.to_string(),
            transport: "http".to_string(),
            command: None,
            args: Vec::new(),
            url: Some("https://example.com/mcp".to_string()),
            env: BTreeMap::new(),
        }
    }

    fn parse(text: &str) -> JsonValue {
        serde_json::from_str(text).expect("test expectation: writer output parses")
    }

    #[test]
    fn upsert_appends_new_entry_leaving_foreign_text_untouched() {
        let out = OpenCodeWriter.upsert(FIXTURE, &stdio_entry("agentx")).unwrap();

        assert_eq!(
            parse(&out)["mcp"]["agentx"],
            serde_json::json!({
                "type": "local",
                "command": ["node", "/opt/servers/index.js", "--stdio"],
                "environment": { "API_TOKEN": "sekrit-value" },
            })
        );
        // Pre-existing members and unrelated sections keep their exact bytes.
        assert!(out.contains(PROVIDER_MEMBER));
        assert!(out.contains(AGENTMEMORY_MEMBER));
        assert!(out.contains(ZVEC_MEMBER));
        // ADR-0005 clause 9: the native enabled flag is never written.
        assert!(!out.contains("\"enabled\""));
    }

    #[test]
    fn upsert_replaces_only_the_value_of_the_existing_member() {
        let out = OpenCodeWriter
            .upsert(FIXTURE, &stdio_entry("zvec-grep-remote"))
            .unwrap();

        assert_eq!(
            parse(&out)["mcp"]["zvec-grep-remote"]["type"],
            JsonValue::from("local")
        );
        // The old remote value is gone; everything else is byte-identical.
        assert!(!out.contains(r#""url": "http://127.0.0.1:7999/mcp""#));
        assert!(out.contains(AGENTMEMORY_MEMBER));
        assert!(out.contains(PROVIDER_MEMBER));
        assert!(out.contains(r#""$schema": "https://opencode.ai/config.json","#));
    }

    #[test]
    fn upsert_into_missing_mcp_section_inserts_first_top_level_member() {
        let src = r#"{
  "$schema": "https://opencode.ai/config.json",
  "provider": { "openai": { "apiKey": "nope" } }
}"#;
        let out = OpenCodeWriter.upsert(src, &remote_entry("agentx")).unwrap();

        assert!(out.starts_with("{\n  \"mcp\": {"));
        assert_eq!(
            parse(&out)["mcp"]["agentx"],
            serde_json::json!({ "type": "remote", "url": "https://example.com/mcp" })
        );
        assert!(out.contains(PROVIDER_MEMBER));
    }

    #[test]
    fn upsert_into_empty_mcp_object_has_no_leading_comma() {
        let out = OpenCodeWriter.upsert(r#"{"mcp": {}}"#, &remote_entry("agentx")).unwrap();
        assert_eq!(
            parse(&out)["mcp"]["agentx"]["url"],
            JsonValue::from("https://example.com/mcp")
        );
        // No stray "," before the only member.
        assert!(!out.contains("{ ,") && !out.contains(", \"agentx\""));
    }

    #[test]
    fn upsert_into_empty_root_object_works() {
        let out = OpenCodeWriter.upsert("{}", &remote_entry("agentx")).unwrap();
        assert_eq!(parse(&out)["mcp"]["agentx"]["type"], JsonValue::from("remote"));
    }

    #[test]
    fn upsert_refuses_jsonc_and_invalid_json() {
        for src in [JSONC, r#"{"mcp": {},}"#, r#"{"mcp": 5}"#, "[1,2]", "not json"] {
            let err = OpenCodeWriter.upsert(src, &remote_entry("agentx")).unwrap_err();
            assert!(
                matches!(err, AgentWriteError::Unsafe(_)),
                "expected Unsafe for {src:?}, got {err:?}"
            );
            assert!(
                err.to_string().contains("comments or invalid JSON")
                    || err.to_string().contains("not a JSON object"),
                "unclear refusal reason: {err}"
            );
        }
    }

    #[test]
    fn remove_middle_member_consumes_trailing_comma() {
        let out = OpenCodeWriter.remove(FIXTURE, "agentmemory").unwrap();

        assert_eq!(
            parse(&out)["mcp"]["zvec-grep-remote"]["type"],
            JsonValue::from("remote")
        );
        assert!(!out.contains("agentmemory"));
        assert!(out.contains(ZVEC_MEMBER));
        assert!(out.contains(PROVIDER_MEMBER));
        assert!(!out.contains(",,"));
    }

    #[test]
    fn remove_last_member_consumes_leading_comma() {
        let out = OpenCodeWriter.remove(FIXTURE, "zvec-grep-remote").unwrap();

        assert!(out.contains(AGENTMEMORY_MEMBER));
        assert!(!out.contains("zvec-grep-remote"));
        assert!(out.contains(PROVIDER_MEMBER));
        // agentmemory is now the last member with no comma after it — proof
        // by re-parse, since serde_json rejects trailing commas.
        assert_eq!(parse(&out)["mcp"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn remove_only_member_leaves_empty_mcp_object() {
        let src = r#"{"mcp": {
    "solo": { "type": "remote", "url": "u" }
}}"#;
        let out = OpenCodeWriter.remove(src, "solo").unwrap();
        assert_eq!(parse(&out)["mcp"], serde_json::json!({}));
    }

    #[test]
    fn remove_is_idempotent_when_name_or_section_absent() {
        assert_eq!(
            OpenCodeWriter.remove(FIXTURE, "ghost").unwrap(),
            FIXTURE.to_string()
        );
        let no_mcp = r#"{"provider": { "openai": {} }}"#;
        assert_eq!(OpenCodeWriter.remove(no_mcp, "ghost").unwrap(), no_mcp);
    }

    #[test]
    fn remove_refuses_jsonc() {
        let err = OpenCodeWriter.remove(JSONC, "agentmemory").unwrap_err();
        assert!(err.to_string().contains("comments or invalid JSON"));
    }

    #[test]
    fn read_entry_parses_local_and_remote_with_full_env_values() {
        let local = OpenCodeWriter
            .read_entry(FIXTURE, "agentmemory")
            .unwrap()
            .expect("present");
        assert_eq!(local.transport, "stdio");
        assert_eq!(local.command.as_deref(), Some("npx"));
        assert_eq!(local.args, ["-y", "@agentmemory/mcp"]);
        assert_eq!(local.url, None);
        assert_eq!(
            local.env.get("AGENTMEMORY_URL").map(String::as_str),
            Some("http://localhost:3111")
        );

        let remote = OpenCodeWriter
            .read_entry(FIXTURE, "zvec-grep-remote")
            .unwrap()
            .expect("present");
        // opencode "remote" reports as "http" — exactly what the inventory
        // shows (shared normalize_transport vocabulary).
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("http://127.0.0.1:7999/mcp"));
        assert_eq!(remote.command, None);
        assert!(remote.args.is_empty());
    }

    #[test]
    fn read_entry_absent_is_none_not_error() {
        assert_eq!(OpenCodeWriter.read_entry(FIXTURE, "ghost").unwrap(), None);
        assert_eq!(
            OpenCodeWriter
                .read_entry(r#"{"provider": {}}"#, "ghost")
                .unwrap(),
            None
        );
    }

    #[test]
    fn read_entry_refuses_jsonc() {
        let err = OpenCodeWriter.read_entry(JSONC, "mcp").unwrap_err();
        assert!(err.to_string().contains("comments or invalid JSON"));
    }

    #[test]
    fn render_then_read_roundtrips_entry_definition() {
        let entry = stdio_entry("roundtrip");
        let out = OpenCodeWriter.upsert(FIXTURE, &entry).unwrap();
        assert_eq!(
            OpenCodeWriter.read_entry(&out, "roundtrip").unwrap(),
            Some(entry)
        );

        let remote = remote_entry("roundtrip-r");
        let out = OpenCodeWriter.upsert(FIXTURE, &remote).unwrap();
        assert_eq!(
            OpenCodeWriter.read_entry(&out, "roundtrip-r").unwrap(),
            Some(remote)
        );
    }

    #[test]
    fn writer_is_registered_for_opencode() {
        let writer = super::super::writer_for_agent("opencode").expect("registered");
        let out = writer.upsert(FIXTURE, &remote_entry("agentx")).unwrap();
        assert!(out.contains(PROVIDER_MEMBER));
        assert!(parse(&out)["mcp"]["agentx"].is_object());
    }
}
