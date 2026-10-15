//! Marketplaces: catalogs of skills the user registers once, browses, and
//! installs from.
//!
//! A marketplace is either a folder on disk — typically a clone the user keeps
//! and pulls themselves — or a remote repository, checked out from the shared
//! repo cache whenever it is refreshed or installed from.
//!
//! Skills installed from a marketplace are ordinary library skills. One from a
//! folder is a `local` skill pointing at its folder inside the clone, so pulling
//! the clone surfaces as "update available" and re-import applies it. One from
//! a repository is a `git` skill with the skill's subpath, so the existing
//! update check and update flow apply unchanged. Which library skills came from
//! a marketplace is worked out from those sources rather than stored, so a skill
//! installed earlier through Git Install shows as installed here too.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::State;

use super::skills::{
    git_source_from_skill, log_install_outcome, store_installed_skill_unlocked,
    InstallSourceMetadata,
};
use crate::core::{
    error::AppError,
    file_manager, git_fetcher,
    install_cancel::InstallCancelRegistry,
    installer,
    marketplace::{self, Catalog, CatalogFormat, ExternalSource},
    repo_lock::RepoLock,
    skill_store::{MarketplaceRecord, SkillRecord, SkillStore},
};

pub const KIND_LOCAL: &str = "local";
pub const KIND_REMOTE: &str = "remote";

