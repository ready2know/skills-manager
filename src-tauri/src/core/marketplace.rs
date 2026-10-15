//! Reading a marketplace: a catalog of skills the user registers once and
//! installs from.
//!
//! A marketplace is a directory tree — a clone the user keeps on disk, or a
//! remote repository checked out on demand. Two layouts are understood:
//!
//! - a Claude Code plugin marketplace: `.claude-plugin/marketplace.json` lists
//!   plugins, and each plugin keeps its skills under `skills/` (or wherever its
//!   manifest's `skills` field points);
//! - anything else: every folder holding a `SKILL.md` is a skill.
//!
//! Nothing here touches the library or the database; it only answers "what
//! could be installed from this tree, and where does each skill live in it".

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{path_guard, scanner, skill_metadata};

pub const MANIFEST_PATH: &str = ".claude-plugin/marketplace.json";
const PLUGIN_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";
const DEFAULT_PLUGIN_SKILLS_DIR: &str = "skills";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFormat {
    /// `.claude-plugin/marketplace.json` was found and read.
    ClaudePlugins,
    /// No manifest: the tree was searched for skill folders.
    SkillFolders,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub format: CatalogFormat,
    pub name: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub plugins: Vec<CatalogPlugin>,
    /// Problems that did not stop the catalog from being read — a plugin whose
    /// folder is missing, a path that leaves the repository. Shown so a skill
    /// that is absent from the list is not a mystery.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogPlugin {
    /// Empty for the single group a [`CatalogFormat::SkillFolders`] catalog has.
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub category: Option<String>,
    /// Set when the plugin lives outside this repository. Its skills are not
    /// listed, because finding them means cloning that other repository.
    pub external: Option<ExternalSource>,
    pub skills: Vec<CatalogSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalSource {
    /// The manifest's own word for it: `github`, `url`, `git-subdir`, `npm`, …
    pub kind: String,
    /// A URL the git installer understands, when there is one. `None` for
    /// sources such as npm packages that are not git repositories.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSkill {
    /// Path from the repository root, `/`-separated. This is what installing
    /// resolves, and what an installed skill records as its subpath.
    pub rel_path: String,
    pub name: String,
    pub description: Option<String>,
}

impl Catalog {
    pub fn skill_count(&self) -> usize {
        self.plugins.iter().map(|plugin| plugin.skills.len()).sum()
    }

    pub fn find_skill(&self, rel_path: &str) -> Option<&CatalogSkill> {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.skills.iter())
            .find(|skill| skill.rel_path == rel_path)
    }
}

/// Read the catalog of the marketplace at `root`, which lies inside the
/// checkout `repo_root` (the same directory unless the marketplace was
/// registered at a subfolder of its repository).
///
/// Skill paths are reported relative to `repo_root`, and every path the
/// manifest names is held inside it: a manifest is someone else's file, and
/// `../../..` in it must not reach the rest of the disk.
pub fn read_catalog(repo_root: &Path, root: &Path) -> Result<Catalog> {
    if !root.is_dir() {
        anyhow::bail!("Folder does not exist: {}", root.display());
    }
    if !path_guard::is_path_safe(repo_root, root) {
        anyhow::bail!("The marketplace folder is outside its repository");
    }

    let manifest_path = root.join(MANIFEST_PATH);
    if manifest_path.is_file() {
        match read_manifest(&manifest_path) {
            Ok(manifest) => return Ok(catalog_from_manifest(repo_root, root, manifest)),
            Err(error) => {
                let mut catalog = scan_skill_folders(repo_root, root);
                catalog.warnings.insert(
                    0,
                    format!("{MANIFEST_PATH} could not be read, listing skill folders instead: {error:#}"),
                );
                return Ok(catalog);
            }
        }
    }
    Ok(scan_skill_folders(repo_root, root))
}

// ── Claude Code plugin marketplaces ──

#[derive(Debug, Deserialize)]
struct Manifest {
    name: Option<String>,
    description: Option<String>,
    owner: Option<Value>,
    metadata: Option<ManifestMetadata>,
    #[serde(default)]
    plugins: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct ManifestMetadata {
    description: Option<String>,
    #[serde(rename = "pluginRoot")]
    plugin_root: Option<String>,
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&raw).context("Invalid JSON")
}

