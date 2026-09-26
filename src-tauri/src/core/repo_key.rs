//! Canonical repo keys (CONTEXT.md: **Repo Key**).
//!
//! One git repository can be spelled many ways — `https://github.com/o/r.git`,
//! `git@github.com:o/r`, `ssh://git@github.com/o/r`, the skills.sh market
//! shorthand `o/r`, or a GitHub tree URL like
//! `https://github.com/o/r/tree/main/sub/dir`. Per ADR-0003, all of these
//! collapse to one canonical key (`github.com/o/r`) that decides which
//! Skill Source a skill belongs to and whether an install is a duplicate.

/// Collapse any accepted spelling of a git repository into its canonical key.
///
/// Returns `None` when the input does not identify a `host/owner/repo`
/// triple (bare words, a bare host, local paths, empty strings…). The key is
/// lowercased: repository hosts route case-insensitively, and grouping must
/// not split one repo into two because of casing.
pub fn canonical_repo_key(source_ref: &str) -> Option<String> {
    let mut s = source_ref.trim().to_string();
    if s.is_empty() {
        return None;
    }

    // Strip scheme.
    for scheme in ["https://", "http://", "ssh://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }

    // Strip a GitHub-style `/tree/<branch>/<path…>` suffix — that URL names a
    // subfolder inside the repo, not a different repo.
    if let Some(idx) = s.find("/tree/") {
        s = s[..idx].to_string();
    }

    // User part handling: `git@host:path` (scp-style), `git@host:path/sub`,
    // and `user@host/path` all reduce to `host/path`.
    if let Some(at) = s.find('@') {
        let after = &s[at + 1..];
        let first_slash = after.find('/');
        // A colon in the host segment is scp syntax; one after the first
        // slash is a plain path character and must survive.
        let colon_in_host = first_slash
            .and_then(|sl| after[..sl].find(':'))
            .or_else(|| if first_slash.is_none() { after.find(':') } else { None });
        match colon_in_host {
            Some(c) => {
                let host = &after[..c];
                let path = &after[c + 1..];
                if host.is_empty() || path.is_empty() {
                    return None;
                }
                s = format!("{host}/{path}");
            }
            None => s = after.to_string(),
        }
    }

    // Strip `.git` suffixes and trailing slashes (either order).
    loop {
        let lower = s.to_lowercase();
        if lower.ends_with(".git") && s.len() >= 4 {
            s.truncate(s.len() - 4);
        } else if s.ends_with('/') {
            s.truncate(s.len() - 1);
        } else {
            break;
        }
    }
    if s.is_empty() {
        return None;
    }

    // Split off the first segment and decide whether it is a host.
    let (first, rest) = match s.split_once('/') {
        Some((f, r)) => (f, Some(r)),
        None => (s.as_str(), None),
    };
    let host_like = first.contains('.')
        || first.contains(':')
        || first == "localhost"
        || first.parse::<std::net::IpAddr>().is_ok();

    let s = if host_like {
        let host = match first.split_once(':') {
            Some((h, _)) if !h.is_empty() => h,
            _ => first,
        };
        // A bare host names no repository.
        let rest = rest?;
        format!("{host}/{rest}")
    } else {
        // No host, has a slash → skills.sh market shorthand `owner/repo`,
        // which is always GitHub (matches install_from_skillssh, which
        // clones `https://github.com/{source}.git`).
        format!("github.com/{s}")
    };

    // Must be host/owner/repo (≥ 3 segments) to name a repository.
    let segments: Vec<&str> = s.split('/').filter(|seg| !seg.is_empty()).collect();
    if segments.len() < 3 {
        return None;
    }
    Some(segments.join("/").to_lowercase())
}

/// The repo key for a skills.sh market source reference.
///
/// A market `source_ref` is `owner/repo/<skill-id>` — the skill id is a path
/// *inside* the repo, never a repo segment. GitHub has no nested groups, so
/// the repo is always exactly `github.com/owner/repo` (ADR-0003): without
/// this truncation every market skill became its own pseudo-repo group and
/// the dedup probe could never match it against a git install of the same
/// repo+path.
pub fn skillssh_repo_key(source_ref: &str) -> Option<String> {
    let key = canonical_repo_key(source_ref)?;
    let mut segments = key.splitn(4, '/');
    match (segments.next(), segments.next(), segments.next()) {
        (Some(a), Some(b), Some(c)) => Some(format!("{a}/{b}/{c}")),
        // Fewer than three segments: `canonical_repo_key` already refused
        // those, but stay honest rather than panic on a future change.
        _ => Some(key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> String {
        canonical_repo_key(s).unwrap()
    }

    #[test]
    fn https_url() {
        assert_eq!(key("https://github.com/mattpocock/skills"), "github.com/mattpocock/skills");
    }

    #[test]
    fn https_url_with_git_suffix() {
        assert_eq!(key("https://github.com/o/r.git"), "github.com/o/r");
    }

    #[test]
    fn case_is_normalized() {
        assert_eq!(
            key("https://github.com/MattPocock/Skills.GIT"),
            "github.com/mattpocock/skills"
        );
    }

    #[test]
    fn scp_style() {
        assert_eq!(key("git@github.com:o/r.git"), "github.com/o/r");
        assert_eq!(key("git@github.com:o/r"), "github.com/o/r");
        // SCP URLs carry the whole repo path after the colon — GitLab nested
        // groups (`group/sub/repo`) are real repos, so nothing after the
        // colon is treated as a subpath (only `/tree/` URLs are stripped).
        assert_eq!(key("git@gitlab.com:g/s/r.git"), "gitlab.com/g/s/r");
    }

    #[test]
    fn ssh_url() {
        assert_eq!(key("ssh://git@github.com/o/r.git"), "github.com/o/r");
        assert_eq!(key("ssh://git@github.com/o/r"), "github.com/o/r");
    }

    #[test]
    fn market_shorthand_maps_to_github() {
        assert_eq!(
            skillssh_repo_key("mattpocock/skills"),
            Some("github.com/mattpocock/skills".into())
        );
    }

    #[test]
    fn market_source_ref_truncates_skill_id() {
        // install_from_skillssh stores `owner/repo/<skill-id>` as source_ref;
        // the skill id is a path inside the repo, not a repo segment
        // (ADR-0003: one repo, one key, one group).
        assert_eq!(
            skillssh_repo_key("mattpocock/skills/handdraw-style-prompter"),
            Some("github.com/mattpocock/skills".into())
        );
        // Converges with every git spelling of the same repo.
        assert_eq!(
            skillssh_repo_key("MattPocock/Skills/Foo"),
            Some(canonical_repo_key("https://github.com/MattPocock/Skills.git").unwrap())
        );
    }

    #[test]
    fn tree_url_strips_to_repo() {
        assert_eq!(key("https://github.com/o/r/tree/main/skills/foo"), "github.com/o/r");
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        assert_eq!(key("https://github.com/o/r/"), "github.com/o/r");
    }

    #[test]
    fn non_github_hosts_are_kept() {
        assert_eq!(key("https://gitlab.com/o/r"), "gitlab.com/o/r");
        assert_eq!(key("git@git.sr.ht:~user/repo.git"), "git.sr.ht/~user/repo");
    }

    #[test]
    fn port_is_stripped_from_host() {
        assert_eq!(key("http://localhost:8080/o/r"), "localhost/o/r");
        assert_eq!(key("https://192.168.1.5:7990/o/r"), "192.168.1.5/o/r");
    }

    #[test]
    fn same_repo_all_spellings_collide() {
        let expected = "github.com/o/r";
        assert_eq!(key("https://github.com/o/r.git"), expected);
        assert_eq!(key("git@github.com:o/r"), expected);
        assert_eq!(key("ssh://git@github.com/o/r"), expected);
        assert_eq!(key("O/R"), expected);
        assert_eq!(key("https://github.com/o/r/tree/dev"), expected);
    }

    #[test]
    fn rejects_non_repo_inputs() {
        assert_eq!(canonical_repo_key(""), None);
        assert_eq!(canonical_repo_key("   "), None);
        assert_eq!(canonical_repo_key("just-a-word"), None);
        assert_eq!(canonical_repo_key("https://github.com/o"), None);
        assert_eq!(canonical_repo_key("https://github.com"), None);
    }
}
