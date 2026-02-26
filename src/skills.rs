use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tracing::warn;

const ENV_SKILLS_USER_CONFIG_OVERRIDE: &str = "XIAOMAOLV_SKILLS_USER_CONFIG";
const ENV_SKILLS_PROJECT_CONFIG_OVERRIDE: &str = "XIAOMAOLV_SKILLS_PROJECT_CONFIG";
const ENV_SKILLS_USER_DIR_OVERRIDE: &str = "XIAOMAOLV_SKILLS_USER_DIR";
const ENV_SKILLS_PROJECT_DIR_OVERRIDE: &str = "XIAOMAOLV_SKILLS_PROJECT_DIR";
const ENV_SKILLS_TEAM_INDEX_URL: &str = "XIAOMAOLV_SKILLS_TEAM_INDEX_URL";
const OFFICIAL_CATALOG_API_URL: &str =
    "https://api.github.com/repos/openai/skills/contents/skills/.curated";
const OFFICIAL_CATALOG_RAW_BASE: &str =
    "https://raw.githubusercontent.com/openai/skills/main/skills/.curated";
const MAX_SKILL_FILES: usize = 256;
const MAX_SKILL_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SKILL_MD_BYTES: u64 = 200 * 1024;
const MAX_SKILL_SNIPPET_CHARS: usize = 2500;
const DEFAULT_USER_AGENT: &str = "xiaomaolv-skills";
const ENV_GITHUB_TOKEN: &str = "GITHUB_TOKEN";
const ENV_GH_TOKEN: &str = "GH_TOKEN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillActivationMode {
    Semantic,
    Always,
    Off,
}