fn catalog_from_manifest(repo_root: &Path, root: &Path, manifest: Manifest) -> Catalog {
    let metadata = manifest.metadata.unwrap_or_default();
    let mut warnings = Vec::new();
    let mut plugins = Vec::new();

    for (index, entry) in manifest.plugins.iter().enumerate() {
        let Some(name) = string_field(entry, "name") else {
            warnings.push(format!("Plugin #{} has no name and was skipped", index + 1));
            continue;
        };
        let mut plugin = CatalogPlugin {
            name: name.clone(),
            description: string_field(entry, "description"),
            version: string_field(entry, "version"),
            category: string_field(entry, "category"),
            external: None,
            skills: Vec::new(),
        };

        match entry.get("source") {
            Some(Value::String(source)) => {
                let plugin_dir = resolve_plugin_dir(root, metadata.plugin_root.as_deref(), source);
                if !path_guard::is_path_safe(repo_root, &plugin_dir) {
                    warnings.push(format!(
                        "Plugin '{name}' points outside the repository and was skipped"
                    ));
                    continue;
                }
                if !plugin_dir.is_dir() {
                    warnings.push(format!("Plugin '{name}': folder '{source}' does not exist"));
                } else {
                    plugin.skills =
                        plugin_skills(repo_root, &plugin_dir, entry, &name, &mut warnings);
                    if plugin.description.is_none() || plugin.version.is_none() {
                        let own = read_plugin_manifest(&plugin_dir);
                        plugin.description = plugin
                            .description
                            .or_else(|| own.as_ref().and_then(|m| string_field(m, "description")));
                        plugin.version = plugin
                            .version
                            .or_else(|| own.as_ref().and_then(|m| string_field(m, "version")));
                    }
                }
            }
            Some(source @ Value::Object(_)) => {
                plugin.external = Some(external_source(source));
            }
            _ => {
                warnings.push(format!("Plugin '{name}' has no usable source and was skipped"));
                continue;
            }
        }
        plugins.push(plugin);
    }

    Catalog {
        format: CatalogFormat::ClaudePlugins,
        name: manifest.name.filter(|s| !s.trim().is_empty()),
        description: manifest
            .description
            .or(metadata.description)
            .filter(|s| !s.trim().is_empty()),
        owner: manifest.owner.as_ref().and_then(owner_name),
        plugins,
        warnings,
    }
}

/// `metadata.pluginRoot` lets a manifest write `"source": "formatter"` for
/// `./plugins/formatter`. A source spelled as an explicit relative path is
/// taken as written.
fn resolve_plugin_dir(root: &Path, plugin_root: Option<&str>, source: &str) -> PathBuf {
    let explicit = source.starts_with("./") || source.starts_with("../") || source == ".";
    match plugin_root.filter(|base| !base.trim().is_empty()) {
        Some(base) if !explicit => root.join(base).join(source),
        _ => root.join(source),
    }
}

