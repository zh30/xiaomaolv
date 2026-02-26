use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::skills::{
    SkillActivationMode, SkillCatalogIndex, SkillConfigPaths, SkillListScope, SkillRegistry,
    SkillRemoveScope, SkillRerankMode, SkillScope,
};

#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommands {
    /// Install a skill from local path, exact id, or semantic query
    Install(SkillsInstallArgs),
    /// Search skills from catalog
    Search(SkillsSearchArgs),
    /// List installed skills
    Ls(SkillsLsArgs),
    /// Change skill activation mode
    Use(SkillsUseArgs),
    /// Update installed catalog skill
    Update(SkillsUpdateArgs),
    /// Remove installed skill
    Rm(SkillsRmArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillScopeArg {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillListScopeArg {
    Merged,
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillTargetScopeArg {
    All,
    User,
    Project,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillIndexArg {
    Official,
    Team,
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillRerankArg {
    None,
    Llm,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SkillModeArg {
    Semantic,
    Always,
    Off,
}

#[derive(Debug, Clone, Args)]
pub struct SkillsInstallArgs {
    /// Local path, exact skill id, or semantic query
    pub target_or_query: String,
    #[arg(long, value_enum, default_value = "user")]
    pub scope: SkillScopeArg,
    #[arg(long, value_enum, default_value = "official")]
    pub index: SkillIndexArg,
    #[arg(long, value_enum, default_value = "semantic")]
    pub mode: SkillModeArg,
    /// Auto-install when semantic match confidence is high
    #[arg(long, default_value_t = false)]
    pub yes: bool,
    /// Optional explicit id when installing from local path
    #[arg(long)]
    pub id: Option<String>,
    #[arg(long, value_enum, default_value = "none")]
    pub rerank: SkillRerankArg,
}

#[derive(Debug, Clone, Args)]
pub struct SkillsSearchArgs {
    pub query: String,
    #[arg(long, default_value_t = 5)]
    pub top: usize,
    #[arg(long, value_enum, default_value = "all")]
    pub index: SkillIndexArg,
    #[arg(long, value_enum, default_value = "none")]
    pub rerank: SkillRerankArg,
}

#[derive(Debug, Clone, Args)]
pub struct SkillsLsArgs {
    #[arg(long, value_enum, default_value = "merged")]
    pub scope: SkillListScopeArg,
}

#[derive(Debug, Clone, Args)]
pub struct SkillsUseArgs {
    pub id: String,
    #[arg(long, value_enum, default_value = "all")]
    pub scope: SkillTargetScopeArg,
    #[arg(long, value_enum)]
    pub mode: SkillModeArg,
}

#[derive(Debug, Clone, Args)]
pub struct SkillsUpdateArgs {
    pub id: String,
    #[arg(long, value_enum, default_value = "all")]
    pub scope: SkillTargetScopeArg,
    #[arg(long)]
    pub to_version: Option<String>,
    #[arg(long, default_value_t = false)]
    pub latest: bool,
    #[arg(long, value_enum, default_value = "all")]
    pub index: SkillIndexArg,
}

#[derive(Debug, Clone, Args)]
pub struct SkillsRmArgs {
    pub id: String,
    #[arg(long, value_enum, default_value = "all")]
    pub scope: SkillTargetScopeArg,
}

#[derive(Debug, Parser)]
#[command(name = "skills")]
#[command(disable_help_flag = true)]
#[command(disable_version_flag = true)]
struct TelegramSkillsCli {
    #[command(subcommand)]
    command: SkillsCommands,
}

#[derive(Debug, Clone)]
pub struct SkillCommandOutput {
    pub text: String,
    pub reload_runtime: bool,
}

pub fn discover_skill_registry() -> anyhow::Result<SkillRegistry> {
    let cwd = std::env::current_dir().context("failed to resolve current directory")?;
    let paths = SkillConfigPaths::discover(&cwd)?;
    SkillRegistry::new(paths)
}

pub fn parse_telegram_skills_command(raw: &str) -> anyhow::Result<SkillsCommands> {
    let tokens = shlex::split(raw)
        .ok_or_else(|| anyhow::anyhow!("invalid command line: unmatched quote"))?;
    if tokens.is_empty() {
        bail!("missing skills subcommand");
    }

    let mut argv = Vec::with_capacity(tokens.len() + 1);
    argv.push("skills".to_string());
    argv.extend(tokens);

    let parsed = TelegramSkillsCli::try_parse_from(argv)
        .map_err(|err| anyhow::anyhow!(err.to_string()))?
        .command;
    normalize_skills_command(parsed)
}

pub fn normalize_skills_command(command: SkillsCommands) -> anyhow::Result<SkillsCommands> {
    match command {
        SkillsCommands::Update(args) => {
            if args.to_version.is_some() && args.latest {
                bail!("use either --to-version or --latest, not both");
            }
            Ok(SkillsCommands::Update(args))
        }
        other => Ok(other),
    }
}

pub async fn execute_skills_command(
    registry: &SkillRegistry,
    command: SkillsCommands,
) -> anyhow::Result<SkillCommandOutput> {
    match normalize_skills_command(command)? {
        SkillsCommands::Ls(args) => {
            let rows = registry
                .list_skills(match args.scope {
                    SkillListScopeArg::Merged => SkillListScope::Merged,
                    SkillListScopeArg::User => SkillListScope::User,
                    SkillListScopeArg::Project => SkillListScope::Project,
                })
                .await?;

            if rows.is_empty() {
                return Ok(SkillCommandOutput {
                    text: "no skills installed".to_string(),
                    reload_runtime: false,
                });
            }

            let mut lines = Vec::new();
            lines.push(format!(
                "{:<22} {:<8} {:<8} {:<8} {:<12} source_ref",
                "id", "source", "enabled", "mode", "version"
            ));
            for row in rows {
                lines.push(format!(
                    "{:<22} {:<8} {:<8} {:<8} {:<12} {}",
                    row.id,
                    row.source,
                    if row.enabled { "yes" } else { "no" },
                    row.mode,
                    row.version,
                    truncate_chars(&row.source_ref, 60),
                ));
            }
            Ok(SkillCommandOutput {
                text: lines.join("\n"),
                reload_runtime: false,
            })
        }
        SkillsCommands::Search(args) => {
            let hits = registry
                .search_catalog(
                    &args.query,
                    args.top.max(1),
                    to_index(args.index),
                    to_rerank(args.rerank),
                )
                .await?;
            if hits.is_empty() {
                return Ok(SkillCommandOutput {
                    text: format!("no skill matched '{}'", args.query),
                    reload_runtime: false,
                });
            }

            let mut lines = Vec::new();
            lines.push(format!("search results for '{}':", args.query));
            for (idx, hit) in hits.iter().enumerate() {
                lines.push(format!(
                    "{}. {} (score={:.3}, version={}, catalog={})",
                    idx + 1,
                    hit.id,
                    hit.score,
                    hit.version,
                    hit.catalog
                ));
                if !hit.description.trim().is_empty() {
                    lines.push(format!("   {}", truncate_chars(&hit.description, 120)));
                }
            }
            Ok(SkillCommandOutput {
                text: lines.join("\n"),
                reload_runtime: false,
            })
        }
        SkillsCommands::Install(args) => {
            let scope = to_scope(args.scope);
            let mode = to_mode(args.mode);
            let target = args.target_or_query.trim();
            if target.is_empty() {
                bail!("target/query is empty");
            }

            let local_path = PathBuf::from(target);
            if local_path.exists() {
                let result = registry
                    .install_local_skill(scope, &local_path, args.id.as_deref(), mode)
                    .await?;
                return Ok(SkillCommandOutput {
                    text: format!("installed local skill '{}' ({})", result.id, result.version),
                    reload_runtime: true,
                });
            }

            let explicit_id_like = is_probable_skill_id(target);
            if let Some(entry) = registry
                .find_catalog_entry_exact(target, to_index(args.index))
                .await?
            {
                let result = registry.install_catalog_skill(scope, &entry, mode).await?;
                return Ok(SkillCommandOutput {
                    text: format!(
                        "installed catalog skill '{}' ({})",
                        result.id, result.version
                    ),
                    reload_runtime: true,
                });
            }

            if explicit_id_like {
                return Ok(SkillCommandOutput {
                    text: format!(
                        "skill '{}' not found in selected catalog index. try '--index all' or install from local path.",
                        target
                    ),
                    reload_runtime: false,
                });
            }

            let hits = match registry
                .search_catalog(target, 5, to_index(args.index), to_rerank(args.rerank))
                .await
            {
                Ok(v) => v,
                Err(err) => {
                    return Ok(SkillCommandOutput {
                        text: format!(
                            "catalog search unavailable: {err}\nTip: set GITHUB_TOKEN/GH_TOKEN, use '--index team', or install from local path."
                        ),
                        reload_runtime: false,
                    });
                }
            };
            let hits = hits
                .into_iter()
                .filter(|hit| hit.score >= 0.45)
                .collect::<Vec<_>>();

            if hits.is_empty() {
                return Ok(SkillCommandOutput {
                    text: format!("no skill matched '{}' with confidence >= 0.45", target),
                    reload_runtime: false,
                });
            }

            let top = &hits[0];
            let second = hits.get(1);
            let diff_ok = second.map(|v| top.score - v.score >= 0.12).unwrap_or(true);
            let auto_install_ok = top.score >= 0.72 && diff_ok;

            if !args.yes || !auto_install_ok {
                let mut lines = Vec::new();
                lines.push(format!(
                    "semantic install matched {} candidate(s):",
                    hits.len()
                ));
                for (idx, hit) in hits.iter().enumerate() {
                    lines.push(format!(
                        "{}. {} (score={:.3}, version={})",
                        idx + 1,
                        hit.id,
                        hit.score,
                        hit.version
                    ));
                }
                let recommend = format!(
                    "xiaomaolv skills install {} --scope {} --mode {} --yes",
                    top.id,
                    match args.scope {
                        SkillScopeArg::User => "user",
                        SkillScopeArg::Project => "project",
                    },
                    mode
                );
                lines.push(String::new());
                lines.push(
                    "No changes made. Re-run with explicit id to confirm install:".to_string(),
                );
                lines.push(recommend);
                if args.yes && !auto_install_ok {
                    lines.push(
                        "Auto-install skipped because confidence threshold is not met.".to_string(),
                    );
                }
                return Ok(SkillCommandOutput {
                    text: lines.join("\n"),
                    reload_runtime: false,
                });
            }

            let entry = registry
                .find_catalog_entry_exact(&top.id, to_index(args.index))
                .await?
                .with_context(|| format!("catalog entry '{}' not found", top.id))?;
            let result = registry.install_catalog_skill(scope, &entry, mode).await?;
            Ok(SkillCommandOutput {
                text: format!("installed skill '{}' ({})", result.id, result.version),
                reload_runtime: true,
            })
        }
        SkillsCommands::Use(args) => {
            let changed = registry
                .set_skill_mode(to_target_scope(args.scope), &args.id, to_mode(args.mode))
                .await?;
            let text = if changed == 0 {
                format!("skill '{}' not found", args.id)
            } else {
                format!("updated skill '{}' mode in {} scope(s)", args.id, changed)
            };
            Ok(SkillCommandOutput {
                text,
                reload_runtime: changed > 0,
            })
        }
        SkillsCommands::Update(args) => {
            let result = registry
                .update_skill(
                    to_target_scope(args.scope),
                    &args.id,
                    args.to_version.as_deref(),
                    args.latest,
                    to_index(args.index),
                )
                .await?;
            let text = format!(
                "update summary: touched={}, updated={}, up_to_date={}, skipped_local={}, missing={}",
                result.touched,
                result.updated,
                result.up_to_date,
                result.skipped_local,
                result.missing
            );
            Ok(SkillCommandOutput {
                text,
                reload_runtime: result.updated > 0,
            })
        }
        SkillsCommands::Rm(args) => {
            let removed = registry
                .remove_skill(to_target_scope(args.scope), &args.id)
                .await?;
            let text = if removed {
                format!("removed skill '{}'", args.id)
            } else {
                format!("skill '{}' not found", args.id)
            };
            Ok(SkillCommandOutput {
                text,
                reload_runtime: removed,
            })
        }
    }
}

pub fn skills_help_text() -> String {
    [
        "可用命令:",
        "/skills ls [--scope merged|user|project]",
        "/skills search <query> [--top 5] [--index official|team|all]",
        "/skills install <path|id|query> [--scope user|project] [--mode semantic|always|off]",
        "/skills use <id> --mode semantic|always|off [--scope all|user|project]",
        "/skills update <id> [--scope all|user|project] [--latest|--to-version <v>]",
        "/skills rm <id> [--scope all|user|project]",
        "示例:",
        "/skills install agent-browser --scope user --mode semantic --yes",
    ]
    .join("\n")
}

fn to_scope(arg: SkillScopeArg) -> SkillScope {
    match arg {
        SkillScopeArg::User => SkillScope::User,
        SkillScopeArg::Project => SkillScope::Project,
    }
}

fn to_target_scope(arg: SkillTargetScopeArg) -> SkillRemoveScope {
    match arg {
        SkillTargetScopeArg::All => SkillRemoveScope::All,
        SkillTargetScopeArg::User => SkillRemoveScope::User,
        SkillTargetScopeArg::Project => SkillRemoveScope::Project,
    }
}

fn to_index(arg: SkillIndexArg) -> SkillCatalogIndex {
    match arg {
        SkillIndexArg::Official => SkillCatalogIndex::Official,
        SkillIndexArg::Team => SkillCatalogIndex::Team,
        SkillIndexArg::All => SkillCatalogIndex::All,
    }
}

fn to_rerank(arg: SkillRerankArg) -> SkillRerankMode {
    match arg {
        SkillRerankArg::None => SkillRerankMode::None,
        SkillRerankArg::Llm => SkillRerankMode::Llm,
    }
}

fn to_mode(arg: SkillModeArg) -> SkillActivationMode {
    match arg {
        SkillModeArg::Semantic => SkillActivationMode::Semantic,
        SkillModeArg::Always => SkillActivationMode::Always,
        SkillModeArg::Off => SkillActivationMode::Off,
    }
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars.max(1)).collect()
}

fn is_probable_skill_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !trimmed.contains(char::is_whitespace)
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::tempdir;

    use crate::skills::SkillConfigPaths;

    use super::{execute_skills_command, parse_telegram_skills_command};

    fn test_paths(tmp: &tempfile::TempDir) -> SkillConfigPaths {
        SkillConfigPaths {
            user_config: tmp.path().join("user-skills.toml"),
            project_config: tmp.path().join("project-skills.toml"),
            user_dir: tmp.path().join("user-skills"),
            project_dir: tmp.path().join("project-skills"),
        }
    }

    fn make_local_skill(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: demo skill\ntags: [demo]\n---\n\ninstruction"),
        )
        .expect("write skill");
        dir
    }

    #[test]
    fn parse_telegram_skills_command_install() {
        let cmd = parse_telegram_skills_command("install agent-browser --scope user --yes")
            .expect("parse");
        assert!(matches!(cmd, super::SkillsCommands::Install(_)));
    }

    #[tokio::test]
    async fn execute_install_ls_rm_local_skill() {
        let tmp = tempdir().expect("tmp");
        let registry = crate::skills::SkillRegistry::new(test_paths(&tmp)).expect("registry");
        let local_dir = make_local_skill(&tmp, "demo-skill");

        let install = parse_telegram_skills_command(&format!(
            "install {} --scope user --mode always",
            local_dir.display()
        ))
        .expect("parse install");
        let out = execute_skills_command(&registry, install)
            .await
            .expect("install");
        assert!(out.reload_runtime);

        let ls = parse_telegram_skills_command("ls --scope merged").expect("parse ls");
        let ls_out = execute_skills_command(&registry, ls).await.expect("ls");
        assert!(ls_out.text.contains("demo-skill"), "{}", ls_out.text);

        let use_cmd = parse_telegram_skills_command("use demo-skill --mode off --scope all")
            .expect("parse use");
        let use_out = execute_skills_command(&registry, use_cmd)
            .await
            .expect("use");
        assert!(use_out.reload_runtime);

        let rm = parse_telegram_skills_command("rm demo-skill --scope all").expect("parse rm");
        let rm_out = execute_skills_command(&registry, rm).await.expect("rm");
        assert!(rm_out.reload_runtime);
    }
}