impl std::fmt::Display for SkillActivationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Semantic => write!(f, "semantic"),
            Self::Always => write!(f, "always"),
            Self::Off => write!(f, "off"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSourceKind {
    Local,
    Catalog,
}

impl std::fmt::Display for SkillSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::Catalog => write!(f, "catalog"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillListScope {
    User,
    Project,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillRemoveScope {
    User,
    Project,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillCatalogIndex {
    Official,
    Team,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillRerankMode {
    None,
    Llm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstalledConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_mode")]
    pub mode: SkillActivationMode,
    pub source_kind: SkillSourceKind,
    pub source_ref: String,
    pub version: String,
    pub checksum_sha256: String,
    pub installed_at_unix: i64,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillsConfigFile {
    #[serde(default)]
    pub skills: HashMap<String, SkillInstalledConfig>,
}

#[derive(Debug, Clone)]
pub struct SkillConfigPaths {
    pub user_config: PathBuf,
    pub project_config: PathBuf,
    pub user_dir: PathBuf,
    pub project_dir: PathBuf,
}

impl SkillConfigPaths {
    pub fn discover(cwd: &Path) -> anyhow::Result<Self> {
        let user_config = if let Ok(override_path) = std::env::var(ENV_SKILLS_USER_CONFIG_OVERRIDE)
        {
            PathBuf::from(override_path)
        } else {
            let base = dirs::config_dir().context("cannot resolve user config directory")?;
            base.join("xiaomaolv").join("skills.toml")
        };

        let project_config =
            if let Ok(override_path) = std::env::var(ENV_SKILLS_PROJECT_CONFIG_OVERRIDE) {
                PathBuf::from(override_path)
            } else {
                cwd.join(".xiaomaolv").join("skills.toml")
            };

        let user_dir = if let Ok(override_path) = std::env::var(ENV_SKILLS_USER_DIR_OVERRIDE) {
            PathBuf::from(override_path)
        } else {
            let base = dirs::data_dir()
                .or_else(dirs::config_dir)
                .context("cannot resolve user data directory")?;
            base.join("xiaomaolv").join("skills")
        };

        let project_dir = if let Ok(override_path) = std::env::var(ENV_SKILLS_PROJECT_DIR_OVERRIDE)
        {
            PathBuf::from(override_path)
        } else {
            cwd.join(".xiaomaolv").join("skills")
        };

        Ok(Self {
            user_config,
            project_config,
            user_dir,
            project_dir,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillView {
    pub id: String,
    pub source: String,
    pub enabled: bool,
    pub mode: String,
    pub source_kind: String,
    pub source_ref: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub version: String,
    pub source_ref: String,
    pub download_url: String,
    pub checksum_sha256: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub catalog: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSearchHit {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub score: f32,
    pub source_ref: String,
    pub catalog: String,
}

#[derive(Debug, Clone)]
pub struct SkillInstallResult {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct SkillUpdateResult {
    pub touched: usize,
    pub updated: usize,
    pub up_to_date: usize,
    pub skipped_local: usize,
    pub missing: usize,
}

pub struct SkillRegistry {
    paths: SkillConfigPaths,
    http: reqwest::Client,
}

impl SkillRegistry {
    pub fn new(paths: SkillConfigPaths) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!(
                "{DEFAULT_USER_AGENT}/{}",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .context("failed to build skills http client")?;
        Ok(Self { paths, http })
    }

    pub async fn list_skills(&self, scope: SkillListScope) -> anyhow::Result<Vec<SkillView>> {
        match scope {
            SkillListScope::User => {
                let file = self.load_scope(SkillScope::User).await?;
                Ok(file
                    .skills
                    .into_iter()
                    .map(|(id, cfg)| view_from_config(id, "user", cfg))
                    .collect())
            }
            SkillListScope::Project => {
                let file = self.load_scope(SkillScope::Project).await?;
                Ok(file
                    .skills
                    .into_iter()
                    .map(|(id, cfg)| view_from_config(id, "project", cfg))
                    .collect())
            }
            SkillListScope::Merged => {
                let merged = self.load_merged_with_source().await?;
                Ok(merged
                    .into_iter()
                    .map(|(id, (source, cfg))| view_from_config(id, source, cfg))
                    .collect())
            }
        }
    }

    pub async fn load_merged(&self) -> anyhow::Result<HashMap<String, SkillInstalledConfig>> {
        Ok(self
            .load_merged_with_source()
            .await?
            .into_iter()
            .map(|(id, (_, cfg))| (id, cfg))
            .collect())
    }

    pub async fn load_merged_with_source(
        &self,
    ) -> anyhow::Result<HashMap<String, (&'static str, SkillInstalledConfig)>> {
        let mut merged: HashMap<String, (&'static str, SkillInstalledConfig)> = self
            .load_scope(SkillScope::User)
            .await?
            .skills
            .into_iter()
            .map(|(id, cfg)| (id, ("user", cfg)))
            .collect();
        for (id, cfg) in self.load_scope(SkillScope::Project).await?.skills {
            merged.insert(id, ("project", cfg));
        }
        Ok(merged)
    }

    pub async fn install_local_skill(
        &self,
        scope: SkillScope,
        source_dir: &Path,
        explicit_id: Option<&str>,
        mode: SkillActivationMode,
    ) -> anyhow::Result<SkillInstallResult> {
        let source_dir = source_dir
            .canonicalize()
            .with_context(|| format!("failed to resolve skill path {}", source_dir.display()))?;
        if !source_dir.is_dir() {
            bail!("skill path '{}' is not a directory", source_dir.display());
        }
        let scan = scan_local_skill_source(&source_dir)?;
        let derived_id = explicit_id.map(str::to_string).unwrap_or_else(|| {
            source_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "skill".to_string())
        });
        let skill_id = normalize_skill_id(&derived_id)?;
        let dest_dir = self.skill_dir_for(scope, &skill_id);
        let source_ref = source_dir.to_string_lossy().to_string();
        let meta = parse_skill_markdown_metadata(&scan.skill_md_content, &skill_id);
        let checksum = sha256_hex(scan.skill_md_content.as_bytes());
        let now = current_unix_timestamp_i64();
        let version = format!("local-{now}");

        self.write_skill_tree(&dest_dir, &scan.files).await?;
        self.upsert_skill_record(
            scope,
            &skill_id,
            SkillInstalledConfig {
                enabled: mode != SkillActivationMode::Off,
                mode,
                source_kind: SkillSourceKind::Local,
                source_ref,
                version: version.clone(),
                checksum_sha256: checksum,
                installed_at_unix: now,
                name: meta.name,
                description: meta.description,
                tags: meta.tags,
            },
        )
        .await?;

        Ok(SkillInstallResult {
            id: skill_id,
            version,
        })
    }

    pub async fn install_catalog_skill(
        &self,
        scope: SkillScope,
        entry: &SkillCatalogEntry,
        mode: SkillActivationMode,
    ) -> anyhow::Result<SkillInstallResult> {
        self.install_catalog_skill_with_override(scope, entry, Some(mode), None)
            .await
    }

    async fn install_catalog_skill_with_override(
        &self,
        scope: SkillScope,
        entry: &SkillCatalogEntry,
        mode_override: Option<SkillActivationMode>,
        enabled_override: Option<bool>,
    ) -> anyhow::Result<SkillInstallResult> {
        let skill_id = normalize_skill_id(&entry.id)?;
        let markdown = self.download_catalog_skill(entry).await?;
        let checksum = sha256_hex(markdown.as_bytes());
        if let Some(expect) = &entry.checksum_sha256
            && !expect.trim().is_empty()
            && !expect.eq_ignore_ascii_case(&checksum)
        {
            bail!("catalog checksum mismatch for skill '{}'", skill_id);
        }
        if markdown.len() as u64 > MAX_SKILL_MD_BYTES {
            bail!(
                "catalog skill '{}' SKILL.md exceeds {} bytes",
                skill_id,
                MAX_SKILL_MD_BYTES
            );
        }

        let dest_dir = self.skill_dir_for(scope, &skill_id);
        self.write_catalog_skill_dir(&dest_dir, &markdown).await?;

        let parsed = parse_skill_markdown_metadata(&markdown, &skill_id);
        let now = current_unix_timestamp_i64();

        let installed = SkillInstalledConfig {
            enabled: enabled_override.unwrap_or_else(|| {
                mode_override.unwrap_or(SkillActivationMode::Semantic) != SkillActivationMode::Off
            }),
            mode: mode_override.unwrap_or(SkillActivationMode::Semantic),
            source_kind: SkillSourceKind::Catalog,
            source_ref: entry.source_ref.clone(),
            version: entry.version.clone(),
            checksum_sha256: checksum,
            installed_at_unix: now,
            name: if parsed.name.is_empty() {
                entry.name.clone()
            } else {
                parsed.name
            },
            description: if parsed.description.is_empty() {
                entry.description.clone()
            } else {
                parsed.description
            },
            tags: if parsed.tags.is_empty() {
                entry.tags.clone()
            } else {
                parsed.tags
            },
        };

        self.upsert_skill_record(scope, &skill_id, installed)
            .await?;
        Ok(SkillInstallResult {
            id: skill_id,
            version: entry.version.clone(),
        })
    }

    pub async fn remove_skill(&self, scope: SkillRemoveScope, id: &str) -> anyhow::Result<bool> {
        let id = normalize_skill_id(id)?;
        match scope {
            SkillRemoveScope::User => self.remove_skill_in_scope(SkillScope::User, &id).await,
            SkillRemoveScope::Project => self.remove_skill_in_scope(SkillScope::Project, &id).await,
            SkillRemoveScope::All => {
                let user = self.remove_skill_in_scope(SkillScope::User, &id).await?;
                let project = self.remove_skill_in_scope(SkillScope::Project, &id).await?;
                Ok(user || project)
            }
        }
    }

    pub async fn set_skill_mode(
        &self,
        scope: SkillRemoveScope,
        id: &str,
        mode: SkillActivationMode,
    ) -> anyhow::Result<usize> {
        let id = normalize_skill_id(id)?;
        let mut changed = 0usize;
        for target in scopes_from_remove_scope(scope) {
            let mut file = self.load_scope(target).await?;
            if let Some(cfg) = file.skills.get_mut(&id) {
                cfg.mode = mode;
                cfg.enabled = mode != SkillActivationMode::Off;
                changed += 1;
                self.save_scope(target, &file).await?;
            }
        }
        Ok(changed)
    }

    pub async fn update_skill(
        &self,
        scope: SkillRemoveScope,
        id: &str,
        to_version: Option<&str>,
        latest: bool,
        index: SkillCatalogIndex,
    ) -> anyhow::Result<SkillUpdateResult> {
        if to_version.is_some() && latest {
            bail!("use either --to-version or --latest, not both");
        }

        let id = normalize_skill_id(id)?;
        let mut out = SkillUpdateResult {
            touched: 0,
            updated: 0,
            up_to_date: 0,
            skipped_local: 0,
            missing: 0,
        };

        for target in scopes_from_remove_scope(scope) {
            let file = self.load_scope(target).await?;
            let Some(installed) = file.skills.get(&id).cloned() else {
                out.missing += 1;
                continue;
            };
            out.touched += 1;
            if installed.source_kind != SkillSourceKind::Catalog {
                out.skipped_local += 1;
                continue;
            }

            let mut selected_index = index;
            if installed.source_ref.starts_with("openai/skills:") {
                selected_index = SkillCatalogIndex::Official;
            } else if installed.source_ref.starts_with("team:") {
                selected_index = SkillCatalogIndex::Team;
            }

            let entries = self.load_catalog_entries(selected_index).await?;
            let mut entry = entries
                .into_iter()
                .find(|v| v.id.eq_ignore_ascii_case(&id))
                .with_context(|| format!("catalog entry '{}' not found", id))?;

            if let Some(expect) = to_version
                && !entry.version.eq_ignore_ascii_case(expect)
            {
                bail!(
                    "catalog version mismatch for '{}': requested {}, got {}",
                    id,
                    expect,
                    entry.version
                );
            }

            if !latest
                && to_version.is_none()
                && installed.version == entry.version
                && installed
                    .checksum_sha256
                    .eq_ignore_ascii_case(entry.checksum_sha256.as_deref().unwrap_or_default())
            {
                out.up_to_date += 1;
                continue;
            }

            if to_version.is_none()
                && !latest
                && installed.version == entry.version
                && entry.checksum_sha256.is_none()
            {
                let markdown = self.download_catalog_skill(&entry).await?;
                let next_checksum = sha256_hex(markdown.as_bytes());
                if installed
                    .checksum_sha256
                    .eq_ignore_ascii_case(&next_checksum)
                {
                    out.up_to_date += 1;
                    continue;
                }
                entry.checksum_sha256 = Some(next_checksum);
            }

            let mode = installed.mode;
            let enabled = installed.enabled;
            self.install_catalog_skill_with_override(target, &entry, Some(mode), Some(enabled))
                .await?;
            out.updated += 1;
        }

        Ok(out)
    }

    pub async fn find_catalog_entry_exact(
        &self,
        id: &str,
        index: SkillCatalogIndex,
    ) -> anyhow::Result<Option<SkillCatalogEntry>> {
        let normalized = normalize_skill_id(id)?;

        if matches!(index, SkillCatalogIndex::Official) {
            return self.probe_official_entry_by_id(&normalized).await;
        }

        if matches!(index, SkillCatalogIndex::All)
            && let Some(entry) = self.probe_official_entry_by_id(&normalized).await?
        {
            return Ok(Some(entry));
        }

        let entries = match self.load_catalog_entries(index).await {
            Ok(entries) => entries,
            Err(err) => {
                warn!(
                    error = %err,
                    index = ?index,
                    "failed to load skill catalog for exact match, falling back to direct probe"
                );
                vec![]
            }
        };
        Ok(entries
            .into_iter()
            .find(|entry| entry.id.eq_ignore_ascii_case(&normalized)))
    }

    pub async fn search_catalog(
        &self,
        query: &str,
        top: usize,
        index: SkillCatalogIndex,
        _rerank: SkillRerankMode,
    ) -> anyhow::Result<Vec<SkillSearchHit>> {
        let normalized_query = query.trim().to_ascii_lowercase();
        if normalized_query.is_empty() {
            bail!("search query is empty");
        }
        let query_tokens = tokenize_text(&normalized_query);
        let entries = self.load_catalog_entries(index).await?;
        let mut hits = entries
            .into_iter()
            .map(|entry| {
                let score = score_catalog_entry(&normalized_query, &query_tokens, &entry);
                SkillSearchHit {
                    id: entry.id,
                    name: entry.name,
                    description: entry.description,
                    version: entry.version,
                    score,
                    source_ref: entry.source_ref,
                    catalog: entry.catalog,
                }
            })
            .filter(|hit| hit.score > 0.0)
            .collect::<Vec<_>>();

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        hits.truncate(top.max(1));
        Ok(hits)
    }

    pub fn skill_dir_for(&self, scope: SkillScope, id: &str) -> PathBuf {
        match scope {
            SkillScope::User => self.paths.user_dir.join(id),
            SkillScope::Project => self.paths.project_dir.join(id),
        }
    }

    async fn remove_skill_in_scope(&self, scope: SkillScope, id: &str) -> anyhow::Result<bool> {
        let mut file = self.load_scope(scope).await?;
        let existed = file.skills.remove(id).is_some();
        if !existed {
            return Ok(false);
        }
        self.save_scope(scope, &file).await?;

        let dir = self.skill_dir_for(scope, id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .await
                .with_context(|| format!("failed to remove skill dir {}", dir.display()))?;
        }
        Ok(true)
    }

    async fn upsert_skill_record(
        &self,
        scope: SkillScope,
        id: &str,
        record: SkillInstalledConfig,
    ) -> anyhow::Result<()> {
        let mut file = self.load_scope(scope).await?;
        file.skills.insert(id.to_string(), record);
        self.save_scope(scope, &file).await
    }

    async fn load_scope(&self, scope: SkillScope) -> anyhow::Result<SkillsConfigFile> {
        let path = self.path_for(scope);
        if !path.exists() {
            return Ok(SkillsConfigFile::default());
        }
        let content = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read skills config {}", path.display()))?;
        let cfg: SkillsConfigFile = toml::from_str(&content)
            .with_context(|| format!("failed to parse skills config {}", path.display()))?;
        Ok(cfg)
    }

    async fn save_scope(&self, scope: SkillScope, file: &SkillsConfigFile) -> anyhow::Result<()> {
        let path = self.path_for(scope);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create skills config dir {}", parent.display())
            })?;
        }
        let content = toml::to_string_pretty(file).context("failed to serialize skills config")?;
        fs::write(&path, content)
            .await
            .with_context(|| format!("failed to write skills config {}", path.display()))?;
        Ok(())
    }

    fn path_for(&self, scope: SkillScope) -> PathBuf {
        match scope {
            SkillScope::User => self.paths.user_config.clone(),
            SkillScope::Project => self.paths.project_config.clone(),
        }
    }

    async fn write_skill_tree(
        &self,
        dest_dir: &Path,
        files: &[ScannedLocalFile],
    ) -> anyhow::Result<()> {
        let parent = dest_dir
            .parent()
            .with_context(|| format!("invalid destination dir {}", dest_dir.display()))?;
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create dir {}", parent.display()))?;

        let tmp_dir = parent.join(format!(
            ".tmp-{}-{}",
            dest_dir
                .file_name()
                .map(|v| v.to_string_lossy())
                .unwrap_or_else(|| std::borrow::Cow::Borrowed("skill")),
            current_unix_timestamp_i64()
        ));
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir).await.ok();
        }
        fs::create_dir_all(&tmp_dir)
            .await
            .with_context(|| format!("failed to create tmp dir {}", tmp_dir.display()))?;

        for file in files {
            let target = tmp_dir.join(&file.relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).await.with_context(|| {
                    format!("failed to create skill subdir {}", parent.display())
                })?;
            }
            fs::copy(&file.absolute, &target).await.with_context(|| {
                format!(
                    "failed to copy '{}' to '{}'",
                    file.absolute.display(),
                    target.display()
                )
            })?;
        }

        if dest_dir.exists() {
            fs::remove_dir_all(dest_dir).await.with_context(|| {
                format!(
                    "failed to remove existing skill dir '{}'",
                    dest_dir.display()
                )
            })?;
        }
        fs::rename(&tmp_dir, dest_dir).await.with_context(|| {
            format!(
                "failed to move tmp skill dir '{}' to '{}'",
                tmp_dir.display(),
                dest_dir.display()
            )
        })?;
        Ok(())
    }

    async fn write_catalog_skill_dir(&self, dest_dir: &Path, markdown: &str) -> anyhow::Result<()> {
        let parent = dest_dir
            .parent()
            .with_context(|| format!("invalid destination dir {}", dest_dir.display()))?;
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create dir {}", parent.display()))?;
        let tmp_dir = parent.join(format!(
            ".tmp-{}-{}",
            dest_dir
                .file_name()
                .map(|v| v.to_string_lossy())
                .unwrap_or_else(|| std::borrow::Cow::Borrowed("skill")),
            current_unix_timestamp_i64()
        ));
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir).await.ok();
        }
        fs::create_dir_all(&tmp_dir)
            .await
            .with_context(|| format!("failed to create tmp dir {}", tmp_dir.display()))?;
        fs::write(tmp_dir.join("SKILL.md"), markdown)
            .await
            .with_context(|| format!("failed to write catalog skill {}", tmp_dir.display()))?;
        if dest_dir.exists() {
            fs::remove_dir_all(dest_dir).await.with_context(|| {
                format!(
                    "failed to remove existing skill dir '{}'",
                    dest_dir.display()
                )
            })?;
        }
        fs::rename(&tmp_dir, dest_dir).await.with_context(|| {
            format!(
                "failed to move tmp skill dir '{}' to '{}'",
                tmp_dir.display(),
                dest_dir.display()
            )
        })?;
        Ok(())
    }

    async fn download_catalog_skill(&self, entry: &SkillCatalogEntry) -> anyhow::Result<String> {
        let response = self
            .http
            .get(&entry.download_url)
            .send()
            .await
            .with_context(|| format!("failed to fetch skill '{}'", entry.id))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to decode skill download body")?;
        if !status.is_success() {
            bail!("download skill '{}' failed: {} {}", entry.id, status, body);
        }
        Ok(body)
    }

    async fn load_catalog_entries(
        &self,
        index: SkillCatalogIndex,
    ) -> anyhow::Result<Vec<SkillCatalogEntry>> {
        let mut all = Vec::new();
        match index {
            SkillCatalogIndex::Official => {
                all.extend(self.load_official_catalog().await?);
            }
            SkillCatalogIndex::Team => {
                all.extend(self.load_team_catalog().await?);
            }
            SkillCatalogIndex::All => {
                let mut any_ok = false;
                match self.load_official_catalog().await {
                    Ok(entries) => {
                        any_ok = true;
                        all.extend(entries);
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to load official skill catalog");
                    }
                }
                match self.load_team_catalog().await {
                    Ok(entries) => {
                        any_ok = true;
                        all.extend(entries);
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to load team skill catalog");
                    }
                }
                if !any_ok {
                    bail!("failed to load all skill catalogs");
                }
            }
        }
        Ok(all)
    }

    async fn load_official_catalog(&self) -> anyhow::Result<Vec<SkillCatalogEntry>> {
        #[derive(Debug, Deserialize)]
        struct GithubContentItem {
            name: String,
            #[serde(rename = "type")]
            item_type: String,
        }

        let response = with_github_auth(
            self.http
                .get(OFFICIAL_CATALOG_API_URL)
                .header("accept", "application/vnd.github+json"),
        )
        .send()
        .await
        .context("failed to fetch official catalog")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to decode official catalog response")?;
        if !status.is_success() {
            bail!("official catalog fetch failed: {} {}", status, body);
        }
        let items: Vec<GithubContentItem> =
            serde_json::from_str(&body).context("failed to decode official catalog json")?;

        let mut out = Vec::new();
        for item in items {
            if item.item_type != "dir" {
                continue;
            }
            let id = normalize_skill_id(&item.name)?;
            let tags = id
                .split(['-', '_', '.'])
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string())
                .collect::<Vec<_>>();
            let description = format!("Curated skill '{}' from openai/skills.", id);
            let source_ref = format!("openai/skills:skills/.curated/{id}@main");
            let download_url = format!("{OFFICIAL_CATALOG_RAW_BASE}/{id}/SKILL.md");
            let mut keywords = tokenize_text(&id).into_iter().collect::<Vec<_>>();
            keywords.extend(tokenize_text(&description));
            out.push(SkillCatalogEntry {
                id: id.clone(),
                name: id.clone(),
                description,
                tags,
                version: "main".to_string(),
                source_ref,
                download_url,
                checksum_sha256: None,
                keywords,
                catalog: "official".to_string(),
            });
        }
        Ok(out)
    }

    async fn load_team_catalog(&self) -> anyhow::Result<Vec<SkillCatalogEntry>> {
        #[derive(Debug, Deserialize)]
        struct TeamCatalogPayload {
            #[serde(default)]
            skills: Vec<TeamCatalogSkill>,
        }

        #[derive(Debug, Deserialize)]
        struct TeamCatalogSkill {
            id: String,
            name: Option<String>,
            description: Option<String>,
            #[serde(default)]
            tags: Vec<String>,
            version: Option<String>,
            source_ref: Option<String>,
            download_url: Option<String>,
            checksum_sha256: Option<String>,
            #[serde(default)]
            keywords: Vec<String>,
        }

        let Ok(url) = std::env::var(ENV_SKILLS_TEAM_INDEX_URL) else {
            return Ok(vec![]);
        };
        let url = url.trim();
        if url.is_empty() {
            return Ok(vec![]);
        }

        let response = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("failed to fetch team catalog from {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to decode team catalog response")?;
        if !status.is_success() {
            bail!("team catalog fetch failed: {} {}", status, body);
        }
        let payload: TeamCatalogPayload =
            serde_json::from_str(&body).context("failed to decode team catalog json")?;
        let mut out = Vec::new();
        for raw in payload.skills {
            let id = normalize_skill_id(&raw.id)?;
            let Some(download_url) = raw.download_url.filter(|v| !v.trim().is_empty()) else {
                continue;
            };
            let name = raw
                .name
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(id.clone());
            let description = raw.description.unwrap_or_default();
            let version = raw.version.unwrap_or_else(|| "latest".to_string());
            let source_ref = raw
                .source_ref
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| format!("team:{id}@{version}"));
            let mut keywords = raw.keywords;
            keywords.extend(tokenize_text(&name));
            keywords.extend(tokenize_text(&description));
            out.push(SkillCatalogEntry {
                id,
                name,
                description,
                tags: raw.tags,
                version,
                source_ref,
                download_url,
                checksum_sha256: raw.checksum_sha256,
                keywords,
                catalog: "team".to_string(),
            });
        }
        Ok(out)
    }

    async fn probe_official_entry_by_id(
        &self,
        normalized_id: &str,
    ) -> anyhow::Result<Option<SkillCatalogEntry>> {
        let download_url = format!("{OFFICIAL_CATALOG_RAW_BASE}/{normalized_id}/SKILL.md");
        let response = with_github_auth(self.http.get(&download_url))
            .send()
            .await
            .with_context(|| format!("failed to probe official skill '{}'", normalized_id))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response
            .text()
            .await
            .context("failed to decode official skill probe response")?;
        if !status.is_success() {
            bail!(
                "official skill probe failed for '{}': {} {}",
                normalized_id,
                status,
                body
            );
        }
        if body.trim().is_empty() {
            return Ok(None);
        }
        if body.len() as u64 > MAX_SKILL_MD_BYTES {
            bail!(
                "official skill '{}' SKILL.md exceeds {} bytes",
                normalized_id,
                MAX_SKILL_MD_BYTES
            );
        }

        let meta = parse_skill_markdown_metadata(&body, normalized_id);
        let mut keywords = tokenize_text(normalized_id).into_iter().collect::<Vec<_>>();
        keywords.extend(tokenize_text(&meta.name));
        keywords.extend(tokenize_text(&meta.description));

        Ok(Some(SkillCatalogEntry {
            id: normalized_id.to_string(),
            name: meta.name,
            description: meta.description,
            tags: meta.tags,
            version: "main".to_string(),
            source_ref: format!("openai/skills:skills/.curated/{normalized_id}@main"),
            download_url,
            checksum_sha256: Some(sha256_hex(body.as_bytes())),
            keywords,
            catalog: "official".to_string(),
        }))
    }
}

fn github_token_from_env() -> Option<String> {
    std::env::var(ENV_GITHUB_TOKEN)
        .ok()
        .or_else(|| std::env::var(ENV_GH_TOKEN).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn with_github_auth(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if let Some(token) = github_token_from_env() {
        builder.bearer_auth(token)
    } else {
        builder
    }
}

#[derive(Debug, Clone)]
pub struct SkillRuntimeSelectionSettings {
    pub max_selected: usize,
    pub max_prompt_chars: usize,
    pub match_min_score: f32,
}

#[derive(Debug, Clone)]
struct SkillRuntimeEntry {
    id: String,
    enabled: bool,
    mode: SkillActivationMode,
    name: String,
    description: String,
    tags: Vec<String>,
    content: String,
    token_set: HashSet<String>,
}

#[derive(Clone, Default)]
pub struct SkillRuntime {
    skills: Vec<SkillRuntimeEntry>,
}

impl SkillRuntime {
    fn new(skills: Vec<SkillRuntimeEntry>) -> Self {
        Self { skills }
    }

    pub async fn from_registry(registry: &SkillRegistry) -> anyhow::Result<Self> {
        let merged = registry.load_merged_with_source().await?;
        let mut runtime_skills = Vec::new();
        for (id, (source, cfg)) in merged {
            let scope = if source == "project" {
                SkillScope::Project
            } else {
                SkillScope::User
            };
            let dir = registry.skill_dir_for(scope, &id);
            let path = dir.join("SKILL.md");
            if !path.exists() {
                continue;
            }
            let content = fs::read_to_string(&path)
                .await
                .with_context(|| format!("failed to read skill content {}", path.display()))?;
            let token_set =
                build_skill_token_set(&id, &cfg.name, &cfg.description, &cfg.tags, &content);
            runtime_skills.push(SkillRuntimeEntry {
                id,
                enabled: cfg.enabled,
                mode: cfg.mode,
                name: cfg.name,
                description: cfg.description,
                tags: cfg.tags,
                content,
                token_set,
            });
        }
        Ok(Self::new(runtime_skills))
    }

    pub fn build_system_prompt(
        &self,
        query: &str,
        settings: &SkillRuntimeSelectionSettings,
    ) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let query_tokens = tokenize_text(query);
        let mut candidates = self
            .skills
            .iter()
            .filter(|skill| skill.enabled && skill.mode != SkillActivationMode::Off)
            .filter_map(|skill| {
                let score = match skill.mode {
                    SkillActivationMode::Always => 2.0,
                    SkillActivationMode::Semantic => {
                        semantic_skill_runtime_score(&query_tokens, skill)
                    }
                    SkillActivationMode::Off => 0.0,
                };
                let include =
                    skill.mode == SkillActivationMode::Always || score >= settings.match_min_score;
                include.then_some((skill, score))
            })
            .collect::<Vec<_>>();

        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by(|a, b| {
            let a_always = a.0.mode == SkillActivationMode::Always;
            let b_always = b.0.mode == SkillActivationMode::Always;
            b_always
                .cmp(&a_always)
                .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal))
                .then_with(|| a.0.id.cmp(&b.0.id))
        });

        let max_selected = settings.max_selected.max(1);
        let mut selected = candidates
            .into_iter()
            .take(max_selected)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return None;
        }

        let mut out = String::from(
            "SKILLS_CONTEXT:\nFollow these installed skill instructions when they are relevant to the user request.\n",
        );
        let mut budget = settings.max_prompt_chars.max(256);
        if out.chars().count() >= budget {
            return None;
        }
        budget -= out.chars().count();

        for (idx, (skill, score)) in selected.drain(..).enumerate() {
            if budget < 64 {
                break;
            }
            let snippet = truncate_chars(&skill.content, MAX_SKILL_SNIPPET_CHARS);
            let section = format!(
                "\n[Skill {n}]\n- id: {id}\n- mode: {mode}\n- score: {score:.3}\n- name: {name}\n- description: {description}\n- tags: {tags}\nInstructions:\n{body}\n",
                n = idx + 1,
                id = skill.id,
                mode = skill.mode,
                score = score,
                name = skill.name,
                description = skill.description,
                tags = skill.tags.join(","),
                body = snippet
            );
            let section_len = section.chars().count();
            if section_len > budget {
                break;
            }
            out.push_str(&section);
            budget -= section_len;
        }

        (out.trim().len() > "SKILLS_CONTEXT:".len()).then_some(out)
    }
}