fn read_plugin_manifest(plugin_dir: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(plugin_dir.join(PLUGIN_MANIFEST_PATH)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// A plugin's skills: the paths its marketplace entry or its own `plugin.json`
/// declares, or its `skills/` folder when neither declares any.
///
/// Declared paths are taken as the plugin's whole list rather than additions
/// to `skills/`. Marketplaces that publish several plugins from one repository
/// root (`"source": "./"`, as `anthropics/skills` does) use the list to say
/// which of the shared skills each plugin is; adding `skills/` back would put
/// every skill in every plugin.
fn plugin_skills(
    repo_root: &Path,
    plugin_dir: &Path,
    entry: &Value,
    plugin_name: &str,
    warnings: &mut Vec<String>,
) -> Vec<CatalogSkill> {
    let own_manifest = read_plugin_manifest(plugin_dir);
    let declared: Vec<String> = [Some(entry), own_manifest.as_ref()]
        .into_iter()
        .flatten()
        .flat_map(|value| path_list(value.get("skills")))
        .collect();
    let mut locations = Vec::new();
    if declared.is_empty() {
        locations.push(plugin_dir.join(DEFAULT_PLUGIN_SKILLS_DIR));
    }
    for declared in declared {
        let location = plugin_dir.join(&declared);
        if !path_guard::is_path_safe(repo_root, &location) {
            warnings.push(format!(
                "Plugin '{plugin_name}': skills path '{declared}' points outside the repository"
            ));
            continue;
        }
        if !location.is_dir() {
            warnings.push(format!(
                "Plugin '{plugin_name}': skills path '{declared}' does not exist"
            ));
            continue;
        }
        locations.push(location);
    }

    let mut seen = HashSet::new();
    let mut skills = Vec::new();
    for location in locations {
        if !location.is_dir() {
            continue;
        }
        for dir in skill_dirs_under(&location) {
            if !path_guard::is_path_safe(repo_root, &dir) {
                continue;
            }
            if let Some(skill) = catalog_skill(repo_root, &dir) {
                if seen.insert(skill.rel_path.clone()) {
                    skills.push(skill);
                }
            }
        }
    }
    sort_skills(&mut skills);
    skills
}

/// A `skills` field is a single path or a list of them.
fn path_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(path)) => vec![path.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn external_source(source: &Value) -> ExternalSource {
    let kind = string_field(source, "source").unwrap_or_else(|| "unknown".to_string());
    let reference = string_field(source, "ref");
    let url = match kind.as_str() {
        "github" => string_field(source, "repo").map(|repo| {
            let base = format!("https://github.com/{}", repo.trim_end_matches(".git"));
            match &reference {
                Some(reference) => format!("{base}/tree/{reference}"),
                None => base,
            }
        }),
        "git-subdir" => string_field(source, "url").map(|url| {
            match (github_web_base(&url), &reference, string_field(source, "path")) {
                (Some(base), Some(reference), Some(path)) => {
                    format!("{base}/tree/{reference}/{}", path.trim_matches('/'))
                }
                // A tree URL needs a ref; without one the whole repository is
                // offered and the preview lists everything in it.
                _ => url,
            }
        }),
        _ => string_field(source, "url"),
    };
    ExternalSource { kind, url }
}

/// `https://github.com/owner/repo` for a GitHub clone URL — HTTPS or SSH —
/// else `None`.
pub fn github_web_base(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let rest = ["https://github.com/", "ssh://git@github.com/", "git@github.com:"]
        .iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}"))
}

fn owner_name(owner: &Value) -> Option<String> {
    match owner {
        Value::String(name) => Some(name.clone()),
        Value::Object(_) => string_field(owner, "name"),
        _ => None,
    }
    .filter(|name| !name.trim().is_empty())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ── Plain skill repositories ──

fn scan_skill_folders(repo_root: &Path, root: &Path) -> Catalog {
    // A skill reached through a symlink as well as directly resolves to the
    // same path; list it once.
    let mut seen = HashSet::new();
    let mut skills: Vec<CatalogSkill> = skill_dirs_under(root)
        .iter()
        .filter(|dir| path_guard::is_path_safe(repo_root, dir))
        .filter_map(|dir| catalog_skill(repo_root, dir))
        .filter(|skill| seen.insert(skill.rel_path.clone()))
        .collect();
    sort_skills(&mut skills);
    let plugins = if skills.is_empty() {
        Vec::new()
    } else {
        vec![CatalogPlugin {
            name: String::new(),
            description: None,
            version: None,
            category: None,
            external: None,
            skills,
        }]
    };
    Catalog {
        format: CatalogFormat::SkillFolders,
        name: None,
        description: None,
        owner: None,
        plugins,
        warnings: Vec::new(),
    }
}

// ── Shared ──

/// `dir` itself when it is a skill, else every skill folder beneath it.
fn skill_dirs_under(dir: &Path) -> Vec<PathBuf> {
    if skill_metadata::is_valid_skill_dir(dir) {
        return vec![dir.to_path_buf()];
    }
    scanner::collect_skill_dirs(dir)
}

fn catalog_skill(repo_root: &Path, dir: &Path) -> Option<CatalogSkill> {
    let rel_path = rel_key(repo_root, dir)?;
    let meta = skill_metadata::parse_skill_md(dir);
    let basename = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| rel_path.clone());
    Some(CatalogSkill {
        name: meta
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(basename),
        description: meta.description.filter(|d| !d.trim().is_empty()),
        rel_path,
    })
}

/// `dir` relative to `repo_root` with `/` separators. `None` for the root
/// itself — a repository that is one single skill has no subpath, and is
/// reported as `"."`.
fn rel_key(repo_root: &Path, dir: &Path) -> Option<String> {
    let base = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let target = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let relative = target.strip_prefix(&base).ok()?;
    let key = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    Some(if key.is_empty() { ".".to_string() } else { key })
}

fn sort_skills(skills: &mut [CatalogSkill]) {
    skills.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
}

/// `root` joined with a `/`-separated catalog path one part at a time, so the
/// result uses the platform's separator throughout.
pub fn join_rel(root: &Path, rel_path: &str) -> PathBuf {
    rel_path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The directory a catalog path names, held inside `repo_root`.
pub fn resolve_skill_path(repo_root: &Path, rel_path: &str) -> Result<PathBuf> {
    let candidate = join_rel(repo_root, rel_path);
    if !path_guard::is_path_safe(repo_root, &candidate) {
        anyhow::bail!("Path '{rel_path}' resolves outside the marketplace");
    }
    if !skill_metadata::is_valid_skill_dir(&candidate) {
        anyhow::bail!("'{rel_path}' is no longer a skill folder — refresh the marketplace");
    }
    Ok(candidate)
}

// ── Marketplaces Claude Code knows about ──

/// A marketplace registered with Claude Code, put the way this app adds one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaudeMarketplace {
    /// Claude Code's name for it — the `@marketplace` in `plugin@marketplace`.
    pub name: String,
    /// The manifest's source kind: `github`, `git`, `directory`, …
    pub kind: String,
    /// A folder path or repository URL to add. `Err` says why there is none.
    pub location: std::result::Result<String, String>,
}

/// Claude Code's configuration folder: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

/// Every marketplace Claude Code has registered.
///
/// `plugins/known_marketplaces.json` is Claude Code's own registry and holds
/// those added with `/plugin marketplace add` as well as the ones declared in
/// settings. `settings.json`'s `extraKnownMarketplaces` adds any it has not
/// fetched yet. A missing file contributes nothing.
pub fn read_claude_marketplaces(config_dir: &Path) -> Result<Vec<ClaudeMarketplace>> {
    let mut found: Vec<ClaudeMarketplace> = Vec::new();
    let registry = config_dir.join("plugins").join("known_marketplaces.json");
    let settings = config_dir.join("settings.json");
    for (path, key) in [(&registry, None), (&settings, Some("extraKnownMarketplaces"))] {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", path.display()))
            }
        };
        let json: Value = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        let entries = match key {
            Some(key) => json.get(key),
            None => Some(&json),
        };
        let Some(Value::Object(entries)) = entries else {
            continue;
        };
        for (name, entry) in entries {
            if found.iter().any(|known| &known.name == name) {
                continue;
            }
            let source = entry.get("source").unwrap_or(&Value::Null);
            found.push(ClaudeMarketplace {
                name: name.clone(),
                kind: string_field(source, "source").unwrap_or_else(|| "unknown".to_string()),
                location: claude_source_location(source),
            });
        }
    }
    found.sort_by_key(|marketplace| marketplace.name.to_lowercase());
    Ok(found)
}

