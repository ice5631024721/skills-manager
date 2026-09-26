//! Pure byte-offset scanner for JSON text — no I/O, no dependencies beyond
//! std (plus serde_json only for decoding escaped key strings).
//!
//! Writers must not reformat a whole file to insert or delete one member;
//! they need exact byte ranges of `"key": value` members inside a given
//! object so they can splice text surgically (ADR-0006 §4: never rewrite the
//! file wholesale). This scanner provides exactly that:
//!
//! * [`find_member`] / [`collect_members`] locate members of the object whose
//!   opening `{` sits at `obj_start`.
//! * [`value_span`] measures any JSON value starting at a given byte offset.
//! * [`object_start`] finds the opening `{` of the value at an offset.
//!
//! Callers are expected to have strict-parsed the text first (`serde_json`),
//! so the scanner treats malformed input as "not found" rather than
//! attempting recovery. All offsets are byte offsets into `src`; every
//! returned range starts and ends on ASCII structural characters, so slicing
//! `src` with them is UTF-8-safe.

/// Byte spans of one object member `"key": value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberSpan {
    /// Offset of the key's opening quote.
    pub member_start: usize,
    /// Offset just past the end of the value (excludes any trailing comma).
    pub member_end: usize,
    /// Offset of the value's first byte.
    pub value_start: usize,
    /// Offset just past the value's last byte.
    pub value_end: usize,
}

