use crate::harness::evolution::{
    EvolutionBenchmarkSuite, EvolutionCaseAssertions, EvolutionEvalCase,
};

/// Stable id of the built-in benchmark suite.
pub const CORE_BENCHMARK_SUITE_ID: &str = "core";
/// Bump when cases are added, removed, or their assertions change so recorded
/// scorecards remain attributable to the exact suite that produced them.
pub const CORE_BENCHMARK_SUITE_VERSION: &str = "2026-09-24.1";

fn bench_case(
    id: &str,
    name: &str,
    input: &str,
    assertions: EvolutionCaseAssertions,
) -> EvolutionEvalCase {
    EvolutionEvalCase {
        id: id.to_string(),
        name: name.to_string(),
        input: input.to_string(),
        assertions,
        weight: 1.0,
        enabled: true,
    }
}

/// The versioned benchmark suite that joins every shadow evaluation. Cases are
/// prompt-level probes derived from the deterministic `tests/harness_eval.rs`
/// scenarios: each one encodes an invariant a prompt patch must not break
/// (direct answers, structured output discipline, no leaked internals,
/// bounded verbosity). A candidate that regresses any of them is rejected by
/// the gate regardless of the operator regression budget.
pub fn core_benchmark_suite() -> EvolutionBenchmarkSuite {
    EvolutionBenchmarkSuite {
        id: CORE_BENCHMARK_SUITE_ID.to_string(),
        version: CORE_BENCHMARK_SUITE_VERSION.to_string(),
        cases: vec![
            // Mirrors `agent_run_final_answer`: a plain question must still
            // yield a direct, bounded final answer.
            bench_case(
                "bench:final_answer",
                "Direct final answer",
                "Reply with exactly the word DONE.",
                EvolutionCaseAssertions {
                    required_substrings: vec!["DONE".to_string()],
                    max_output_chars: Some(64),
                    ..Default::default()
                },
            ),
            // Mirrors `tool_protocol_schema_retry`: structured output stays
            // schema-disciplined when the contract asks for JSON.
            bench_case(
                "bench:json_contract",
                "JSON output contract",
                "Output only the JSON object {\"status\":\"ok\"} with no other text.",
                EvolutionCaseAssertions {
                    require_json: true,
                    required_substrings: vec!["\"status\"".to_string()],
                    max_output_chars: Some(256),
                    ..Default::default()
                },
            ),
            // Mirrors `output_exit_block_hidden_tool_error`: internal tool or
            // runtime error details must not leak into user-visible answers.
            bench_case(
                "bench:no_internal_leak",
                "No internal error leakage",
                "What time is it right now?",
                EvolutionCaseAssertions {
                    forbidden_substrings: vec![
                        "internal error".to_string(),
                        "tool call failed".to_string(),
                        "tool_error".to_string(),
                    ],
                    max_output_chars: Some(512),
                    ..Default::default()
                },
            ),
            // Mirrors `skill_selection_visible`: the reply answers the asked
            // question directly instead of narrating capabilities.
            bench_case(
                "bench:direct_answer",
                "Direct answer without capability narration",
                "What is 2 + 2? Answer with just the number.",
                EvolutionCaseAssertions {
                    required_substrings: vec!["4".to_string()],
                    forbidden_substrings: vec![
                        "as an AI".to_string(),
                        "I cannot".to_string(),
                        "tool".to_string(),
                    ],
                    max_output_chars: Some(32),
                    ..Default::default()
                },
            ),
            // Mirrors the compaction scenarios: a summary stays bounded and
            // faithful to the asked shape.
            bench_case(
                "bench:bounded_summary",
                "Bounded summary",
                "Summarize in a single word: a long walk in the park.",
                EvolutionCaseAssertions {
                    forbidden_substrings: vec!["I think".to_string(), "however".to_string()],
                    max_output_chars: Some(48),
                    ..Default::default()
                },
            ),
        ],
    }
}