/// Where a Claude Code marketplace source lives, as a folder or a URL the git
/// installer understands.
fn claude_source_location(source: &Value) -> std::result::Result<String, String> {
    let reference = string_field(source, "ref");
    let subpath = string_field(source, "path").map(|path| path.trim_matches('/').to_string());
    match string_field(source, "source").as_deref() {
        Some("github") => {
            let repo = string_field(source, "repo").ok_or("the source names no repository")?;
            github_location(
                &format!("https://github.com/{}", repo.trim_end_matches(".git")),
                reference,
                subpath,
            )
        }
        Some("git") => {
            let url = string_field(source, "url").ok_or("the source names no URL")?;
            match github_web_base(&url) {
                Some(base) => github_location(&base, reference, subpath),
                None if reference.is_none() && subpath.is_none() => Ok(url),
                // Only a GitHub URL can carry a branch or folder here.
                None => Err("a branch or folder on a host other than GitHub".to_string()),
            }
        }
        Some("directory") => string_field(source, "path").ok_or_else(|| "the source names no folder".to_string()),
        Some("file") => {
            let path = string_field(source, "path").ok_or("the source names no file")?;
            let file = Path::new(&path);
            file.parent()
                .filter(|dir| dir.file_name().is_some_and(|name| name == ".claude-plugin"))
                .and_then(Path::parent)
                .map(|root| root.to_string_lossy().to_string())
                .ok_or_else(|| "a manifest file outside a .claude-plugin folder".to_string())
        }
        Some("url") => Err("a marketplace.json served by URL".to_string()),
        Some(other) => Err(format!("a {other} source")),
        None => Err("no source".to_string()),
    }
}