#[derive(Debug, Serialize)]
pub struct MarketplaceDto {
    pub id: String,
    pub name: String,
    /// `"local"` or `"remote"`.
    pub kind: String,
    /// The folder path, or the URL as entered.
    pub location: String,
    pub clone_url: Option<String>,
    pub branch: Option<String>,
    pub subpath: Option<String>,
    /// A web page for a remote marketplace hosted on GitHub.
    pub web_url: Option<String>,
    /// `None` until the first successful scan.
    pub format: Option<CatalogFormat>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub revision: Option<String>,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
    pub plugin_count: usize,
    pub skill_count: usize,
    pub installed_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct MarketplaceDetailDto {
    pub marketplace: MarketplaceDto,
    pub plugins: Vec<MarketplacePluginDto>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MarketplacePluginDto {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub category: Option<String>,
    pub external: Option<ExternalSource>,
    pub skills: Vec<MarketplaceSkillDto>,
}

#[derive(Debug, Serialize)]
pub struct MarketplaceSkillDto {
    pub rel_path: String,
    pub name: String,
    pub description: Option<String>,
    /// The library skill this entry is already installed as, if any.
    pub installed_skill_id: Option<String>,
    pub installed_name: Option<String>,
    pub update_status: Option<String>,
}

/// The marketplaces Claude Code has registered, as candidates to import.
#[derive(Debug, Serialize)]
pub struct ClaudeMarketplacesDto {
    /// Where they were read from; `None` when there is no home folder.
    pub config_dir: Option<String>,
    pub marketplaces: Vec<ClaudeMarketplaceDto>,
}

#[derive(Debug, Serialize)]
pub struct ClaudeMarketplaceDto {
    /// Claude Code's name for it, used as the name here too.
    pub name: String,
    /// The source kind in Claude Code's configuration.
    pub kind: String,
    /// What importing adds: a folder path or a repository URL.
    pub location: Option<String>,
    /// Why it cannot be imported, when it cannot.
    pub unsupported: Option<String>,
    /// The marketplace already registered here for the same location.
    pub existing_name: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct MarketplaceInstallResult {
    /// Names the new library skills were given.
    pub installed: Vec<String>,
    /// Already in the library; left as they are.
    pub skipped: Vec<String>,
    /// `"<name>: <reason>"` per skill that could not be installed.
    pub failed: Vec<String>,
}

/// What [`add_marketplace_internal`] was given, worked out.
enum Location {
    Local(PathBuf),
    Remote(git_fetcher::ParsedGitSource),
}

// ── Internal API (shared with the CLI) ──

pub fn list_marketplaces_internal(store: &SkillStore) -> Result<Vec<MarketplaceDto>, AppError> {
    let skills = store.get_all_skills().map_err(AppError::db)?;
    let index = InstalledIndex::new(&skills);
    store
        .get_all_marketplaces()
        .map_err(AppError::db)?
        .into_iter()
        .map(|record| {
            let catalog = stored_catalog(&record);
            Ok(marketplace_dto(&record, catalog.as_ref(), &index))
        })
        .collect()
}

/// The marketplace and everything in it. A folder is re-read every time — it
/// is cheap and the user may have just pulled it; a repository shows the
/// catalog from its last refresh.
pub fn get_marketplace_internal(store: &SkillStore, id: &str) -> Result<MarketplaceDetailDto, AppError> {
    let record = load_record(store, id)?;
    if record.kind == KIND_LOCAL {
        match scan_local(Path::new(&record.location)) {
            Ok((catalog, revision)) => {
                save_catalog(store, id, &catalog, revision.as_deref())?;
                return detail_dto(store, &load_record(store, id)?, Some(catalog));
            }
            Err(error) => {
                store
                    .update_marketplace_error(id, &error.message)
                    .map_err(AppError::db)?;
                return detail_dto(store, &load_record(store, id)?, None);
            }
        }
    }
    detail_dto(store, &record, None)
}

/// Register a marketplace. `location` is a folder path or anything Git Install
/// accepts: a clone URL, `owner/repo`, or a GitHub `…/tree/<branch>/<path>` URL
/// naming the folder the marketplace lives in.
///
/// The marketplace is scanned before it is saved, so a typo or a repository the
/// user cannot reach is reported instead of leaving a dead entry behind.
pub fn add_marketplace_internal(
    store: &SkillStore,
    location: &str,
    name: Option<&str>,
    proxy_url: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_progress: Option<git_fetcher::ProgressCallback>,
) -> Result<MarketplaceDetailDto, AppError> {
    let location = classify_location(location, proxy_url, true)?;
    let existing = store.get_all_marketplaces().map_err(AppError::db)?;
    if let Some(duplicate) = existing.iter().find(|record| is_same_location(record, &location)) {
        return Err(AppError::invalid_input(format!(
            "This marketplace is already added as '{}'",
            duplicate.name
        )));
    }

    let now = chrono::Utc::now().timestamp_millis();
    let (record, catalog) = match location {
        Location::Local(path) => {
            let (catalog, revision) = scan_local(&path)?;
            let fallback = folder_name(&path);
            let record = MarketplaceRecord {
                id: uuid::Uuid::new_v4().to_string(),
                name: display_name(name, &catalog, &fallback),
                kind: KIND_LOCAL.to_string(),
                location: path.to_string_lossy().to_string(),
                clone_url: None,
                branch: None,
                subpath: None,
                catalog: Some(catalog_json(&catalog)?),
                revision,
                last_synced_at: Some(now),
                last_error: None,
                created_at: now,
                updated_at: now,
            };
            (record, catalog)
        }
        Location::Remote(parsed) => {
            let (catalog, revision) = scan_remote(
                &parsed.clone_url,
                parsed.branch.as_deref(),
                parsed.subpath.as_deref(),
                proxy_url,
                cancel,
                on_progress,
            )?;
            let fallback = repo_name(&parsed.clone_url, parsed.subpath.as_deref());
            let record = MarketplaceRecord {
                id: uuid::Uuid::new_v4().to_string(),
                name: display_name(name, &catalog, &fallback),
                kind: KIND_REMOTE.to_string(),
                location: parsed.original_url.clone(),
                clone_url: Some(parsed.clone_url.clone()),
                branch: parsed.branch.clone(),
                subpath: parsed.subpath.clone(),
                catalog: Some(catalog_json(&catalog)?),
                revision,
                last_synced_at: Some(now),
                last_error: None,
                created_at: now,
                updated_at: now,
            };
            (record, catalog)
        }
    };

    store.insert_marketplace(&record).map_err(AppError::db)?;
    detail_dto(store, &record, Some(catalog))
}

/// Scan the marketplace again: re-read the folder, or fetch the repository.
/// A failure is recorded on the marketplace and the last good catalog stays
/// browsable.
pub fn refresh_marketplace_internal(
    store: &SkillStore,
    id: &str,
    proxy_url: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_progress: Option<git_fetcher::ProgressCallback>,
) -> Result<MarketplaceDetailDto, AppError> {
    let record = load_record(store, id)?;
    let scanned = if record.kind == KIND_LOCAL {
        scan_local(Path::new(&record.location))
    } else {
        let clone_url = remote_clone_url(&record)?;
        scan_remote(
            clone_url,
            record.branch.as_deref(),
            record.subpath.as_deref(),
            proxy_url,
            cancel,
            on_progress,
        )
    };
    match scanned {
        Ok((catalog, revision)) => {
            save_catalog(store, id, &catalog, revision.as_deref())?;
            detail_dto(store, &load_record(store, id)?, Some(catalog))
        }
        Err(error) => {
            // A cancelled refresh says nothing about the marketplace.
            if error.kind != crate::core::error::ErrorKind::Cancelled {
                store
                    .update_marketplace_error(id, &error.message)
                    .map_err(AppError::db)?;
            }
            Err(error)
        }
    }
}

pub fn rename_marketplace_internal(
    store: &SkillStore,
    id: &str,
    name: &str,
) -> Result<MarketplaceDto, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::invalid_input("Marketplace name cannot be empty"));
    }
    load_record(store, id)?;
    store.rename_marketplace(id, name).map_err(AppError::db)?;
    let record = load_record(store, id)?;
    let skills = store.get_all_skills().map_err(AppError::db)?;
    Ok(marketplace_dto(
        &record,
        stored_catalog(&record).as_ref(),
        &InstalledIndex::new(&skills),
    ))
}

/// Forget a marketplace. Skills installed from it stay in the library and keep
/// their own sources, so they go on updating as before.
pub fn remove_marketplace_internal(store: &SkillStore, id: &str) -> Result<(), AppError> {
    load_record(store, id)?;
    store.delete_marketplace(id).map_err(AppError::db)
}