/// Advance past JSON whitespace (` \t\n\r`) starting at `i`.
pub fn skip_ws(src: &str, mut i: usize) -> usize {
    let bytes = src.as_bytes();
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// If the value at offset `i` (leading whitespace allowed) is a JSON object,
/// return the offset of its opening `{`.
pub fn object_start(src: &str, i: usize) -> Option<usize> {
    let at = skip_ws(src, i);
    (src.as_bytes().get(at) == Some(&b'{')).then_some(at)
}

/// Exact `(start, end)` span of the JSON value beginning at offset `i`
/// (leading whitespace allowed; `end` is exclusive). Handles strings with
/// escapes, nested objects/arrays, and bare literals (numbers, `true`,
/// `false`, `null`).
pub fn value_span(src: &str, i: usize) -> Option<(usize, usize)> {
    let at = skip_ws(src, i);
    let first = *src.as_bytes().get(at)?;
    let end = match first {
        b'"' => string_end(src, at)?,
        b'{' | b'[' => container_end(src, at)?,
        b't' | b'f' | b'n' | b'-' | b'0'..=b'9' => literal_end(src, at),
        _ => return None,
    };
    Some((at, end))
}

/// Collect every member of the object whose `{` is at `obj_start`, in source
/// order. `None` when `obj_start` is not an object or the text is malformed.
pub fn collect_members(src: &str, obj_start: usize) -> Option<Vec<MemberSpan>> {
    let bytes = src.as_bytes();
    if bytes.get(obj_start) != Some(&b'{') {
        return None;
    }
    let mut members = Vec::new();
    let mut i = skip_ws(src, obj_start + 1);
    if bytes.get(i) == Some(&b'}') {
        return Some(members);
    }
    loop {
        // Key: JSON requires a string here.
        if bytes.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = string_end(src, i)?;
        let colon = skip_ws(src, key_end);
        if bytes.get(colon) != Some(&b':') {
            return None;
        }
        let (value_start, value_end) = value_span(src, colon + 1)?;
        members.push(MemberSpan {
            member_start: i,
            member_end: value_end,
            value_start,
            value_end,
        });
        let after = skip_ws(src, value_end);
        match bytes.get(after) {
            Some(&b',') => i = skip_ws(src, after + 1),
            Some(&b'}') => return Some(members),
            _ => return None,
        }
    }
}

/// Find the member with `key` inside the object whose `{` is at `obj_start`.
/// Keys that merely *occur* inside string values or nested documents are not
/// matches: scanning is strictly member-wise, one object level at a time.
/// A path like `["mcp", name]` is walked by chaining: locate `mcp`, then
/// `find_member` again at the returned `value_start` (which is the offset of
/// the `mcp` object's `{`).
pub fn find_member(src: &str, obj_start: usize, key: &str) -> Option<MemberSpan> {
    collect_members(src, obj_start)?
        .into_iter()
        .find(|member| match string_end(src, member.member_start) {
            Some(key_end) => key_matches(src, member.member_start, key_end, key),
            None => false,
        })
}

/// Offset just past the closing quote of the string starting at `open`.
/// Correctly skips `\"`, `\\` and `\uXXXX` (`\u` consumes two bytes; the four
/// hex digits can never be `"` or `\`).
fn string_end(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Offset just past the closing bracket of the object/array opening at
/// `open`, tracking nesting and suppressing bracket bytes inside strings.
fn container_end(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            match b {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Offset just past a number/true/false/null literal starting at `start`.
fn literal_end(src: &str, start: usize) -> usize {
    let bytes = src.as_bytes();
    let mut i = start;
    while i < bytes.len()
        && !matches!(
            bytes[i],
            b',' | b'{' | b'}' | b'[' | b']' | b' ' | b'\t' | b'\n' | b'\r'
        )
    {
        i += 1;
    }
    i
}

/// Does the quoted key text `src[open..close]` decode to `key`?
fn key_matches(src: &str, open: usize, close: usize, key: &str) -> bool {
    // Most keys are escape-free: compare the raw inner text first.
    let raw = &src[open + 1..close - 1];
    if !raw.contains('\\') {
        return raw == key;
    }
    decode_key(src, open, close).is_some_and(|decoded| decoded == key)
}

/// Decode a quoted JSON string (`src[open..close]`, quotes included).
fn decode_key(src: &str, open: usize, close: usize) -> Option<String> {
    serde_json::from_str::<String>(&src[open..close]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(src: &str, span: MemberSpan) -> &str {
        &src[span.member_start..span.member_end]
    }

    fn value_text(src: &str, span: MemberSpan) -> &str {
        &src[span.value_start..span.value_end]
    }

    #[test]
    fn finds_top_level_and_nested_member_spans() {
        let src = r#"{
  "mcp": {
    "zvec-grep-remote": { "type": "remote", "url": "http://127.0.0.1:7999/mcp" }
  },
  "provider": { "openai": { "apiKey": "nope" } }
}"#;
        let root = object_start(src, 0).unwrap();
        let mcp = find_member(src, root, "mcp").expect("mcp found");
        assert_eq!(value_text(src, mcp), "{\n    \"zvec-grep-remote\": { \"type\": \"remote\", \"url\": \"http://127.0.0.1:7999/mcp\" }\n  }");

        let entry = find_member(src, mcp.value_start, "zvec-grep-remote").expect("entry found");
        assert_eq!(
            value_text(src, entry),
            "{ \"type\": \"remote\", \"url\": \"http://127.0.0.1:7999/mcp\" }"
        );

        let url = find_member(src, entry.value_start, "url").expect("url found");
        assert_eq!(value_text(src, url), "\"http://127.0.0.1:7999/mcp\"");
        assert_eq!(
            slice(src, url),
            "\"url\": \"http://127.0.0.1:7999/mcp\""
        );
    }

    #[test]
    fn absent_key_returns_none() {
        let src = r#"{"a": 1, "b": {"c": 2}}"#;
        assert!(find_member(src, 0, "zzz").is_none());
        // Nested-only keys must not match at the top level.
        assert!(find_member(src, 0, "c").is_none());
    }

    #[test]
    fn keys_inside_string_values_do_not_match() {
        // "mcp" appears only inside a string value, with a quote-escaped form
        // and an escape-prefixed brace that must not confuse the scanner.
        let src = r#"{"a":"mcp","b":"say \"mcp\" now","c":"tail \\","d":[{"mcp":1}]}"#;
        assert!(find_member(src, 0, "mcp").is_none());
        // …and the members themselves are all still found.
        assert!(find_member(src, 0, "c").is_some());
        assert!(find_member(src, 0, "d").is_some());
    }

    #[test]
    fn escaped_and_unicode_keys_match_decoded_form() {
        let src = "{ \"mc\\u0070\": 1, \"\\u65e5\\u672c\": 2 }";
        let mcp = find_member(src, 0, "mcp").expect("escape-decoded key matches");
        assert_eq!(value_text(src, mcp), "1");
        let jp = find_member(src, 0, "\u{65e5}\u{672c}").expect("unicode key matches");
        assert_eq!(value_text(src, jp), "2");
        // Raw text must not match the escaped form literally.
        assert!(find_member(src, 0, "mc\\u0070").is_none());
    }

    #[test]
    fn unicode_values_keep_exact_spans() {
        let src = "{ \"url\": \"http://x/日本\u{1F600}\", \"m\": 0 }";
        let url = find_member(src, 0, "url").unwrap();
        assert_eq!(value_text(src, url), "\"http://x/日本\u{1F600}\"");
    }

    #[test]
    fn literal_and_array_value_spans() {
        let src = r#"{"n": -12.5e3, "t": true, "f":false, "z": null, "arr": [[1, {"x":"y"}], "]"], "mcp": {}}"#;
        for (key, want) in [
            ("n", "-12.5e3"),
            ("t", "true"),
            ("f", "false"),
            ("z", "null"),
            ("arr", r#"[[1, {"x":"y"}], "]"]"#),
            ("mcp", "{}"),
        ] {
            let m = find_member(src, 0, key).unwrap_or_else(|| panic!("member {key}"));
            assert_eq!(value_text(src, m), want, "value span for {key}");
        }
    }

    #[test]
    fn nested_brackets_and_quotes_in_arrays_are_balanced() {
        // The string "]" contains a bracket byte; depth counting must ignore it.
        let src = r#"{"list": ["{", "[", "]", "\""], "after": 1}"#;
        let list = find_member(src, 0, "list").unwrap();
        assert_eq!(
            value_text(src, list),
            r#"["{", "[", "]", "\""]"#
        );
        let after = find_member(src, 0, "after").unwrap();
        assert_eq!(value_text(src, after), "1");
    }

    #[test]
    fn empty_and_malformed_objects() {
        assert!(find_member("{}", 0, "any").is_none());
        assert!(find_member("{  }", 1, "any").is_none());
        // Trailing comma: malformed → no result (writers pre-parse strictly,
        // so this only guards against scanner misbehavior).
        assert!(collect_members(r#"{"a":1,}"#, 0).is_none());
        assert!(collect_members(r#"[1,2]"#, 0).is_none());
    }

    #[test]
    fn collect_members_returns_all_in_order() {
        let src = r#"{ "b": 1, "a": {"nested": 0}, "c": [2] }"#;
        let members = collect_members(src, 0).unwrap();
        let keys: Vec<&str> = members
            .iter()
            .map(|m| &src[m.member_start + 1..m.member_start + 2])
            .collect();
        assert_eq!(keys, vec!["b", "a", "c"]);
        assert_eq!(members[1].member_end, members[1].value_end);
    }

    #[test]
    fn object_start_skips_leading_whitespace() {
        let src = "\n\n  {\"a\":1}";
        assert_eq!(object_start(src, 0), Some(4));
        assert_eq!(find_member(src, object_start(src, 0).unwrap(), "a").unwrap().value_end, src.len() - 1);
        assert_eq!(object_start("[1]", 0), None);
    }
}
