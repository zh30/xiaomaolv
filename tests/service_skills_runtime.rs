use std::sync::Arc;

use tempfile::tempdir;
use tokio::sync::RwLock;
use xiaomaolv::domain::{IncomingMessage, MessageRole};
use xiaomaolv::memory::{SqliteMemoryBackend, SqliteMemoryStore};
use xiaomaolv::provider::{ChatProvider, CompletionRequest};
use xiaomaolv::service::{AgentMcpSettings, AgentSkillsSettings, MessageService};
use xiaomaolv::skills::{
    SkillActivationMode, SkillConfigPaths, SkillRegistry, SkillRuntime, SkillScope,
};

struct InspectProvider;

#[async_trait::async_trait]
impl ChatProvider for InspectProvider {
    async fn complete(&self, req: CompletionRequest) -> anyhow::Result<String> {
        let has_skills_context = req
            .messages
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("SKILLS_CONTEXT"));
        if has_skills_context {
            Ok("skills:on".to_string())
        } else {
            Ok("skills:off".to_string())
        }
    }
}

fn test_paths(tmp: &tempfile::TempDir) -> SkillConfigPaths {
    SkillConfigPaths {
        user_config: tmp.path().join("user-skills.toml"),
        project_config: tmp.path().join("project-skills.toml"),
        user_dir: tmp.path().join("user-skills"),
        project_dir: tmp.path().join("project-skills"),
    }
}

fn create_local_skill(tmp: &tempfile::TempDir, name: &str, desc: &str) -> std::path::PathBuf {
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\ntags: [assistant]\n---\n\nBe concise."),
    )
    .expect("write");
    dir
}

async fn build_service_with_runtime(runtime: SkillRuntime) -> MessageService {
    let store = SqliteMemoryStore::new("sqlite::memory:")
        .await
        .expect("store");
    MessageService::new_with_backend(
        Arc::new(InspectProvider),
        Arc::new(SqliteMemoryBackend::new(store)),
        None,
        AgentMcpSettings::default(),
        16,
        0,
        0,
    )
    .with_agent_skills(
        Some(Arc::new(RwLock::new(runtime))),
        AgentSkillsSettings {
            enabled: true,
            max_selected: 3,
            max_prompt_chars: 8000,
            match_min_score: 0.45,
            llm_rerank_enabled: false,
        },
    )
}

#[tokio::test]
async fn always_mode_skill_is_injected() {
    let tmp = tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_paths(&tmp)).expect("registry");
    let skill_dir = create_local_skill(&tmp, "always-skill", "Always active helper");
    registry
        .install_local_skill(
            SkillScope::User,
            &skill_dir,
            Some("always-skill"),
            SkillActivationMode::Always,
        )
        .await
        .expect("install");
    let runtime = SkillRuntime::from_registry(&registry)
        .await
        .expect("runtime");
    let service = build_service_with_runtime(runtime).await;

    let out = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "s1".to_string(),
            user_id: "u1".to_string(),
            text: "hello".to_string(),
            reply_target: None,
        })
        .await
        .expect("handle");
    assert_eq!(out.text, "skills:on");
}

#[tokio::test]
async fn semantic_mode_skill_matches_query() {
    let tmp = tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_paths(&tmp)).expect("registry");
    let skill_dir = create_local_skill(&tmp, "calendar-skill", "calendar reminder scheduling");
    registry
        .install_local_skill(
            SkillScope::User,
            &skill_dir,
            Some("calendar-skill"),
            SkillActivationMode::Semantic,
        )
        .await
        .expect("install");
    let runtime = SkillRuntime::from_registry(&registry)
        .await
        .expect("runtime");
    let service = build_service_with_runtime(runtime).await;

    let out = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "s2".to_string(),
            user_id: "u2".to_string(),
            text: "calendar reminder scheduling".to_string(),
            reply_target: None,
        })
        .await
        .expect("handle");
    assert_eq!(out.text, "skills:on");
}

#[tokio::test]
async fn off_mode_skill_is_not_injected() {
    let tmp = tempdir().expect("tmp");
    let registry = SkillRegistry::new(test_paths(&tmp)).expect("registry");
    let skill_dir = create_local_skill(&tmp, "off-skill", "disabled helper");
    registry
        .install_local_skill(
            SkillScope::User,
            &skill_dir,
            Some("off-skill"),
            SkillActivationMode::Off,
        )
        .await
        .expect("install");
    let runtime = SkillRuntime::from_registry(&registry)
        .await
        .expect("runtime");
    let service = build_service_with_runtime(runtime).await;

    let out = service
        .handle(IncomingMessage {
            channel: "http".to_string(),
            session_id: "s3".to_string(),
            user_id: "u3".to_string(),
            text: "hello".to_string(),
            reply_target: None,
        })
        .await
        .expect("handle");
    assert_eq!(out.text, "skills:off");
}