/// Install the catalog entries at `rel_paths`. Entries already in the library
/// are skipped rather than overwritten — updating them is the library's update
/// flow, which asks before it removes anything.
pub fn install_marketplace_skills_internal(
    store: &SkillStore,
    id: &str,
    rel_paths: &[String],
    proxy_url: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_progress: Option<git_fetcher::ProgressCallback>,
) -> Result<MarketplaceInstallResult, AppError> {
    let record = load_record(store, id)?;
    let mut wanted = Vec::new();
    let mut seen = HashSet::new();
    for rel_path in rel_paths {
        if seen.insert(rel_path.as_str()) {
            wanted.push(rel_path.as_str());
        }
    }
    if wanted.is_empty() {
        return Ok(MarketplaceInstallResult::default());
    }

    if record.kind == KIND_LOCAL {
        let root = PathBuf::from(&record.location);
        let (catalog, revision) = scan_local(&root)?;
        save_catalog(store, id, &catalog, revision.as_deref())?;
        let _lock =
            RepoLock::acquire_foreground("install marketplace skills").map_err(AppError::db)?;
        return install_entries(store, &record, &catalog, &wanted, cancel, |rel_path| {
            let dir = marketplace::resolve_skill_path(&root, rel_path)
                .map_err(|e| AppError::invalid_input(e.to_string()))?;
            let result = installer::install_from_local(&dir, None).map_err(AppError::io)?;
            let metadata = InstallSourceMetadata {
                source_type: "local".to_string(),
                source_ref: Some(dir.to_string_lossy().to_string()),
                source_ref_resolved: None,
                source_subpath: None,
                source_branch: None,
                source_revision: None,
                remote_revision: None,
                update_status: "local_only".to_string(),
            };
            Ok((result, metadata))
        });
    }

    let clone_url = remote_clone_url(&record)?.to_string();
    let checkout = git_fetcher::clone_repo_ref_with_progress(
        &clone_url,
        record.branch.as_deref(),
        cancel,
        proxy_url,
        on_progress,
    )
    .map_err(AppError::classify_git_error)?;
    let outcome = (|| {
        // Install from what was just fetched, and list that: the stored catalog
        // may be older than the checkout.
        let root = scan_root(&checkout, record.subpath.as_deref())?;
        let catalog = marketplace::read_catalog(&checkout, &root)
            .map_err(|e| AppError::invalid_input(format!("{e:#}")))?;
        let revision = git_fetcher::get_head_revision(&checkout).map_err(AppError::git)?;
        save_catalog(store, id, &catalog, Some(&revision))?;
        let _lock =
            RepoLock::acquire_foreground("install marketplace skills").map_err(AppError::db)?;
        // Checked once the lock is held, right before anything is written.
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(AppError::cancelled("Installation cancelled by user"));
        }
        install_entries(store, &record, &catalog, &wanted, cancel, |rel_path| {
            let dir = marketplace::resolve_skill_path(&checkout, rel_path)
                .map_err(|e| AppError::invalid_input(e.to_string()))?;
            let result = installer::install_from_git_dir(&dir, None).map_err(AppError::io)?;
            let metadata = InstallSourceMetadata {
                source_type: "git".to_string(),
                source_ref: Some(clone_url.clone()),
                source_ref_resolved: Some(clone_url.clone()),
                source_subpath: (rel_path != ".").then(|| rel_path.to_string()),
                source_branch: record.branch.clone(),
                source_revision: Some(revision.clone()),
                remote_revision: Some(revision.clone()),
                update_status: "up_to_date".to_string(),
            };
            Ok((result, metadata))
        })
    })();
    git_fetcher::cleanup_temp(&checkout);
    outcome
}

/// The shared half of installing: skip what is installed, install the rest one
/// by one, and keep going past a skill that fails. The caller holds the lock.
///
/// A cancel stops before the next skill; the ones already installed stay.
fn install_entries(
    store: &SkillStore,
    record: &MarketplaceRecord,
    catalog: &Catalog,
    wanted: &[&str],
    cancel: Option<&Arc<AtomicBool>>,
    install_one: impl Fn(&str) -> Result<(installer::InstallResult, InstallSourceMetadata), AppError>,
) -> Result<MarketplaceInstallResult, AppError> {
    let skills = store.get_all_skills().map_err(AppError::db)?;
    let index = InstalledIndex::new(&skills);
    let mut summary = MarketplaceInstallResult::default();

    for &rel_path in wanted {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(AppError::cancelled("Installation cancelled by user"));
        }
        let Some(entry) = catalog.find_skill(rel_path) else {
            summary
                .failed
                .push(format!("{rel_path}: no longer in this marketplace"));
            continue;
        };
        if let Some(existing) = index.find(record, rel_path) {
            summary.skipped.push(existing.name.clone());
            continue;
        }
        let outcome = install_one(rel_path).and_then(|(result, metadata)| {
            let skill_id = store_installed_skill_unlocked(store, &result, &metadata, None)?;
            Ok((skill_id, result.name))
        });
        log_install_outcome(
            store,
            &format!("marketplace {}", record.name),
            outcome.as_ref(),
        );
        match outcome {
            Ok((_, name)) => summary.installed.push(name),
            Err(error) => summary.failed.push(format!("{}: {}", entry.name, error.message)),
        }
    }
    Ok(summary)
}