fn github_location(
    base: &str,
    reference: Option<String>,
    subpath: Option<String>,
) -> std::result::Result<String, String> {
    match (reference, subpath.filter(|path| !path.is_empty())) {
        (None, None) => Ok(base.to_string()),
        (Some(reference), None) => Ok(format!("{base}/tree/{reference}")),
        (Some(reference), Some(path)) => Ok(format!("{base}/tree/{reference}/{path}")),
        // A tree URL needs a branch, and guessing the default one could name
        // a branch that does not exist.
        (None, Some(_)) => Err("a folder without a branch".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn skill(dir: &Path, name: &str, description: &str) {
        write(
            &dir.join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\n\nBody\n"),
        );
    }

    fn names(plugin: &CatalogPlugin) -> Vec<(&str, &str)> {
        plugin
            .skills
            .iter()
            .map(|s| (s.name.as_str(), s.rel_path.as_str()))
            .collect()
    }

    #[test]
    fn reads_a_claude_plugin_marketplace() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join(MANIFEST_PATH),
            r#"{
                "name": "team-marketplace",
                "owner": {"name": "Team"},
                "metadata": {"description": "Our skills"},
                "plugins": [
                    {"name": "env-debug", "source": "./plugins/env-debug", "description": "Debug helpers"},
                    {"name": "tools", "source": "./plugins/tools", "skills": ["./extra"]},
                    {"name": "remote", "source": {"source": "github", "repo": "acme/remote", "ref": "v2"}},
                    {"name": "subdir", "source": {"source": "git-subdir", "url": "https://github.com/acme/mono.git", "path": "plugins/x", "ref": "main"}},
                    {"name": "pkg", "source": {"source": "npm", "package": "@acme/pkg"}}
                ]
            }"#,
        );
        skill(&root.join("plugins/env-debug/skills/query-metrics"), "query-metrics", "Query metrics");
        skill(&root.join("plugins/env-debug/skills/investigate"), "investigate", "Investigate");
        // Outside `skills/`, so not a skill of the plugin.
        skill(&root.join("plugins/env-debug/reference/not-a-skill"), "nope", "Nope");
        write(
            &root.join("plugins/tools/.claude-plugin/plugin.json"),
            r#"{"name": "tools", "version": "1.2.0", "description": "From plugin.json"}"#,
        );
        skill(&root.join("plugins/tools/skills/lint"), "lint", "Lint");
        skill(&root.join("plugins/tools/extra"), "extra-skill", "Extra");

        let catalog = read_catalog(root, root).unwrap();
        assert_eq!(catalog.format, CatalogFormat::ClaudePlugins);
        assert_eq!(catalog.name.as_deref(), Some("team-marketplace"));
        assert_eq!(catalog.description.as_deref(), Some("Our skills"));
        assert_eq!(catalog.owner.as_deref(), Some("Team"));
        assert_eq!(catalog.plugins.len(), 5);
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);

        let env = &catalog.plugins[0];
        assert_eq!(
            names(env),
            vec![
                ("investigate", "plugins/env-debug/skills/investigate"),
                ("query-metrics", "plugins/env-debug/skills/query-metrics"),
            ]
        );
        assert_eq!(env.description.as_deref(), Some("Debug helpers"));

        // Declaring `skills` names the plugin's skills; `skills/lint` is not one.
        let tools = &catalog.plugins[1];
        assert_eq!(names(tools), vec![("extra-skill", "plugins/tools/extra")]);
        assert_eq!(tools.description.as_deref(), Some("From plugin.json"));
        assert_eq!(tools.version.as_deref(), Some("1.2.0"));

        let remote = catalog.plugins[2].external.as_ref().unwrap();
        assert_eq!(remote.url.as_deref(), Some("https://github.com/acme/remote/tree/v2"));
        let subdir = catalog.plugins[3].external.as_ref().unwrap();
        assert_eq!(
            subdir.url.as_deref(),
            Some("https://github.com/acme/mono/tree/main/plugins/x")
        );
        let npm = catalog.plugins[4].external.as_ref().unwrap();
        assert_eq!(npm.kind, "npm");
        assert_eq!(npm.url, None);
        assert_eq!(catalog.skill_count(), 3);
    }

    #[test]
    fn plugins_sharing_the_repository_root_list_only_their_own_skills() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join(MANIFEST_PATH),
            r#"{"plugins": [
                {"name": "documents", "source": "./", "strict": false, "skills": ["./skills/docx", "./skills/pdf"]},
                {"name": "examples", "source": "./", "strict": false, "skills": ["./skills/art", "./skills/gone"]}
            ]}"#,
        );
        for name in ["docx", "pdf", "art", "unlisted"] {
            skill(&root.join("skills").join(name), name, name);
        }

        let catalog = read_catalog(root, root).unwrap();
        assert_eq!(
            names(&catalog.plugins[0]),
            vec![("docx", "skills/docx"), ("pdf", "skills/pdf")]
        );
        assert_eq!(names(&catalog.plugins[1]), vec![("art", "skills/art")]);
        assert_eq!(catalog.warnings.len(), 1, "the missing path is reported: {:?}", catalog.warnings);
    }

    #[test]
    fn plugin_root_prefixes_bare_sources() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join(MANIFEST_PATH),
            r#"{"metadata": {"pluginRoot": "./plugins"}, "plugins": [{"name": "fmt", "source": "fmt"}]}"#,
        );
        skill(&root.join("plugins/fmt/skills/format"), "format", "Format");

        let catalog = read_catalog(root, root).unwrap();
        assert_eq!(names(&catalog.plugins[0]), vec![("format", "plugins/fmt/skills/format")]);
    }

    #[test]
    fn manifest_paths_cannot_leave_the_repository() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        skill(&tmp.path().join("outside/skills/secret"), "secret", "Secret");
        write(
            &repo.join(MANIFEST_PATH),
            r#"{"plugins": [
                {"name": "escape", "source": "../outside"},
                {"name": "sneaky", "source": "./p", "skills": ["../../outside/skills"]}
            ]}"#,
        );
        std::fs::create_dir_all(repo.join("p")).unwrap();

        let catalog = read_catalog(&repo, &repo).unwrap();
        assert_eq!(catalog.skill_count(), 0);
        assert_eq!(catalog.plugins.len(), 1, "the escaping plugin is dropped");
        assert_eq!(catalog.warnings.len(), 2, "{:?}", catalog.warnings);
    }

    #[test]
    fn a_repository_without_a_manifest_lists_its_skill_folders() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        skill(&root.join("skills/beta"), "beta", "B");
        skill(&root.join("skills/alpha"), "alpha", "A");
        skill(&root.join("node_modules/pkg"), "ignored", "I");

        let catalog = read_catalog(root, root).unwrap();
        assert_eq!(catalog.format, CatalogFormat::SkillFolders);
        assert_eq!(
            names(&catalog.plugins[0]),
            vec![("alpha", "skills/alpha"), ("beta", "skills/beta")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_is_listed_once() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        skill(&root.join("skills/alpha"), "alpha", "A");
        std::fs::create_dir_all(root.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink("../../skills/alpha", root.join(".claude/skills/alpha")).unwrap();

        let catalog = read_catalog(root, root).unwrap();
        assert_eq!(names(&catalog.plugins[0]), vec![("alpha", "skills/alpha")]);
    }

    #[test]
    fn a_broken_manifest_falls_back_to_skill_folders_with_a_warning() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(&root.join(MANIFEST_PATH), "{ not json");
        skill(&root.join("skills/alpha"), "alpha", "A");

        let catalog = read_catalog(root, root).unwrap();
        assert_eq!(catalog.format, CatalogFormat::SkillFolders);
        assert_eq!(catalog.skill_count(), 1);
        assert_eq!(catalog.warnings.len(), 1);
    }

    #[test]
    fn paths_are_relative_to_the_repository_not_the_marketplace_folder() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        skill(&repo.join("catalog/skills/alpha"), "alpha", "A");

        let catalog = read_catalog(repo, &repo.join("catalog")).unwrap();
        assert_eq!(names(&catalog.plugins[0]), vec![("alpha", "catalog/skills/alpha")]);
        assert!(resolve_skill_path(repo, "catalog/skills/alpha").is_ok());
        assert!(resolve_skill_path(repo, "../elsewhere").is_err());
        assert!(resolve_skill_path(repo, "catalog").is_err(), "not a skill folder");
    }

    #[test]
    fn claude_code_marketplaces_come_from_its_registry_and_settings() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        write(
            &dir.join("plugins/known_marketplaces.json"),
            r#"{
                "team": {"source": {"source": "github", "repo": "acme/team"}, "installLocation": "/x"},
                "pinned": {"source": {"source": "github", "repo": "acme/pinned", "ref": "v2", "path": "catalog"}},
                "mono": {"source": {"source": "git", "url": "git@github.com:acme/mono.git", "ref": "dev"}},
                "local": {"source": {"source": "directory", "path": "/work/market"}},
                "hosted": {"source": {"source": "git", "url": "https://git.example.com/m.git"}}
            }"#,
        );
        write(
            &dir.join("settings.json"),
            r#"{"theme": "dark", "extraKnownMarketplaces": {
                "team": {"source": {"source": "github", "repo": "someone/else"}},
                "pending": {"source": {"source": "file", "path": "/work/p/.claude-plugin/marketplace.json"}},
                "remote-json": {"source": {"source": "url", "url": "https://example.com/marketplace.json"}},
                "loose": {"source": {"source": "github", "repo": "acme/loose", "path": "sub"}}
            }}"#,
        );

        let found = read_claude_marketplaces(dir).unwrap();
        let by_name = |name: &str| found.iter().find(|m| m.name == name).unwrap().location.clone();
        assert_eq!(found.len(), 8);
        // The registry wins over settings for the same name.
        assert_eq!(by_name("team"), Ok("https://github.com/acme/team".to_string()));
        assert_eq!(by_name("pinned"), Ok("https://github.com/acme/pinned/tree/v2/catalog".to_string()));
        assert_eq!(by_name("mono"), Ok("https://github.com/acme/mono/tree/dev".to_string()));
        assert_eq!(by_name("local"), Ok("/work/market".to_string()));
        assert_eq!(by_name("hosted"), Ok("https://git.example.com/m.git".to_string()));
        assert_eq!(by_name("pending"), Ok("/work/p".to_string()));
        assert!(by_name("remote-json").is_err());
        assert!(by_name("loose").is_err());
    }

    #[test]
    fn no_claude_code_configuration_means_no_marketplaces() {
        let tmp = tempdir().unwrap();
        assert!(read_claude_marketplaces(tmp.path()).unwrap().is_empty());
        write(&tmp.path().join("settings.json"), "{ broken");
        assert!(read_claude_marketplaces(tmp.path()).is_err());
    }

    #[test]
    fn a_repository_that_is_one_skill_is_listed_at_its_root() {
        let tmp = tempdir().unwrap();
        skill(tmp.path(), "solo", "Solo");

        let catalog = read_catalog(tmp.path(), tmp.path()).unwrap();
        assert_eq!(names(&catalog.plugins[0]), vec![("solo", ".")]);
        assert!(resolve_skill_path(tmp.path(), ".").is_ok());
    }
}