#[derive(Debug, Clone)]
struct ScannedLocalFile {
    absolute: PathBuf,
    relative: PathBuf,
}

#[derive(Debug, Clone)]
struct LocalSkillScan {
    files: Vec<ScannedLocalFile>,
    skill_md_content: String,
}

#[derive(Debug, Clone)]
struct ParsedSkillMetadata {
    name: String,
    description: String,
    tags: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

fn default_mode() -> SkillActivationMode {
    SkillActivationMode::Semantic
}

fn view_from_config(id: String, source: &str, cfg: SkillInstalledConfig) -> SkillView {
    SkillView {
        id,
        source: source.to_string(),
        enabled: cfg.enabled,
        mode: cfg.mode.to_string(),
        source_kind: cfg.source_kind.to_string(),
        source_ref: cfg.source_ref,
        version: cfg.version,
        name: cfg.name,
        description: cfg.description,
        tags: cfg.tags,
    }
}

fn normalize_skill_id(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        bail!("skill id is empty");
    }
    let mut out = String::new();
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        bail!("skill id '{}' has no valid characters", raw);
    }
    Ok(out)
}

fn scopes_from_remove_scope(scope: SkillRemoveScope) -> Vec<SkillScope> {
    match scope {
        SkillRemoveScope::User => vec![SkillScope::User],
        SkillRemoveScope::Project => vec![SkillScope::Project],
        SkillRemoveScope::All => vec![SkillScope::User, SkillScope::Project],
    }
}