/// Claude Code's marketplaces, each with what importing it would add and
/// whether it is here already. Reads local files only: duplicates are judged
/// without asking a remote, and importing re-checks with the full resolution.
pub fn claude_code_marketplaces_internal(
    store: &SkillStore,
) -> Result<ClaudeMarketplacesDto, AppError> {
    let Some(config_dir) = marketplace::claude_config_dir() else {
        return Ok(ClaudeMarketplacesDto {
            config_dir: None,
            marketplaces: Vec::new(),
        });
    };
    let found = marketplace::read_claude_marketplaces(&config_dir)
        .map_err(|e| AppError::invalid_input(format!("{e:#}")))?;
    let existing = store.get_all_marketplaces().map_err(AppError::db)?;
    let marketplaces = found
        .into_iter()
        .map(|entry| {
            let (location, unsupported, existing_name) = match entry.location {
                Err(reason) => (None, Some(reason), None),
                Ok(location) => match classify_location(&location, None, false) {
                    Err(error) => (Some(location), Some(error.message), None),
                    Ok(classified) => {
                        let existing_name = existing
                            .iter()
                            .find(|record| is_same_location(record, &classified))
                            .map(|record| record.name.clone());
                        (Some(location), None, existing_name)
                    }
                },
            };
            ClaudeMarketplaceDto {
                name: entry.name,
                kind: entry.kind,
                location,
                unsupported,
                existing_name,
            }
        })
        .collect();
    Ok(ClaudeMarketplacesDto {
        config_dir: Some(config_dir.to_string_lossy().to_string()),
        marketplaces,
    })
}

pub fn marketplace_folder_internal(store: &SkillStore, id: &str) -> Result<PathBuf, AppError> {
    let record = load_record(store, id)?;
    if record.kind != KIND_LOCAL {
        return Err(AppError::invalid_input(
            "Only a marketplace on this machine has a folder to show",
        ));
    }
    let path = PathBuf::from(&record.location);
    if !path.is_dir() {
        return Err(AppError::invalid_input(format!(
            "Folder does not exist: {}",
            record.location
        )));
    }
    Ok(path)
}

/// Find a marketplace by id, or by name when the name is unambiguous — what a
/// CLI user types.
pub fn resolve_marketplace_ref(store: &SkillStore, reference: &str) -> Result<String, AppError> {
    let records = store.get_all_marketplaces().map_err(AppError::db)?;
    if let Some(record) = records.iter().find(|record| record.id == reference) {
        return Ok(record.id.clone());
    }
    let matches: Vec<&MarketplaceRecord> = records
        .iter()
        .filter(|record| record.name.eq_ignore_ascii_case(reference) || record.location == reference)
        .collect();
    match matches.as_slice() {
        [record] => Ok(record.id.clone()),
        [] => Err(AppError::not_found(format!("No marketplace named '{reference}'"))),
        _ => Err(AppError::invalid_input(format!(
            "'{reference}' matches several marketplaces; use its id"
        ))),
    }
}

// ── Scanning ──

fn scan_local(path: &Path) -> Result<(Catalog, Option<String>), AppError> {
    if !path.is_dir() {
        return Err(AppError::not_found(format!(
            "Folder does not exist: {}",
            path.display()
        )));
    }
    let catalog = marketplace::read_catalog(path, path)
        .map_err(|e| AppError::invalid_input(format!("{e:#}")))?;
    // A clone's commit is worth showing; a folder that is not one has none.
    let revision = if path.join(".git").exists() {
        git_fetcher::get_head_revision(path).ok()
    } else {
        None
    };
    Ok((catalog, revision))
}

fn scan_remote(
    clone_url: &str,
    branch: Option<&str>,
    subpath: Option<&str>,
    proxy_url: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_progress: Option<git_fetcher::ProgressCallback>,
) -> Result<(Catalog, Option<String>), AppError> {
    git_fetcher::validate_git_url(clone_url).map_err(AppError::git)?;
    let checkout =
        git_fetcher::clone_repo_ref_with_progress(clone_url, branch, cancel, proxy_url, on_progress)
            .map_err(AppError::classify_git_error)?;
    let scanned = (|| {
        let root = scan_root(&checkout, subpath)?;
        let catalog = marketplace::read_catalog(&checkout, &root)
            .map_err(|e| AppError::invalid_input(format!("{e:#}")))?;
        Ok((catalog, git_fetcher::get_head_revision(&checkout).ok()))
    })();
    git_fetcher::cleanup_temp(&checkout);
    scanned
}

fn scan_root(checkout: &Path, subpath: Option<&str>) -> Result<PathBuf, AppError> {
    let Some(subpath) = subpath.filter(|s| !s.is_empty()) else {
        return Ok(checkout.to_path_buf());
    };
    let root = checkout.join(subpath);
    if !crate::core::path_guard::is_path_safe(checkout, &root) {
        return Err(AppError::invalid_input(format!(
            "Path '{subpath}' resolves outside the repository"
        )));
    }
    if !root.is_dir() {
        return Err(AppError::not_found(format!(
            "Path '{subpath}' does not exist in the repository"
        )));
    }
    Ok(root)
}

/// What `input` names. `resolve_refs` asks the remote which part of a GitHub
/// `…/tree/a/b/c` URL is the branch; without it the first segment is taken,
/// which needs no network and is right for every branch without a `/`.
fn classify_location(
    input: &str,
    proxy_url: Option<&str>,
    resolve_refs: bool,
) -> Result<Location, AppError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::invalid_input(
            "Enter a folder path or a repository URL",
        ));
    }
    let lower = trimmed.to_lowercase();
    let looks_like_url = ["https://", "http://", "ssh://", "git@"]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    let path = expand_home(trimmed);
    if !looks_like_url && (path.is_absolute() || trimmed.starts_with('~') || path.is_dir()) {
        if !path.is_dir() {
            return Err(AppError::invalid_input(format!(
                "Folder does not exist: {trimmed}"
            )));
        }
        return Ok(Location::Local(canonical_folder(&path).map_err(AppError::io)?));
    }
    git_fetcher::validate_git_url(trimmed).map_err(|e| AppError::invalid_input(e.to_string()))?;
    Ok(Location::Remote(if resolve_refs {
        git_fetcher::parse_git_source_resolved(trimmed, proxy_url)
    } else {
        git_fetcher::parse_git_source(trimmed)
    }))
}

