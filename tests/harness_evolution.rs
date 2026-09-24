use std::collections::BTreeMap;

use xiaomaolv::harness::evolution::{
    EvolutionBenchmarkSuite, EvolutionCaseAssertions, EvolutionEvalCase, EvolutionGateConfig,
    EvolutionPromotionDecision, EvolutionScorer, PromptPatch,
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
            ..Default::default()
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

fn bench_case(id: &str, required: &[&str]) -> EvolutionEvalCase {
    eval_case(id, required, &[], 1.0)
}

#[test]
fn max_output_chars_assertion_bounds_response_size() {
    let mut case = eval_case("bounded", &[], &[], 1.0);
    case.assertions.max_output_chars = Some(8);
    assert!(case.validate().is_ok(), "size bound alone is a valid case");

    let baseline = BTreeMap::from([("bounded".to_string(), "short".to_string())]);
    let candidate = BTreeMap::from([(
        "bounded".to_string(),
        "this answer is much too long".to_string(),
    )]);
    let scorecard = EvolutionScorer::score(&[case.clone()], &baseline, &candidate)
        .expect("size-bounded scorecard");
    assert_eq!(scorecard.regressions, 1);
    let result = &scorecard.case_results[0];
    assert!(result.baseline_passed);
    assert!(!result.candidate_passed);
    assert!(
        result
            .candidate_issues
            .iter()
            .any(|issue| issue.contains("exceeds 8 characters"))
    );

    case.assertions.max_output_chars = Some(0);
    assert!(case.validate().is_err(), "zero bound must be rejected");
    case.assertions.max_output_chars = Some(1_048_577);
    assert!(case.validate().is_err(), "unbounded size must be rejected");
}

#[test]
fn benchmark_suite_validation_enforces_identity_bounds_and_unique_cases() {
    let valid = EvolutionBenchmarkSuite {
        id: "core".to_string(),
        version: "2026-09-24.1".to_string(),
        cases: vec![bench_case("bench:a", &["ok"])],
    };
    assert!(valid.validate().is_ok());
    assert_eq!(valid.label(), "core@2026-09-24.1");

    for suite in [
        EvolutionBenchmarkSuite {
            id: "  ".to_string(),
            ..valid.clone()
        },
        EvolutionBenchmarkSuite {
            version: String::new(),
            ..valid.clone()
        },
        EvolutionBenchmarkSuite {
            cases: vec![],
            ..valid.clone()
        },
        EvolutionBenchmarkSuite {
            cases: (0..17)
                .map(|idx| bench_case(&format!("bench:{idx}"), &["ok"]))
                .collect(),
            ..valid.clone()
        },
        EvolutionBenchmarkSuite {
            cases: vec![
                bench_case("bench:dup", &["ok"]),
                bench_case("bench:dup", &["ok"]),
            ],
            ..valid.clone()
        },
    ] {
        assert!(
            suite.validate().is_err(),
            "suite {suite:?} must fail validation"
        );
    }
}

#[test]
fn benchmark_cases_join_the_scorecard_with_provenance() {
    let operator_cases = vec![eval_case("operator:a", &["pass"], &[], 1.0)];
    let suite = EvolutionBenchmarkSuite {
        id: "core".to_string(),
        version: "v1".to_string(),
        cases: vec![
            bench_case("bench:a", &["pass"]),
            bench_case("bench:b", &["pass"]),
        ],
    };
    let baseline = BTreeMap::from([
        ("operator:a".to_string(), "pass".to_string()),
        ("bench:a".to_string(), "pass".to_string()),
        ("bench:b".to_string(), "miss".to_string()),
    ]);
    let candidate = BTreeMap::from([
        ("operator:a".to_string(), "pass".to_string()),
        ("bench:a".to_string(), "pass".to_string()),
        ("bench:b".to_string(), "pass".to_string()),
    ]);

    let scorecard =
        EvolutionScorer::score_with_benchmark(&operator_cases, Some(&suite), &baseline, &candidate)
            .expect("benchmark-aware scorecard");

    assert_eq!(scorecard.total_cases, 3);
    assert_eq!(scorecard.benchmark_suite.as_deref(), Some("core@v1"));
    assert_eq!(scorecard.benchmark_regressions, 0);
    assert_eq!(scorecard.baseline_passed_cases, 2);
    assert_eq!(scorecard.candidate_passed_cases, 3);
    assert!(
        scorecard
            .case_results
            .iter()
            .filter(|result| result.benchmark)
            .count()
            == 2
    );
    assert!(
        !scorecard
            .case_results
            .iter()
            .find(|result| result.case_id == "operator:a")
            .expect("operator result")
            .benchmark
    );
}

#[test]
fn benchmark_regressions_are_fatal_even_within_the_regression_budget() {
    let operator_cases = vec![eval_case("operator:a", &["pass"], &[], 1.0)];
    let suite = EvolutionBenchmarkSuite {
        id: "core".to_string(),
        version: "v1".to_string(),
        cases: vec![bench_case("bench:a", &["pass"])],
    };
    let baseline = BTreeMap::from([
        ("operator:a".to_string(), "pass".to_string()),
        ("bench:a".to_string(), "pass".to_string()),
    ]);
    let candidate = BTreeMap::from([
        ("operator:a".to_string(), "pass".to_string()),
        ("bench:a".to_string(), "miss".to_string()),
    ]);
    let scorecard =
        EvolutionScorer::score_with_benchmark(&operator_cases, Some(&suite), &baseline, &candidate)
            .expect("scorecard");
    assert_eq!(scorecard.regressions, 1);
    assert_eq!(scorecard.benchmark_regressions, 1);

    let config = EvolutionGateConfig {
        min_eval_cases: 1,
        min_candidate_score: 0.0,
        min_score_delta: -1.0,
        // A generous operator regression budget must not rescue a benchmark
        // regression.
        max_regressions: 10,
        ..Default::default()
    };
    let EvolutionPromotionDecision::Rejected { reasons } = config.decide(&scorecard) else {
        panic!("benchmark regression must reject promotion");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("benchmark scenario regressions"))
    );
    assert!(
        !reasons
            .iter()
            .any(|reason| reason.contains("exceed maximum"))
    );
}
