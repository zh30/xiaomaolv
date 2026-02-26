use tempfile::tempdir;
use xiaomaolv::skills::SkillConfigPaths;
use xiaomaolv::skills_commands::{
    execute_skills_command, parse_telegram_skills_command, skills_help_text,
};

fn test_paths(tmp: &tempfile::TempDir) -> SkillConfigPaths {
    SkillConfigPaths {
        user_config: tmp.path().join("user-skills.toml"),
        project_config: tmp.path().join("project-skills.toml"),
        user_dir: tmp.path().join("user-skills"),
        project_dir: tmp.path().join("project-skills"),
    }
}

fn make_local_skill(tmp: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: local demo\ntags: [demo]\n---\n\nInstruction body."
        ),
    )
    .expect("write skill");
    dir
}

#[test]
fn parse_telegram_skills_command_supports_install() {
    let cmd =
        parse_telegram_skills_command("install agent-browser --scope user --yes").expect("parse");
    assert!(matches!(
        cmd,
        xiaomaolv::skills_commands::SkillsCommands::Install(_)
    ));
}

#[test]
fn help_text_contains_core_subcommands() {
    let text = skills_help_text();
    assert!(text.contains("/skills ls"));
    assert!(text.contains("/skills install"));
    assert!(text.contains("/skills use"));
}

#[tokio::test]
async fn execute_local_install_use_ls_rm() {
    let tmp = tempdir().expect("tmp");
    let registry = xiaomaolv::skills::SkillRegistry::new(test_paths(&tmp)).expect("registry");
    let local_dir = make_local_skill(&tmp, "alpha-skill");

    let install = parse_telegram_skills_command(&format!(
        "install {} --scope user --mode semantic",
        local_dir.display()
    ))
    .expect("parse install");
    let install_out = execute_skills_command(&registry, install)
        .await
        .expect("install");
    assert!(install_out.reload_runtime);
    assert!(install_out.text.contains("installed"));

    let use_cmd = parse_telegram_skills_command("use alpha-skill --scope all --mode always")
        .expect("parse use");
    let use_out = execute_skills_command(&registry, use_cmd)
        .await
        .expect("use");
    assert!(use_out.reload_runtime);

    let ls = parse_telegram_skills_command("ls --scope merged").expect("parse ls");
    let ls_out = execute_skills_command(&registry, ls).await.expect("ls");
    assert!(ls_out.text.contains("alpha-skill"), "{}", ls_out.text);
    assert!(ls_out.text.contains("always"), "{}", ls_out.text);

    let rm = parse_telegram_skills_command("rm alpha-skill --scope all").expect("parse rm");
    let rm_out = execute_skills_command(&registry, rm).await.expect("rm");
    assert!(rm_out.reload_runtime);
}