/// `canonicalize` without the `\\?\` prefix Windows puts on the result, which
/// would otherwise show up in the UI and in the source of every skill installed
/// from the folder.
fn canonical_folder(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            // `\\?\UNC\server\share` has no shorter spelling that means the same.
            if !rest.starts_with(r"UNC\") {
                return Ok(PathBuf::from(rest));
            }
        }
    }
    Ok(canonical)
}

fn expand_home(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if input == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(input)
}

fn is_same_location(record: &MarketplaceRecord, location: &Location) -> bool {
    match location {
        Location::Local(path) => {
            record.kind == KIND_LOCAL && path_key(Path::new(&record.location)) == path_key(path)
        }
        Location::Remote(parsed) => {
            record.kind == KIND_REMOTE
                && record.clone_url.as_deref().map(url_key) == Some(url_key(&parsed.clone_url))
                && record.branch == parsed.branch
                && subpath_key(record.subpath.as_deref()) == subpath_key(parsed.subpath.as_deref())
        }
    }
}

// ── Which catalog entries are installed ──

/// A library skill from a repository, with the branch it tracks.
type TrackedSkill<'a> = (Option<String>, &'a SkillRecord);

/// Library skills keyed by where they came from, so a catalog entry can tell
/// whether it is already installed — whichever way it got there.
struct InstalledIndex<'a> {
    by_path: HashMap<PathBuf, &'a SkillRecord>,
    /// Keyed by repository and subpath.
    by_remote: HashMap<(String, String), Vec<TrackedSkill<'a>>>,
}

impl<'a> InstalledIndex<'a> {
    fn new(skills: &'a [SkillRecord]) -> Self {
        let mut by_path = HashMap::new();
        let mut by_remote: HashMap<_, Vec<_>> = HashMap::new();
        for skill in skills {
            match skill.source_type.as_str() {
                "local" | "import" => {
                    if let Some(source) = skill.source_ref.as_deref() {
                        by_path.entry(path_key(Path::new(source))).or_insert(skill);
                    }
                }
                // Read the source the way the update check does, so records
                // from before `source_ref_resolved` existed are found too.
                "git" | "skillssh" => {
                    if let Ok(source) = git_source_from_skill(skill) {
                        by_remote
                            .entry((url_key(&source.clone_url), subpath_key(source.subpath.as_deref())))
                            .or_default()
                            .push((source.branch, skill));
                    }
                }
                _ => {}
            }
        }
        Self { by_path, by_remote }
    }

    fn find(&self, record: &MarketplaceRecord, rel_path: &str) -> Option<&'a SkillRecord> {
        if record.kind == KIND_LOCAL {
            let dir = marketplace::join_rel(Path::new(&record.location), rel_path);
            return self.by_path.get(&path_key(&dir)).copied();
        }
        let clone_url = record.clone_url.as_deref()?;
        self.by_remote
            .get(&(url_key(clone_url), subpath_key(Some(rel_path))))?
            .iter()
            // No branch means the default one, which may well be the branch
            // the other side names; only two different named branches differ.
            .find(|(branch, _)| match (branch.as_deref(), record.branch.as_deref()) {
                (Some(installed), Some(wanted)) => installed == wanted,
                _ => true,
            })
            .map(|(_, skill)| *skill)
    }
}

fn path_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        PathBuf::from(
            path.to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string(),
        )
    })
}

/// Spellings of one clone URL that name the same repository: with or without
/// `.git` or a trailing `/` and, on GitHub — which ignores case and serves a
/// repository over HTTPS and SSH alike — any of those forms. Other hosts keep
/// their case and scheme, since a self-hosted server may treat `Repo` and
/// `repo` as two repositories.
fn url_key(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let lower = trimmed.to_lowercase();
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ] {
        if let Some(repo) = lower.strip_prefix(prefix) {
            return format!("github.com/{repo}");
        }
    }
    trimmed.to_string()
}

fn subpath_key(subpath: Option<&str>) -> String {
    let normalized = subpath.unwrap_or("").replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed == "." {
        String::new()
    } else {
        trimmed.to_string()
    }
}

// ── DTOs ──

fn detail_dto(
    store: &SkillStore,
    record: &MarketplaceRecord,
    catalog: Option<Catalog>,
) -> Result<MarketplaceDetailDto, AppError> {
    let catalog = catalog.or_else(|| stored_catalog(record));
    let skills = store.get_all_skills().map_err(AppError::db)?;
    let index = InstalledIndex::new(&skills);
    let marketplace = marketplace_dto(record, catalog.as_ref(), &index);
    let Some(catalog) = catalog else {
        return Ok(MarketplaceDetailDto {
            marketplace,
            plugins: Vec::new(),
            warnings: Vec::new(),
        });
    };
    let plugins = catalog
        .plugins
        .into_iter()
        .map(|plugin| MarketplacePluginDto {
            skills: plugin
                .skills
                .into_iter()
                .map(|skill| {
                    let installed = index.find(record, &skill.rel_path);
                    MarketplaceSkillDto {
                        installed_skill_id: installed.map(|s| s.id.clone()),
                        installed_name: installed.map(|s| s.name.clone()),
                        update_status: installed.map(|s| s.update_status.clone()),
                        rel_path: skill.rel_path,
                        name: skill.name,
                        description: skill.description,
                    }
                })
                .collect(),
            name: plugin.name,
            description: plugin.description,
            version: plugin.version,
            category: plugin.category,
            external: plugin.external,
        })
        .collect();
    Ok(MarketplaceDetailDto {
        marketplace,
        plugins,
        warnings: catalog.warnings,
    })
}