fn scan_local_skill_source(root: &Path) -> anyhow::Result<LocalSkillScan> {
    if !root.is_dir() {
        bail!("'{}' is not a directory", root.display());
    }
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    scan_local_skill_dir(root, root, &mut files, &mut total_bytes)?;
    if files.len() > MAX_SKILL_FILES {
        bail!("skill package has too many files (> {})", MAX_SKILL_FILES);
    }
    if total_bytes > MAX_SKILL_BYTES {
        bail!("skill package exceeds {} bytes", MAX_SKILL_BYTES);
    }

    let skill_md = files
        .iter()
        .find(|f| f.relative == Path::new("SKILL.md"))
        .with_context(|| format!("missing SKILL.md in '{}'", root.display()))?;
    let metadata = std::fs::metadata(&skill_md.absolute).with_context(|| {
        format!(
            "failed to stat skill metadata file '{}'",
            skill_md.absolute.display()
        )
    })?;
    if metadata.len() > MAX_SKILL_MD_BYTES {
        bail!("SKILL.md exceeds {} bytes", MAX_SKILL_MD_BYTES);
    }
    let skill_md_content = std::fs::read_to_string(&skill_md.absolute)
        .with_context(|| format!("failed to read '{}'", skill_md.absolute.display()))?;
    Ok(LocalSkillScan {
        files,
        skill_md_content,
    })
}

