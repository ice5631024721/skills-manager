//! Commands for the management-view parent structure: Skill Sources
//! (auto-derived repo groups). ADR-0004 removed Collections — grouping is
//! source-only (CONTEXT.md).
//!
//! Design of record: `CONTEXT.md` plus `docs/adr/0001..0004` — grouping lives
//! only in the DB and the GUI; disk layouts stay flat.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::core::error::AppError;
use crate::core::skill_metadata;
use crate::core::skill_store::{SkillRecord, SkillStore};
use crate::core::{git_fetcher, source_description};

use super::skills::{apply_update_from_checkout, UpdateOutcome};

// ── DTOs ──

#[derive(Debug, Serialize)]
pub struct SkillSourceDto {
    pub id: String,
    pub repo_key: String,
    pub display_url: String,
    pub branch: Option<String>,
    pub description: Option<String>,
    /// `none` | `github` | `user`
    pub description_source: String,
    pub skill_count: i64,
}

/// One upstream skill found during a source refresh that is not installed.
/// Reported so the UI can offer the existing preview/confirm install flow —
/// refresh itself never installs silently (design Q2b).
#[derive(Debug, Serialize)]
pub struct SourceRefreshNewSkill {
    pub rel_path: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct SourceRefreshResult {
    /// Skills whose content actually changed and was applied.
    pub refreshed: Vec<String>,
    /// Skills already matching their upstream content.
    pub unchanged: Vec<String>,
    /// Updates held back because they would remove files and a batch has
    /// nobody to ask (same semantics as `batch_update_skills`).
    pub held_back: Vec<String>,
    pub failed: Vec<String>,
    /// Skills whose upstream path vanished; kept locally, marked in the DB.
    pub upstream_deleted: Vec<String>,
    /// Upstream skills present in the repo but not installed.
    pub new_skills: Vec<SourceRefreshNewSkill>,
}

// ── Sources ──

fn source_counts(store: &SkillStore) -> HashMap<String, i64> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    for link in store.get_skill_source_links().unwrap_or_default() {
        if let Some(source_id) = link.source_id {
            *counts.entry(source_id).or_insert(0) += 1;
        }
    }
    counts
}

fn source_to_dto(source: crate::core::skill_store::SkillSourceRecord, count: i64) -> SkillSourceDto {
    SkillSourceDto {
        id: source.id,
        repo_key: source.repo_key,
        display_url: source.display_url,
        branch: source.branch,
        description: source.description,
        description_source: source.description_source,
        skill_count: count,
    }
}

#[tauri::command]
pub async fn get_skill_sources(
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<SkillSourceDto>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let counts = source_counts(&store);
        Ok(store
            .get_all_skill_sources()
            .map_err(AppError::db)?
            .into_iter()
            .map(|s| {
                let count = counts.get(&s.id).copied().unwrap_or(0);
                source_to_dto(s, count)
            })
            .collect())
    })
    .await?
}

#[tauri::command]
pub async fn refresh_skill_source_description(
    source_id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<SkillSourceDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let source = store
            .get_skill_source(&source_id)
            .map_err(AppError::db)?
            .ok_or_else(|| AppError::not_found("Skill source not found"))?;

        // A user-written description always wins over a re-fetch.
        if source.description_source != "user" {
            let fetched = source_description::fetch_github_description(
                &source.repo_key,
                store.proxy_url().as_deref(),
            );
            let (description, kind) = match fetched {
                Some(desc) => (Some(desc), "github"),
                None => (None, "none"),
            };
            store
                .update_skill_source_description(&source_id, description.as_deref(), kind)
                .map_err(AppError::db)?;
        }

        let updated = store
            .get_skill_source(&source_id)
            .map_err(AppError::db)?
            .ok_or_else(|| AppError::not_found("Skill source not found"))?;
        Ok(source_to_dto(updated, 0))
    })
    .await?
}

#[tauri::command]
pub async fn set_skill_source_user_description(
    source_id: String,
    description: Option<String>,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        store
            .get_skill_source(&source_id)
            .map_err(AppError::db)?
            .ok_or_else(|| AppError::not_found("Skill source not found"))?;
        let trimmed = description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        store
            .update_skill_source_description(
                &source_id,
                trimmed,
                if trimmed.is_some() { "user" } else { "none" },
            )
            .map_err(AppError::db)?;
        Ok(())
    })
    .await?
}