fn marketplace_dto(
    record: &MarketplaceRecord,
    catalog: Option<&Catalog>,
    index: &InstalledIndex<'_>,
) -> MarketplaceDto {
    let installed_count = catalog
        .map(|catalog| {
            catalog
                .plugins
                .iter()
                .flat_map(|plugin| plugin.skills.iter())
                .filter(|skill| index.find(record, &skill.rel_path).is_some())
                .count()
        })
        .unwrap_or(0);
    MarketplaceDto {
        id: record.id.clone(),
        name: record.name.clone(),
        kind: record.kind.clone(),
        location: record.location.clone(),
        clone_url: record.clone_url.clone(),
        branch: record.branch.clone(),
        subpath: record.subpath.clone(),
        web_url: web_url(record),
        format: catalog.map(|catalog| catalog.format),
        description: catalog.and_then(|catalog| catalog.description.clone()),
        owner: catalog.and_then(|catalog| catalog.owner.clone()),
        revision: record.revision.clone(),
        last_synced_at: record.last_synced_at,
        last_error: record.last_error.clone(),
        plugin_count: catalog
            .map(|catalog| {
                catalog
                    .plugins
                    .iter()
                    .filter(|plugin| !plugin.name.is_empty())
                    .count()
            })
            .unwrap_or(0),
        skill_count: catalog.map(Catalog::skill_count).unwrap_or(0),
        installed_count,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

fn web_url(record: &MarketplaceRecord) -> Option<String> {
    let base = marketplace::github_web_base(record.clone_url.as_deref()?)?;
    let subpath = record.subpath.as_deref().filter(|s| !s.is_empty());
    Some(match (record.branch.as_deref(), subpath) {
        (None, None) => base,
        (branch, subpath) => {
            let mut url = format!("{base}/tree/{}", branch.unwrap_or("HEAD"));
            if let Some(subpath) = subpath {
                url.push('/');
                url.push_str(subpath);
            }
            url
        }
    })
}

// ── Helpers ──

fn load_record(store: &SkillStore, id: &str) -> Result<MarketplaceRecord, AppError> {
    store
        .get_marketplace_by_id(id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Marketplace not found"))
}

fn remote_clone_url(record: &MarketplaceRecord) -> Result<&str, AppError> {
    record
        .clone_url
        .as_deref()
        .ok_or_else(|| AppError::invalid_input("Marketplace has no repository URL"))
}

fn stored_catalog(record: &MarketplaceRecord) -> Option<Catalog> {
    let raw = record.catalog.as_deref()?;
    match serde_json::from_str(raw) {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            log::warn!("stored catalog of marketplace {} is unreadable: {error}", record.id);
            None
        }
    }
}

fn catalog_json(catalog: &Catalog) -> Result<String, AppError> {
    serde_json::to_string(catalog).map_err(AppError::internal)
}

fn save_catalog(
    store: &SkillStore,
    id: &str,
    catalog: &Catalog,
    revision: Option<&str>,
) -> Result<(), AppError> {
    store
        .update_marketplace_catalog(id, &catalog_json(catalog)?, revision)
        .map_err(AppError::db)
}

fn display_name(requested: Option<&str>, catalog: &Catalog, fallback: &str) -> String {
    requested
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| catalog.name.clone())
        .unwrap_or_else(|| fallback.to_string())
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// `repo` for `…/owner/repo.git`, or the last folder of `subpath` when the
/// marketplace lives in one.
fn repo_name(clone_url: &str, subpath: Option<&str>) -> String {
    if let Some(last) = subpath
        .and_then(|subpath| subpath.trim_matches('/').rsplit('/').next())
        .filter(|last| !last.is_empty())
    {
        return last.to_string();
    }
    let trimmed = clone_url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    trimmed
        .rsplit(['/', ':'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

// ── Tauri commands ──

/// Unregisters an operation's cancel key once its command returns.
struct CancelKeyGuard {
    registry: Arc<InstallCancelRegistry>,
    key: String,
}

impl Drop for CancelKeyGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.key);
    }
}

/// Clone progress, reported on the same `install-progress` channel the other
/// installers use, under `key`.
fn progress_reporter(app_handle: &tauri::AppHandle, key: &str) -> git_fetcher::ProgressCallback {
    use tauri::Emitter;
    let app_handle = app_handle.clone();
    let key = key.to_string();
    Box::new(move |detail: &str| {
        app_handle
            .emit(
                "install-progress",
                serde_json::json!({ "skill_id": key, "phase": "cloning", "detail": detail }),
            )
            .ok();
    })
}

/// The cancel/progress key of an operation on an existing marketplace.
pub fn operation_key(id: &str) -> String {
    format!("marketplace:{id}")
}

#[tauri::command]
pub async fn list_marketplaces(
    store: State<'_, Arc<SkillStore>>,
) -> Result<Vec<MarketplaceDto>, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || list_marketplaces_internal(&store)).await?
}