fn scan_local_skill_dir(
    root: &Path,
    current: &Path,
    files: &mut Vec<ScannedLocalFile>,
    total_bytes: &mut u64,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(current)
        .with_context(|| format!("failed to read dir '{}'", current.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read dir entry in '{}'", current.display()))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat '{}'", path.display()))?;
        if meta.file_type().is_symlink() {
            bail!(
                "symlink is not allowed in skill package: {}",
                path.display()
            );
        }
        if meta.is_dir() {
            scan_local_skill_dir(root, &path, files, total_bytes)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("failed to compute relative path for '{}'", path.display()))?
            .to_path_buf();
        if relative.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            bail!("invalid path in skill package: {}", relative.display());
        }
        *total_bytes = total_bytes.saturating_add(meta.len());
        files.push(ScannedLocalFile {
            absolute: path,
            relative,
        });
        if files.len() > MAX_SKILL_FILES {
            bail!("skill package has too many files (> {})", MAX_SKILL_FILES);
        }
    }
    Ok(())
}

fn parse_skill_markdown_metadata(markdown: &str, fallback_id: &str) -> ParsedSkillMetadata {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        return ParsedSkillMetadata {
            name: fallback_id.to_string(),
            description: String::new(),
            tags: vec![],
        };
    }

    let mut name = String::new();
    let mut description = String::new();
    let mut tags = Vec::new();

    if let Some(frontmatter) = parse_frontmatter(markdown) {
        if let Some(v) = frontmatter.get("name") {
            name = v.to_string();
        }
        if let Some(v) = frontmatter.get("description") {
            description = v.to_string();
        }
        if let Some(v) = frontmatter.get("tags") {
            tags = parse_tags_line(v);
        }
    }

    if name.trim().is_empty() {
        name = fallback_id.to_string();
    }

    if description.trim().is_empty() {
        for line in markdown.lines() {
            let clean = line.trim().trim_start_matches('#').trim();
            if clean.is_empty() {
                continue;
            }
            if clean.eq_ignore_ascii_case(&name) {
                continue;
            }
            if clean.starts_with("---") {
                continue;
            }
            description = clean.to_string();
            break;
        }
    }

    ParsedSkillMetadata {
        name: name.trim().to_string(),
        description: description.trim().to_string(),
        tags: tags
            .into_iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect(),
    }
}

