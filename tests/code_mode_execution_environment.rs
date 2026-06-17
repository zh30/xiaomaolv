use xiaomaolv::code_mode::AgentCodeModeSettings;
use xiaomaolv::harness::execution_environment::{
    ExecutionEnvironment, ExecutionIsolation, LocalExecutionEnvironment,
    SubprocessExecutionEnvironment,
};

#[test]
fn execution_environments_report_isolation_level() {
    let settings = AgentCodeModeSettings::default();
    let local = LocalExecutionEnvironment::new(settings.clone());
    let subprocess = SubprocessExecutionEnvironment::new(settings);

    assert_eq!(local.isolation(), ExecutionIsolation::InProcess);
    assert_eq!(
        subprocess.isolation(),
        ExecutionIsolation::SubprocessNoSandbox
    );
    assert!(!subprocess.isolation().is_security_sandbox());
}