#[tauri::command]
pub async fn get_marketplace(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<MarketplaceDetailDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || get_marketplace_internal(&store, &id)).await?
}

/// Progress and cancellation are keyed `marketplace:<location>` as entered.
#[tauri::command]
pub async fn add_marketplace(
    location: String,
    name: Option<String>,
    store: State<'_, Arc<SkillStore>>,
    cancel_registry: State<'_, Arc<InstallCancelRegistry>>,
    app_handle: tauri::AppHandle,
) -> Result<MarketplaceDetailDto, AppError> {
    let store = store.inner().clone();
    let proxy_url = store.proxy_url();
    let registry = cancel_registry.inner().clone();
    let key = operation_key(&location);
    let cancel = registry.register(&key);
    let _guard = CancelKeyGuard { registry, key: key.clone() };
    tauri::async_runtime::spawn_blocking(move || {
        add_marketplace_internal(
            &store,
            &location,
            name.as_deref(),
            proxy_url.as_deref(),
            Some(&cancel),
            Some(progress_reporter(&app_handle, &key)),
        )
    })
    .await?
}

#[tauri::command]
pub async fn refresh_marketplace(
    id: String,
    store: State<'_, Arc<SkillStore>>,
    cancel_registry: State<'_, Arc<InstallCancelRegistry>>,
    app_handle: tauri::AppHandle,
) -> Result<MarketplaceDetailDto, AppError> {
    let store = store.inner().clone();
    let proxy_url = store.proxy_url();
    let registry = cancel_registry.inner().clone();
    let key = operation_key(&id);
    let cancel = registry.register(&key);
    let _guard = CancelKeyGuard { registry, key: key.clone() };
    tauri::async_runtime::spawn_blocking(move || {
        refresh_marketplace_internal(
            &store,
            &id,
            proxy_url.as_deref(),
            Some(&cancel),
            Some(progress_reporter(&app_handle, &key)),
        )
    })
    .await?
}

#[tauri::command]
pub async fn rename_marketplace(
    id: String,
    name: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<MarketplaceDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || rename_marketplace_internal(&store, &id, &name))
        .await?
}

#[tauri::command]
pub async fn remove_marketplace(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || remove_marketplace_internal(&store, &id)).await?
}

#[tauri::command]
pub async fn install_marketplace_skills(
    id: String,
    rel_paths: Vec<String>,
    store: State<'_, Arc<SkillStore>>,
    cancel_registry: State<'_, Arc<InstallCancelRegistry>>,
    app_handle: tauri::AppHandle,
) -> Result<MarketplaceInstallResult, AppError> {
    let store = store.inner().clone();
    let proxy_url = store.proxy_url();
    let registry = cancel_registry.inner().clone();
    let key = operation_key(&id);
    let cancel = registry.register(&key);
    let _guard = CancelKeyGuard { registry, key: key.clone() };
    tauri::async_runtime::spawn_blocking(move || {
        install_marketplace_skills_internal(
            &store,
            &id,
            &rel_paths,
            proxy_url.as_deref(),
            Some(&cancel),
            Some(progress_reporter(&app_handle, &key)),
        )
    })
    .await?
}

#[tauri::command]
pub async fn list_claude_code_marketplaces(
    store: State<'_, Arc<SkillStore>>,
) -> Result<ClaudeMarketplacesDto, AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || claude_code_marketplaces_internal(&store)).await?
}

