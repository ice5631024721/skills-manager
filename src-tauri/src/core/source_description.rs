//! Best-effort GitHub repository description lookup for Skill Sources.
//!
//! CONTEXT.md: a Skill Source carries a description so the group header alone
//! tells you what the skills inside are for. The description is synced from
//! the GitHub repo when possible and stays user-editable (user edits win —
//! `description_source` on the store row tracks who wrote it).
//!
//! Everything here is non-fatal by contract: a failed lookup returns `None`
//! and must never block or fail an install.

/// Fetch the GitHub repository description for a canonical repo key.
///
/// Returns `None` for non-GitHub hosts (the description field simply stays
/// hand-written), unknown repos, rate limits, and any network problem.
pub fn fetch_github_description(repo_key: &str, proxy_url: Option<&str>) -> Option<String> {
    let rest = repo_key.strip_prefix("github.com/")?;
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    let client = crate::core::skillssh_api::build_http_client(proxy_url, 6);
    let resp = client
        .get(format!("https://api.github.com/repos/{owner}/{repo}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        log::debug!(
            "github description lookup for {repo_key} failed: {}",
            resp.status()
        );
        return None;
    }
    let body: serde_json::Value = resp.json().ok()?;
    let description = body.get("description")?.as_str()?.trim();
    if description.is_empty() {
        None
    } else {
        Some(description.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_github_host_short_circuits_without_network() {
        assert_eq!(fetch_github_description("gitlab.com/o/r", None), None);
    }

    #[test]
    fn malformed_keys_return_none_without_network() {
        assert_eq!(fetch_github_description("github.com/o", None), None);
        assert_eq!(fetch_github_description("github.com", None), None);
    }
}
