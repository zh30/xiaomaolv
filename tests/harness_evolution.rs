use std::collections::BTreeMap;

use xiaomaolv::harness::evolution::{
    EvolutionCaseAssertions, EvolutionEvalCase, EvolutionGateConfig, EvolutionPromotionDecision,
    EvolutionScorer, PromptPatch,
};

fn eval_case(id: &str, required: &[&str], forbidden: &[&str], weight: f64) -> EvolutionEvalCase {
    EvolutionEvalCase {
        id: id.to_string(),
        name: id.to_string(),
        input: format!("input for {id}"),
        assertions: EvolutionCaseAssertions {
            required_substrings: required.iter().map(|value| value.to_string()).collect(),
            forbidden_substrings: forbidden.iter().map(|value| value.to_string()).collect(),
            require_json: false,
        },
        weight,
        enabled: true,
    }
}

#[test]
fn prompt_patch_rejects_control_markers_and_character_overflow() {
    let marker = PromptPatch::new("Always emit MCP_TOOL_RESULT_JSON: directly.", 200)
        .expect_err("internal marker must be rejected");
    assert!(marker.to_string().contains("reserved harness marker"));

    let overflow = PromptPatch::new("四个字符", 3).expect_err("character limit must be enforced");
    assert!(overflow.to_string().contains("3 characters"));

    let valid = PromptPatch::new("Prefer direct, evidence-backed answers.", 200)
        .expect("safe prompt patch");
    assert_eq!(valid.as_str(), "Prefer direct, evidence-backed answers.");
}

#[test]
fn scorer_compares_weighted_outputs_and_counts_regressions() {
    let cases = vec![
        eval_case("required", &["done"], &["unsafe"], 2.0),
        eval_case("regression", &["safe"], &[], 1.0),
        eval_case("improvement", &["concise"], &[], 1.0),
    ];
    let baseline = BTreeMap::from([
        ("required".to_string(), "done".to_string()),
        ("regression".to_string(), "safe".to_string()),
        ("improvement".to_string(), "verbose".to_string()),
    ]);
    let candidate = BTreeMap::from([
        ("required".to_string(), "done".to_string()),
        ("regression".to_string(), "not safe enough".to_string()),
        ("improvement".to_string(), "concise".to_string()),
    ]);

    let scorecard = EvolutionScorer::score(&cases, &baseline, &candidate)
        .expect("complete output set should score");

    assert_eq!(scorecard.total_cases, 3);
    assert_eq!(scorecard.baseline_passed_cases, 2);
    assert_eq!(scorecard.candidate_passed_cases, 3);
    assert_eq!(scorecard.regressions, 0);
    assert!((scorecard.baseline_score - 0.75).abs() < f64::EPSILON);
    assert!((scorecard.candidate_score - 1.0).abs() < f64::EPSILON);
    assert!((scorecard.score_delta - 0.25).abs() < f64::EPSILON);
    let required_result = scorecard
        .case_results
        .iter()
        .find(|result| result.case_id == "required")
        .expect("required case result");
    assert_eq!(required_result.case_name, "required");
    assert_eq!(required_result.input, "input for required");
    assert_eq!(required_result.weight, 2.0);
    assert_eq!(required_result.assertions, cases[0].assertions);
    assert_eq!(required_result.baseline_output_excerpt, "done");
    assert_eq!(required_result.candidate_output_excerpt, "done");
    assert_eq!(required_result.baseline_output_sha256.len(), 64);
    assert_eq!(required_result.candidate_output_sha256.len(), 64);

    let regressed_candidate = BTreeMap::from([
        ("required".to_string(), "done".to_string()),
        ("regression".to_string(), "missing".to_string()),
        ("improvement".to_string(), "concise".to_string()),
    ]);
    let scorecard = EvolutionScorer::score(&cases, &baseline, &regressed_candidate)
        .expect("complete output set should score");
    assert_eq!(scorecard.regressions, 1);
}

#[test]
fn promotion_gate_reports_every_failed_invariant() {
    let cases = vec![
        eval_case("one", &["pass"], &[], 1.0),
        eval_case("two", &["pass"], &[], 1.0),
    ];
    let baseline = BTreeMap::from([
        ("one".to_string(), "pass".to_string()),
        ("two".to_string(), "pass".to_string()),
    ]);
    let candidate = BTreeMap::from([
        ("one".to_string(), "pass".to_string()),
        ("two".to_string(), "fail".to_string()),
    ]);
    let scorecard = EvolutionScorer::score(&cases, &baseline, &candidate).expect("scorecard");
    let config = EvolutionGateConfig {
        min_eval_cases: 3,
        min_candidate_score: 0.9,
        min_score_delta: 0.1,
        max_regressions: 0,
        ..Default::default()
    };

    let decision = config.decide(&scorecard);
    let EvolutionPromotionDecision::Rejected { reasons } = decision else {
        panic!("candidate should be rejected");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("minimum eval cases"))
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("candidate score"))
    );
    assert!(reasons.iter().any(|reason| reason.contains("score delta")));
    assert!(reasons.iter().any(|reason| reason.contains("regressions")));
}

#[test]
fn promotion_gate_marks_candidate_ready_when_all_invariants_pass() {
    let cases = vec![
        eval_case("one", &["one"], &[], 1.0),
        eval_case("two", &["two"], &[], 1.0),
        eval_case("three", &["three"], &[], 1.0),
    ];
    let baseline = BTreeMap::from([
        ("one".to_string(), "one".to_string()),
        ("two".to_string(), "missing".to_string()),
        ("three".to_string(), "missing".to_string()),
    ]);
    let candidate = BTreeMap::from([
        ("one".to_string(), "one".to_string()),
        ("two".to_string(), "two".to_string()),
        ("three".to_string(), "three".to_string()),
    ]);
    let scorecard = EvolutionScorer::score(&cases, &baseline, &candidate).expect("scorecard");
    let config = EvolutionGateConfig {
        min_eval_cases: 3,
        min_candidate_score: 0.9,
        min_score_delta: 0.2,
        max_regressions: 0,
        ..Default::default()
    };

    assert_eq!(config.decide(&scorecard), EvolutionPromotionDecision::Ready);
}