#[tauri::command]
pub async fn reveal_marketplace_folder(
    id: String,
    store: State<'_, Arc<SkillStore>>,
) -> Result<(), AppError> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = marketplace_folder_internal(&store, &id)?;
        // explorer.exe exits non-zero even when it succeeds, so the platform
        // branch is judged by `exit_is_failure` rather than checked here.
        let status = file_manager::reveal_item(&path).map_err(AppError::io)?;
        if file_manager::exit_is_failure(status.success()) {
            return Err(AppError::io(format!(
                "File manager exited with status: {status}"
            )));
        }
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_skill(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
        )
        .unwrap();
    }

    fn skill_record(id: &str, source_type: &str, source_ref: &str) -> SkillRecord {
        SkillRecord {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            source_type: source_type.to_string(),
            source_ref: Some(source_ref.to_string()),
            source_ref_resolved: None,
            source_subpath: None,
            source_branch: None,
            source_revision: None,
            remote_revision: None,
            central_path: format!("/library/{id}"),
            content_hash: None,
            enabled: true,
            created_at: 1,
            updated_at: 1,
            status: "ok".to_string(),
            update_status: "unknown".to_string(),
            last_checked_at: None,
            last_check_error: None,
        }
    }

    fn marketplace_record(kind: &str, location: &str, clone_url: Option<&str>) -> MarketplaceRecord {
        MarketplaceRecord {
            id: "m1".to_string(),
            name: "m".to_string(),
            kind: kind.to_string(),
            location: location.to_string(),
            clone_url: clone_url.map(str::to_string),
            branch: None,
            subpath: None,
            catalog: None,
            revision: None,
            last_synced_at: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn a_local_entry_is_installed_when_a_library_skill_points_at_its_folder() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_skill(&root.join("skills/alpha"), "alpha");
        write_skill(&root.join("skills/beta"), "beta");
        let skills = vec![skill_record(
            "alpha",
            "local",
            // With a trailing slash, as a folder picker may hand it over.
            &format!("{}/", root.join("skills/alpha").display()),
        )];
        let index = InstalledIndex::new(&skills);
        let record = marketplace_record(KIND_LOCAL, &root.to_string_lossy(), None);

        assert_eq!(index.find(&record, "skills/alpha").map(|s| s.id.as_str()), Some("alpha"));
        assert!(index.find(&record, "skills/beta").is_none());
    }

    #[test]
    fn a_remote_entry_matches_any_spelling_of_its_repository() {
        let mut from_git_tab = skill_record("alpha", "git", "https://github.com/Acme/Market");
        from_git_tab.source_ref_resolved = Some("https://github.com/Acme/Market.git".to_string());
        from_git_tab.source_subpath = Some("plugins/x/skills/alpha".to_string());
        let mut whole_repo = skill_record("solo", "git", "acme/solo");
        whole_repo.source_ref_resolved = None;
        let skills = vec![from_git_tab, whole_repo];
        let index = InstalledIndex::new(&skills);

        let market = marketplace_record(KIND_REMOTE, "acme/market", Some("https://github.com/acme/market"));
        assert_eq!(
            index.find(&market, "plugins/x/skills/alpha").map(|s| s.id.as_str()),
            Some("alpha")
        );
        assert!(index.find(&market, "plugins/x/skills/beta").is_none());

        let solo = marketplace_record(KIND_REMOTE, "acme/solo", Some("https://github.com/acme/solo.git"));
        assert_eq!(index.find(&solo, ".").map(|s| s.id.as_str()), Some("solo"));
    }

    #[test]
    fn github_repositories_match_across_ssh_https_and_case() {
        assert_eq!(url_key("git@github.com:Acme/Market.git"), url_key("https://github.com/acme/market"));
        assert_eq!(url_key("ssh://git@github.com/acme/market/"), url_key("https://github.com/acme/market.git"));
        // Self-hosted servers may be case-sensitive.
        assert_ne!(url_key("https://git.example.com/Acme/Market"), url_key("https://git.example.com/acme/market"));
    }

    #[test]
    fn branches_only_differ_when_both_are_named() {
        let mut on_main = skill_record("main-skill", "git", "https://github.com/acme/market");
        on_main.source_subpath = Some("skills/a".to_string());
        on_main.source_branch = Some("main".to_string());
        // An old record: no resolved URL, the subpath only in the tree URL.
        let old_record = skill_record(
            "old",
            "git",
            "git@github.com:acme/market.git",
        );
        let skills = vec![on_main, old_record];
        let index = InstalledIndex::new(&skills);

        let mut market = marketplace_record(KIND_REMOTE, "acme/market", Some("https://github.com/acme/market.git"));
        assert_eq!(index.find(&market, "skills/a").map(|s| s.id.as_str()), Some("main-skill"));
        assert_eq!(index.find(&market, ".").map(|s| s.id.as_str()), Some("old"));
        market.branch = Some("v2".to_string());
        assert!(index.find(&market, "skills/a").is_none());
        market.branch = Some("main".to_string());
        assert!(index.find(&market, "skills/a").is_some());
    }

    #[test]
    fn locations_are_told_apart_and_duplicates_recognised() {
        let tmp = tempdir().unwrap();
        let Location::Local(path) = classify_location(&tmp.path().to_string_lossy(), None, false).unwrap()
        else {
            panic!("an existing folder is a local marketplace");
        };
        let local = marketplace_record(KIND_LOCAL, &path.to_string_lossy(), None);
        let again = classify_location(&format!("{}/", tmp.path().display()), None, false).unwrap();
        assert!(is_same_location(&local, &again));

        assert!(classify_location("/definitely/not/here", None, false).is_err());
        assert!(classify_location("", None, false).is_err());
        assert!(classify_location("file:///etc", None, false).is_err());

        let Location::Remote(parsed) = classify_location("acme/market", None, false).unwrap() else {
            panic!("owner/repo is a remote marketplace");
        };
        assert_eq!(parsed.clone_url, "https://github.com/acme/market.git");
        let remote = marketplace_record(KIND_REMOTE, "acme/market", Some(&parsed.clone_url));
        let spelled_out = classify_location("https://github.com/acme/market", None, false).unwrap();
        assert!(is_same_location(&remote, &spelled_out));
        assert!(!is_same_location(&local, &spelled_out));
    }

    #[test]
    fn names_fall_back_to_the_repository_or_folder() {
        assert_eq!(repo_name("https://github.com/acme/market.git", None), "market");
        assert_eq!(repo_name("git@github.com:acme/market.git", None), "market");
        assert_eq!(repo_name("https://github.com/acme/mono", Some("tools/catalog/")), "catalog");
    }

    #[test]
    fn web_urls_point_at_the_marketplace_on_github() {
        let mut record = marketplace_record(KIND_REMOTE, "acme/market", Some("https://github.com/acme/market.git"));
        assert_eq!(web_url(&record).as_deref(), Some("https://github.com/acme/market"));
        record.subpath = Some("catalog".to_string());
        assert_eq!(
            web_url(&record).as_deref(),
            Some("https://github.com/acme/market/tree/HEAD/catalog")
        );
        record.clone_url = Some("https://gitlab.com/acme/market.git".to_string());
        assert_eq!(web_url(&record), None);
    }
}