/// One clone per distinct branch among the group's skills; every installed
/// skill is diffed against the fresh checkout and updated in place. New
/// upstream skills are *reported*, not installed; skills missing upstream are
/// kept and marked (design Q2b/Q3).
#[tauri::command]
pub async fn refresh_skill_source(
    source_id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<SourceRefreshResult, AppError> {
    let store = store.inner().clone();
    let proxy_url = store.proxy_url();
    tauri::async_runtime::spawn_blocking(move || {
        let source = store
            .get_skill_source(&source_id)
            .map_err(AppError::db)?
            .ok_or_else(|| AppError::not_found("Skill source not found"))?;
        let skill_ids = store
            .get_skill_ids_for_source(&source_id)
            .map_err(AppError::db)?;

        let mut result = SourceRefreshResult::default();
        if skill_ids.is_empty() {
            return Ok(result);
        }

        let mut by_branch: HashMap<Option<String>, Vec<SkillRecord>> = HashMap::new();
        for id in &skill_ids {
            let Some(skill) = store.get_skill_by_id(id).map_err(AppError::db)? else {
                continue;
            };
            if !matches!(skill.source_type.as_str(), "git" | "skillssh") {
                result
                    .failed
                    .push(format!("{}: not a git-sourced skill", skill.name));
                continue;
            }
            by_branch
                .entry(skill.source_branch.clone())
                .or_default()
                .push(skill);
        }

        let mut seen_new: HashSet<String> = HashSet::new();
        for (branch, skills) in &by_branch {
            // Any group member resolves to the same canonical repo; use the
            // first one that yields a clone URL.
            let Some(clone_url) = skills.iter().find_map(|s| {
                super::skills::git_source_from_skill(s)
                    .ok()
                    .map(|g| g.clone_url)
            }) else {
                for s in skills {
                    result
                        .failed
                        .push(format!("{}: cannot resolve clone URL", s.name));
                }
                continue;
            };

            let revision = match git_fetcher::resolve_remote_revision(
                &clone_url,
                branch.as_deref(),
                proxy_url.as_deref(),
            ) {
                Ok(rev) => rev,
                Err(e) => {
                    let message = e.to_string();
                    for s in skills {
                        let _ = store.update_skill_check_state(
                            &s.id,
                            s.remote_revision.as_deref(),
                            "error",
                            Some(&message),
                        );
                        result.failed.push(format!("{}: {}", s.name, message));
                    }
                    continue;
                }
            };

            // One full clone per branch — the payoff of grouping (design Q6①):
            // N skills, one network clone instead of N.
            let temp_dir =
                match git_fetcher::clone_repo_ref_scoped(&clone_url, branch.as_deref(), None, None, proxy_url.as_deref(), None)
                {
                    Ok(dir) => dir,
                    Err(e) => {
                        let message = AppError::classify_git_error(e).message;
                        for s in skills {
                            let _ = store.update_skill_check_state(
                                &s.id,
                                s.remote_revision.as_deref(),
                                "error",
                                Some(&message),
                            );
                            result.failed.push(format!("{}: {}", s.name, message));
                        }
                        continue;
                    }
                };

            let refresh_result = refresh_from_checkout(
                &store,
                skills,
                &temp_dir,
                &clone_url,
                branch.as_deref(),
                &revision,
                &mut result,
            );
            scan_new_skills(&store, &source.repo_key, &temp_dir, &mut seen_new, &mut result.new_skills);
            git_fetcher::cleanup_temp(&temp_dir);
            if let Err(e) = refresh_result {
                result.failed.push(e.message);
            }
        }

        Ok(result)
    })
    .await?
}

/// Diff every group skill against the fresh checkout and update in place.
fn refresh_from_checkout(
    store: &SkillStore,
    skills: &[SkillRecord],
    temp_dir: &Path,
    clone_url: &str,
    branch: Option<&str>,
    revision: &str,
    result: &mut SourceRefreshResult,
) -> Result<(), AppError> {
    for skill in skills {
        let subpath = skill.source_subpath.clone().unwrap_or_default();
        let candidate = if subpath.is_empty() {
            temp_dir.to_path_buf()
        } else {
            temp_dir.join(&subpath)
        };

        // Upstream vanished: keep the local copy, flag it (design Q3a).
        if !skill_metadata::is_valid_skill_dir(&candidate) {
            store
                .set_skill_upstream_deleted(&skill.id, true)
                .map_err(AppError::db)?;
            result.upstream_deleted.push(skill.name.clone());
            continue;
        }

        let fresh_hash =
            crate::core::content_hash::hash_directory(&candidate).map_err(AppError::io)?;
        if skill.content_hash.as_deref() == Some(fresh_hash.as_str()) {
            let _ = store.set_skill_upstream_deleted(&skill.id, false);
            result.unchanged.push(skill.name.clone());
            continue;
        }

        match apply_update_from_checkout(
            store,
            skill,
            temp_dir,
            &candidate,
            clone_url,
            branch,
            revision,
            None,
        ) {
            Ok(UpdateOutcome::Applied { content_changed }) => {
                if content_changed {
                    result.refreshed.push(skill.name.clone());
                } else {
                    result.unchanged.push(skill.name.clone());
                }
            }
            Ok(UpdateOutcome::Held { .. }) => {
                // Nobody to ask inside a batch; the user resolves this skill
                // through the per-skill update flow, which can confirm.
                result.held_back.push(skill.name.clone());
            }
            Err(e) => {
                let _ = store.update_skill_check_state(
                    &skill.id,
                    skill.remote_revision.as_deref(),
                    "error",
                    Some(&e.message),
                );
                result.failed.push(format!("{}: {}", skill.name, e.message));
            }
        }
    }
    Ok(())
}

/// Report upstream skills that are not installed anywhere under this repo key.
fn scan_new_skills(
    store: &SkillStore,
    repo_key: &str,
    temp_dir: &Path,
    seen: &mut HashSet<String>,
    out: &mut Vec<SourceRefreshNewSkill>,
) {
    // Scan the whole checkout — a repo root that is itself a skill shows up
    // as a `None` relative path, which is exactly how such installs are
    // recorded (`source_subpath IS NULL`), so the dedup probe below matches.
    for dir in super::skills::collect_git_skill_dirs(temp_dir) {
        let rel = git_fetcher::relative_subpath(temp_dir, &dir);
        if let Some(r) = &rel {
            if !seen.insert(r.clone()) {
                continue;
            }
        }
        let already_installed =
            matches!(store.find_skill_by_repo_and_subpath(repo_key, rel.as_deref()), Ok(Some(_)));
        if already_installed {
            continue;
        }
        let meta = skill_metadata::parse_skill_md(&dir);
        out.push(SourceRefreshNewSkill {
            rel_path: rel.unwrap_or_default(),
            name: meta
                .name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    dir.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                }),
            description: meta.description,
        });
    }
}