fn parse_frontmatter(markdown: &str) -> Option<HashMap<String, String>> {
    let mut lines = markdown.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut map = HashMap::new();
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        map.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    Some(map)
}

fn parse_tags_line(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return trimmed
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|v| !v.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn tokenize_text(input: &str) -> HashSet<String> {
    input
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| v.len() >= 2)
        .collect()
}

fn overlap_count(a: &HashSet<String>, b: &HashSet<String>) -> usize {
    a.intersection(b).count()
}

fn score_catalog_entry(
    query_raw: &str,
    query_tokens: &HashSet<String>,
    entry: &SkillCatalogEntry,
) -> f32 {
    let id_lc = entry.id.to_ascii_lowercase();
    let exact_id_boost = if query_raw == id_lc {
        10.0
    } else if id_lc.contains(query_raw) {
        4.0
    } else {
        0.0
    };
    let name_tokens = tokenize_text(&entry.name);
    let tags_tokens = entry
        .tags
        .iter()
        .flat_map(|v| tokenize_text(v))
        .collect::<HashSet<_>>();
    let desc_tokens = tokenize_text(&entry.description);
    let mut keyword_tokens = entry
        .keywords
        .iter()
        .flat_map(|v| tokenize_text(v))
        .collect::<HashSet<_>>();
    keyword_tokens.extend(name_tokens.clone());
    keyword_tokens.extend(desc_tokens.clone());
    keyword_tokens.extend(tags_tokens.clone());

    let name_overlap = overlap_count(query_tokens, &name_tokens) as f32;
    let tags_overlap = overlap_count(query_tokens, &tags_tokens) as f32;
    let desc_overlap = overlap_count(query_tokens, &desc_tokens) as f32;
    let keyword_overlap = overlap_count(query_tokens, &keyword_tokens) as f32;

    let raw = exact_id_boost
        + name_overlap * 5.0
        + tags_overlap * 4.0
        + desc_overlap * 3.0
        + keyword_overlap * 2.0;
    (raw / 16.0).clamp(0.0, 1.0)
}

