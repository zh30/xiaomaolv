use std::path::{Path, PathBuf};

use tempfile::tempdir;
use xiaomaolv::skills::{
    SkillActivationMode, SkillConfigPaths, SkillListScope, SkillRegistry, SkillRemoveScope,
    SkillScope,
};

fn test_paths(tmp: &tempfile::TempDir) -> SkillConfigPaths {
    SkillConfigPaths {
        user_config: tmp.path().join("user-skills.toml"),
        project_config: tmp.path().join("project-skills.toml"),
        user_dir: tmp.path().join("user-skills"),
        project_dir: tmp.path().join("project-skills"),
    }
}

fn create_local_skill(root: &Path, name: &str, desc: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {desc}\ntags: [demo,skill]\n---\n\nUse this skill carefully."
        ),
    )
    .expect("write");
    dir
}

#[tokio::test]
async fn registry_merge_uses_project_precedence() {
    let tmp = tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_paths(&tmp)).expect("registry");

    let user_skill = create_local_skill(tmp.path(), "shared-skill-user", "user version");
    let project_skill = create_local_skill(tmp.path(), "shared-skill-project", "project version");

    registry
        .install_local_skill(
            SkillScope::User,
            &user_skill,
            Some("shared-skill"),
            SkillActivationMode::Semantic,
        )
        .await
        .expect("install user");
    registry
        .install_local_skill(
            SkillScope::Project,
            &project_skill,
            Some("shared-skill"),
            SkillActivationMode::Always,
        )
        .await
        .expect("install project");

    let merged = registry
        .list_skills(SkillListScope::Merged)
        .await
        .expect("list merged");
    let shared = merged
        .iter()
        .find(|v| v.id == "shared-skill")
        .expect("shared skill");
    assert_eq!(shared.source, "project");
    assert_eq!(shared.mode, "always");
}

#[tokio::test]
async fn registry_remove_all_scope() {
    let tmp = tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_paths(&tmp)).expect("registry");
    let user_skill = create_local_skill(tmp.path(), "demo-user", "user");
    let project_skill = create_local_skill(tmp.path(), "demo-project", "project");

    registry
        .install_local_skill(
            SkillScope::User,
            &user_skill,
            Some("demo"),
            SkillActivationMode::Semantic,
        )
        .await
        .expect("install user");
    registry
        .install_local_skill(
            SkillScope::Project,
            &project_skill,
            Some("demo"),
            SkillActivationMode::Semantic,
        )
        .await
        .expect("install project");

    let removed = registry
        .remove_skill(SkillRemoveScope::All, "demo")
        .await
        .expect("remove");
    assert!(removed);

    let merged = registry.load_merged().await.expect("merged");
    assert!(!merged.contains_key("demo"));
}
