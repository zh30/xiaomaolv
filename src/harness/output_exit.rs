use std::sync::Arc;

use anyhow::Context;
use tracing::warn;

use crate::config::OutputVerificationMode;
use crate::domain::{MessageRole, StoredMessage};
use crate::harness::trajectory::ToolCallRecord;
use crate::harness::verifier::{
    DeterministicOutputVerifier, OutputVerificationRequest, OutputVerificationResult,
    VerificationIssue,
};
use crate::provider::{ChatProvider, CompletionRequest};

pub struct OutputExit {
    provider: Arc<dyn ChatProvider>,
    verifier: Option<DeterministicOutputVerifier>,
    mode: OutputVerificationMode,
    llm_enabled: bool,
    max_prompt_chars: usize,
    max_result_chars: usize,
}

pub struct OutputExitRequest<'a> {
    pub history: &'a [StoredMessage],
    pub channel: &'a str,
    pub final_answer: String,
    pub tool_calls: &'a [ToolCallRecord],
    pub required_format: Option<String>,
}

pub struct OutputExitResult {
    pub text: String,
    pub verified: bool,
    pub blocked_or_revised: bool,
    pub issue_codes: Vec<String>,
}

impl OutputExit {
    pub fn new(
        provider: Arc<dyn ChatProvider>,
        verifier: Option<DeterministicOutputVerifier>,
        mode: OutputVerificationMode,
        llm_enabled: bool,
        max_prompt_chars: usize,
        max_result_chars: usize,
    ) -> Self {
        Self {
            provider,
            verifier,
            mode,
            llm_enabled,
            max_prompt_chars,
            max_result_chars,
        }
    }

    pub async fn finalize(&self, req: OutputExitRequest<'_>) -> anyhow::Result<OutputExitResult> {
        let Some(verifier) = &self.verifier else {
            return Ok(OutputExitResult {
                text: req.final_answer,
                verified: false,
                blocked_or_revised: false,
                issue_codes: Vec::new(),
            });
        };

        let verification = verifier.verify(&OutputVerificationRequest {
            final_answer: req.final_answer.clone(),
            recent_history: req.history.to_vec(),
            tool_calls: req.tool_calls.to_vec(),
            channel: req.channel.to_string(),
            required_format: req.required_format.clone(),
        });
        if verification.passed {
            return Ok(OutputExitResult {
                text: req.final_answer,
                verified: true,
                blocked_or_revised: false,
                issue_codes: Vec::new(),
            });
        }

        warn_output_verification_failure(&verification);
        let issue_codes = verification
            .issues
            .iter()
            .map(|issue| issue.code.clone())
            .collect::<Vec<_>>();

        match self.mode {
            OutputVerificationMode::Off | OutputVerificationMode::Observe => Ok(OutputExitResult {
                text: req.final_answer,
                verified: true,
                blocked_or_revised: false,
                issue_codes,
            }),
            OutputVerificationMode::Block => Ok(OutputExitResult {
                text: verification
                    .suggested_revision
                    .unwrap_or_else(default_output_verification_fallback),
                verified: true,
                blocked_or_revised: true,
                issue_codes,
            }),
            OutputVerificationMode::ReviseOnce => {
                if !self.llm_enabled {
                    return Ok(OutputExitResult {
                        text: verification
                            .suggested_revision
                            .unwrap_or_else(default_output_verification_fallback),
                        verified: true,
                        blocked_or_revised: true,
                        issue_codes,
                    });
                }

                let revision_prompt = output_revision_prompt(&verification, self.max_prompt_chars);
                let mut revision_history = req.history.to_vec();
                revision_history.push(StoredMessage {
                    role: MessageRole::Assistant,
                    content: truncate_output_verification_text(
                        &req.final_answer,
                        self.max_result_chars,
                    ),
                });
                revision_history.push(StoredMessage {
                    role: MessageRole::System,
                    content: revision_prompt,
                });
                let revised = self
                    .provider
                    .complete(CompletionRequest {
                        messages: revision_history,
                        ..Default::default()
                    })
                    .await
                    .context("provider completion failed during output verification revision")?;
                let revised =
                    truncate_output_verification_text(revised.trim(), self.max_result_chars);
                let revised_verification = verifier.verify(&OutputVerificationRequest {
                    final_answer: revised.clone(),
                    recent_history: req.history.to_vec(),
                    tool_calls: req.tool_calls.to_vec(),
                    channel: req.channel.to_string(),
                    required_format: req.required_format.clone(),
                });
                if revised_verification.passed {
                    Ok(OutputExitResult {
                        text: revised,
                        verified: true,
                        blocked_or_revised: true,
                        issue_codes,
                    })
                } else {
                    warn_output_verification_failure(&revised_verification);
                    Ok(OutputExitResult {
                        text: revised_verification
                            .suggested_revision
                            .unwrap_or_else(default_output_verification_fallback),
                        verified: true,
                        blocked_or_revised: true,
                        issue_codes,
                    })
                }
            }
        }
    }
}

fn warn_output_verification_failure(verification: &OutputVerificationResult) {
    let issues = verification
        .issues
        .iter()
        .map(verification_issue_summary)
        .collect::<Vec<_>>();
    warn!(?issues, "Output verification failed");
}

fn output_revision_prompt(verification: &OutputVerificationResult, max_chars: usize) -> String {
    let payload = serde_json::to_string(verification)
        .unwrap_or_else(|_| "{\"passed\":false,\"issues\":[]}".to_string());
    let payload = truncate_output_verification_text(&payload, max_chars);
    format!(
        "OUTPUT_VERIFICATION_FAILED_JSON:\n{payload}\n\nRevise the previous assistant answer once. Return only the corrected final answer for the user. Do not expose tool-call JSON or verification internals."
    )
}

fn truncate_output_verification_text(input: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out = input.chars().take(max_chars).collect::<String>();
    out.push_str("...(truncated)");
    out
}

fn verification_issue_summary(issue: &VerificationIssue) -> String {
    format!(
        "{}:{}:{}",
        issue.severity.as_str(),
        issue.code,
        issue.message
    )
}

fn default_output_verification_fallback() -> String {
    "I could not produce a reliable final answer.".to_string()
}