fn build_skill_token_set(
    id: &str,
    name: &str,
    description: &str,
    tags: &[String],
    content: &str,
) -> HashSet<String> {
    let mut out = tokenize_text(id);
    out.extend(tokenize_text(name));
    out.extend(tokenize_text(description));
    for tag in tags {
        out.extend(tokenize_text(tag));
    }
    let snippet = truncate_chars(content, 4096);
    out.extend(tokenize_text(&snippet));
    out
}

fn semantic_skill_runtime_score(query_tokens: &HashSet<String>, skill: &SkillRuntimeEntry) -> f32 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let matched = overlap_count(query_tokens, &skill.token_set) as f32;
    let denom = query_tokens.len().max(1) as f32;
    (matched / denom).clamp(0.0, 1.0)
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars.max(1)).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn current_unix_timestamp_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{SkillCatalogEntry, score_catalog_entry, tokenize_text};

    #[test]
    fn catalog_scoring_prefers_exact_match() {
        let entry = SkillCatalogEntry {
            id: "agent-browser".to_string(),
            name: "agent-browser".to_string(),
            description: "Browser automation skill".to_string(),
            tags: vec!["browser".to_string(), "automation".to_string()],
            version: "main".to_string(),
            source_ref: "openai/skills:skills/.curated/agent-browser@main".to_string(),
            download_url: "https://example.com/SKILL.md".to_string(),
            checksum_sha256: None,
            keywords: vec![],
            catalog: "official".to_string(),
        };
        let score = score_catalog_entry("agent-browser", &tokenize_text("agent-browser"), &entry);
        assert!(score > 0.7);
    }
}
