use super::*;

impl MessageService {
    pub(super) async fn complete_with_mcp_loop(
        &self,
        mut history: Vec<StoredMessage>,
        tools: Vec<McpToolInfo>,
        runtime: McpRuntime,
        incoming: &IncomingMessage,
    ) -> anyhow::Result<String> {
        let mut telemetry = McpLoopTelemetry::new(tools.len());
        let mcp_prompt = build_mcp_system_prompt(&tools)?;
        let model = self.provider.model_name().unwrap_or("unknown").to_string();
        let mut run = AgentRun::start(AgentRunStart {
            logger: self.trajectory_logger.clone(),
            metrics: self.trajectory_metrics.clone(),
            session_id: incoming.session_id.clone(),
            channel: incoming.channel.clone(),
            user_id: incoming.user_id.clone(),
            model,
        })
        .await;

        history.push(StoredMessage {
            role: MessageRole::System,
            content: mcp_prompt,
        });

        let protocol = ToolProtocol::new(tools.clone(), self.agent_mcp.max_tool_result_chars);
        let max_iterations = self.agent_mcp.max_iterations.max(1);
        let mut verification_retry_used = false;
        let mut tool_calls = Vec::new();
        for iteration in 0..max_iterations {
            telemetry.observe_prompt_chars(&history);
            let reply = match self
                .complete_provider_for_run(
                    &mut run,
                    CompletionRequest {
                        messages: history.clone(),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(reply) => reply,
                Err(err) => {
                    run.finish(AgentRunExit::InternalError).await;
                    return Err(err).context("provider completion failed");
                }
            };
            telemetry.iterations += 1;
            run.observe_iteration(iteration);

            let tool_call = match protocol.parse_reply(&reply) {
                ToolProposal::Tool(tool_call) => tool_call,
                ToolProposal::FinalAnswer => {
                    let reply = match self
                        .verify_final_answer(&history, &incoming.channel, reply, &tool_calls)
                        .await
                    {
                        Ok(reply) => reply,
                        Err(err) => {
                            run.finish(AgentRunExit::InternalError).await;
                            return Err(err).context("output verification failed");
                        }
                    };
                    telemetry.emit("final_answer");
                    run.finish(AgentRunExit::FinalAnswer(reply.clone())).await;
                    return Ok(reply);
                }
                ToolProposal::ParseError(verification) => {
                    warn_verification_failure(&verification);
                    if !verification_retry_used {
                        verification_retry_used = true;
                        history.push(StoredMessage {
                            role: MessageRole::Assistant,
                            content: reply,
                        });
                        history.push(verification_feedback_message(
                            &verification,
                            ToolVerificationMode::Retry,
                        ));
                        continue;
                    }
                    history.push(StoredMessage {
                        role: MessageRole::Assistant,
                        content: reply,
                    });
                    history.push(verification_feedback_message(
                        &verification,
                        ToolVerificationMode::Block,
                    ));
                    let final_reply = match self
                        .complete_provider_for_run(
                            &mut run,
                            CompletionRequest {
                                messages: history.clone(),
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        Ok(reply) => reply,
                        Err(err) => {
                            run.finish(AgentRunExit::InternalError).await;
                            return Err(err)
                                .context("provider completion failed after mcp parse error");
                        }
                    };
                    let final_reply = match self
                        .verify_final_answer(&history, &incoming.channel, final_reply, &tool_calls)
                        .await
                    {
                        Ok(reply) => reply,
                        Err(err) => {
                            run.finish(AgentRunExit::InternalError).await;
                            return Err(err)
                                .context("output verification failed after mcp parse error");
                        }
                    };
                    telemetry.emit("tool_error");
                    run.finish(AgentRunExit::ToolError(final_reply.clone()))
                        .await;
                    return Ok(final_reply);
                }
            };

            if let Err(verification) = protocol.validate_call(&tool_call) {
                warn_verification_failure(&verification);
                let record = verification_failure_record(&tool_call, &verification, iteration);
                let record = run.record_tool_call(record).await;
                tool_calls.push(record);
                if !verification_retry_used {
                    verification_retry_used = true;
                    history.push(StoredMessage {
                        role: MessageRole::Assistant,
                        content: reply,
                    });
                    history.push(verification_feedback_message(
                        &verification,
                        ToolVerificationMode::Retry,
                    ));
                    continue;
                }
                history.push(StoredMessage {
                    role: MessageRole::Assistant,
                    content: reply,
                });
                history.push(verification_feedback_message(
                    &verification,
                    ToolVerificationMode::Block,
                ));
                let final_reply = match self
                    .complete_provider_for_run(
                        &mut run,
                        CompletionRequest {
                            messages: history.clone(),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(reply) => reply,
                    Err(err) => {
                        run.finish(AgentRunExit::InternalError).await;
                        return Err(err)
                            .context("provider completion failed after mcp tool validation error");
                    }
                };
                let final_reply = match self
                    .verify_final_answer(&history, &incoming.channel, final_reply, &tool_calls)
                    .await
                {
                    Ok(reply) => reply,
                    Err(err) => {
                        run.finish(AgentRunExit::InternalError).await;
                        return Err(err)
                            .context("output verification failed after mcp tool validation error");
                    }
                };
                telemetry.emit("tool_error");
                run.finish(AgentRunExit::ToolError(final_reply.clone()))
                    .await;
                return Ok(final_reply);
            }

            let envelope = protocol
                .execute_validated(&runtime, tool_call, iteration)
                .await;
            let envelope = match envelope {
                Ok(envelope) => envelope,
                Err(err) => {
                    run.finish(AgentRunExit::InternalError).await;
                    return Err(err).context("mcp tool execution envelope failed");
                }
            };
            telemetry.tool_calls_total += 1;
            if envelope.record.ok {
                telemetry.tool_calls_ok += 1;
            } else {
                telemetry.tool_calls_err += 1;
            }

            let mut record = envelope.record.clone();
            let verification = self.tool_verifier.as_ref().map(|v| v.verify(&record));
            if let Some(verification) = &verification
                && !verification.passed
            {
                warn_verification_failure(verification);
                annotate_record_with_verification_failure(&mut record, verification);
            }
            let logged_record = run.record_tool_call(record.clone()).await;
            tool_calls.push(logged_record);

            if let Some(verification) = verification
                && !verification.passed
            {
                match self.tool_verification_mode {
                    ToolVerificationMode::Observe => {}
                    ToolVerificationMode::Retry if !verification_retry_used => {
                        verification_retry_used = true;
                        history.push(StoredMessage {
                            role: MessageRole::Assistant,
                            content: reply,
                        });
                        history.push(verification_feedback_message(
                            &verification,
                            ToolVerificationMode::Retry,
                        ));
                        continue;
                    }
                    ToolVerificationMode::Retry | ToolVerificationMode::Block => {
                        history.push(StoredMessage {
                            role: MessageRole::Assistant,
                            content: reply,
                        });
                        history.push(verification_feedback_message(
                            &verification,
                            ToolVerificationMode::Block,
                        ));
                        let final_reply = match self
                            .complete_provider_for_run(
                                &mut run,
                                CompletionRequest {
                                    messages: history.clone(),
                                    ..Default::default()
                                },
                            )
                            .await
                        {
                            Ok(reply) => reply,
                            Err(err) => {
                                run.finish(AgentRunExit::InternalError).await;
                                return Err(err).context(
                                    "provider completion failed after tool verification block",
                                );
                            }
                        };
                        let final_reply = match self
                            .verify_final_answer(
                                &history,
                                &incoming.channel,
                                final_reply,
                                &tool_calls,
                            )
                            .await
                        {
                            Ok(reply) => reply,
                            Err(err) => {
                                run.finish(AgentRunExit::InternalError).await;
                                return Err(err).context(
                                    "output verification failed after tool verification block",
                                );
                            }
                        };
                        telemetry.emit("tool_error");
                        run.finish(AgentRunExit::ToolError(final_reply.clone()))
                            .await;
                        return Ok(final_reply);
                    }
                }
            }

            history.push(StoredMessage {
                role: MessageRole::Assistant,
                content: reply,
            });
            history.push(StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "MCP_TOOL_RESULT_JSON:\n{}",
                    serde_json::to_string(&envelope.message_json)
                        .unwrap_or_else(|_| "{\"ok\":false}".to_string())
                ),
            });
        }

        history.push(StoredMessage {
            role: MessageRole::System,
            content: "MCP tool loop reached max iterations. Give a final answer based on available context."
                .to_string(),
        });
        let final_reply = match self
            .complete_provider_for_run(
                &mut run,
                CompletionRequest {
                    messages: history.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(err).context("provider completion failed");
            }
        };
        let final_reply = match self
            .verify_final_answer(&history, &incoming.channel, final_reply, &tool_calls)
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(err).context("output verification failed after max iterations");
            }
        };
        telemetry.emit("max_iterations");

        run.finish(AgentRunExit::MaxIterations(final_reply.clone()))
            .await;

        Ok(final_reply)
    }

    pub(super) async fn complete_with_mcp_loop_stream(
        &self,
        mut history: Vec<StoredMessage>,
        tools: Vec<McpToolInfo>,
        runtime: McpRuntime,
        sink: &mut dyn StreamSink,
        incoming: &IncomingMessage,
    ) -> anyhow::Result<String> {
        let mut telemetry = McpLoopTelemetry::new(tools.len());
        let mcp_prompt = build_mcp_system_prompt(&tools)?;
        let model = self.provider.model_name().unwrap_or("unknown").to_string();
        let mut run = AgentRun::start(AgentRunStart {
            logger: self.trajectory_logger.clone(),
            metrics: self.trajectory_metrics.clone(),
            session_id: incoming.session_id.clone(),
            channel: incoming.channel.clone(),
            user_id: incoming.user_id.clone(),
            model,
        })
        .await;

        history.push(StoredMessage {
            role: MessageRole::System,
            content: mcp_prompt,
        });

        let protocol = ToolProtocol::new(tools.clone(), self.agent_mcp.max_tool_result_chars);
        let max_iterations = self.agent_mcp.max_iterations.max(1);
        let mut verification_retry_used = false;
        let mut tool_calls = Vec::new();
        for iteration in 0..max_iterations {
            telemetry.observe_prompt_chars(&history);
            let mut buffered_sink = BufferedStreamSink::default();
            let resolved_reply = match self
                .complete_buffered_provider_for_run(
                    &mut run,
                    CompletionRequest {
                        messages: history.clone(),
                        ..Default::default()
                    },
                    &mut buffered_sink,
                )
                .await
            {
                Ok(reply) => reply,
                Err(err) => {
                    run.finish(AgentRunExit::InternalError).await;
                    return Err(err).context("provider stream completion failed");
                }
            };
            telemetry.iterations += 1;
            run.observe_iteration(iteration);

            let tool_call = match protocol.parse_reply(&resolved_reply) {
                ToolProposal::Tool(tool_call) => tool_call,
                ToolProposal::FinalAnswer => {
                    let resolved_reply = match self
                        .verify_final_answer(
                            &history,
                            &incoming.channel,
                            resolved_reply,
                            &tool_calls,
                        )
                        .await
                    {
                        Ok(reply) => reply,
                        Err(err) => {
                            run.finish(AgentRunExit::InternalError).await;
                            return Err(err).context("output verification failed");
                        }
                    };
                    telemetry.emit("final_answer");
                    replay_text_or_finish_internal_error(&mut run, &resolved_reply, sink).await?;
                    run.finish(AgentRunExit::FinalAnswer(resolved_reply.clone()))
                        .await;
                    return Ok(resolved_reply);
                }
                ToolProposal::ParseError(verification) => {
                    warn_verification_failure(&verification);
                    if !verification_retry_used {
                        verification_retry_used = true;
                        history.push(StoredMessage {
                            role: MessageRole::Assistant,
                            content: resolved_reply,
                        });
                        history.push(verification_feedback_message(
                            &verification,
                            ToolVerificationMode::Retry,
                        ));
                        continue;
                    }
                    history.push(StoredMessage {
                        role: MessageRole::Assistant,
                        content: resolved_reply,
                    });
                    history.push(verification_feedback_message(
                        &verification,
                        ToolVerificationMode::Block,
                    ));
                    let mut final_sink = BufferedStreamSink::default();
                    let resolved_final = match self
                        .complete_buffered_provider_for_run(
                            &mut run,
                            CompletionRequest {
                                messages: history.clone(),
                                ..Default::default()
                            },
                            &mut final_sink,
                        )
                        .await
                    {
                        Ok(reply) => reply,
                        Err(err) => {
                            run.finish(AgentRunExit::InternalError).await;
                            return Err(err).context(
                                "provider stream completion failed after mcp parse error",
                            );
                        }
                    };
                    let resolved_final = match self
                        .verify_final_answer(
                            &history,
                            &incoming.channel,
                            resolved_final,
                            &tool_calls,
                        )
                        .await
                    {
                        Ok(reply) => reply,
                        Err(err) => {
                            run.finish(AgentRunExit::InternalError).await;
                            return Err(err)
                                .context("output verification failed after mcp parse error");
                        }
                    };
                    telemetry.emit("tool_error");
                    replay_text_or_finish_internal_error(&mut run, &resolved_final, sink).await?;
                    run.finish(AgentRunExit::ToolError(resolved_final.clone()))
                        .await;
                    return Ok(resolved_final);
                }
            };

            if let Err(verification) = protocol.validate_call(&tool_call) {
                warn_verification_failure(&verification);
                let record = verification_failure_record(&tool_call, &verification, iteration);
                let record = run.record_tool_call(record).await;
                tool_calls.push(record);
                if !verification_retry_used {
                    verification_retry_used = true;
                    history.push(StoredMessage {
                        role: MessageRole::Assistant,
                        content: resolved_reply,
                    });
                    history.push(verification_feedback_message(
                        &verification,
                        ToolVerificationMode::Retry,
                    ));
                    continue;
                }
                history.push(StoredMessage {
                    role: MessageRole::Assistant,
                    content: resolved_reply,
                });
                history.push(verification_feedback_message(
                    &verification,
                    ToolVerificationMode::Block,
                ));
                let mut final_sink = BufferedStreamSink::default();
                let resolved_final = match self
                    .complete_buffered_provider_for_run(
                        &mut run,
                        CompletionRequest {
                            messages: history.clone(),
                            ..Default::default()
                        },
                        &mut final_sink,
                    )
                    .await
                {
                    Ok(reply) => reply,
                    Err(err) => {
                        run.finish(AgentRunExit::InternalError).await;
                        return Err(err).context(
                            "provider stream completion failed after mcp tool validation error",
                        );
                    }
                };
                let resolved_final = match self
                    .verify_final_answer(&history, &incoming.channel, resolved_final, &tool_calls)
                    .await
                {
                    Ok(reply) => reply,
                    Err(err) => {
                        run.finish(AgentRunExit::InternalError).await;
                        return Err(err)
                            .context("output verification failed after mcp tool validation error");
                    }
                };
                telemetry.emit("tool_error");
                replay_text_or_finish_internal_error(&mut run, &resolved_final, sink).await?;
                run.finish(AgentRunExit::ToolError(resolved_final.clone()))
                    .await;
                return Ok(resolved_final);
            }

            let envelope = protocol
                .execute_validated(&runtime, tool_call, iteration)
                .await;
            let envelope = match envelope {
                Ok(envelope) => envelope,
                Err(err) => {
                    run.finish(AgentRunExit::InternalError).await;
                    return Err(err).context("mcp tool execution envelope failed");
                }
            };
            telemetry.tool_calls_total += 1;
            if envelope.record.ok {
                telemetry.tool_calls_ok += 1;
            } else {
                telemetry.tool_calls_err += 1;
            }

            let mut record = envelope.record.clone();
            let verification = self.tool_verifier.as_ref().map(|v| v.verify(&record));
            if let Some(verification) = &verification
                && !verification.passed
            {
                warn_verification_failure(verification);
                annotate_record_with_verification_failure(&mut record, verification);
            }
            let logged_record = run.record_tool_call(record.clone()).await;
            tool_calls.push(logged_record);

            if let Some(verification) = verification
                && !verification.passed
            {
                match self.tool_verification_mode {
                    ToolVerificationMode::Observe => {}
                    ToolVerificationMode::Retry if !verification_retry_used => {
                        verification_retry_used = true;
                        history.push(StoredMessage {
                            role: MessageRole::Assistant,
                            content: resolved_reply,
                        });
                        history.push(verification_feedback_message(
                            &verification,
                            ToolVerificationMode::Retry,
                        ));
                        continue;
                    }
                    ToolVerificationMode::Retry | ToolVerificationMode::Block => {
                        history.push(StoredMessage {
                            role: MessageRole::Assistant,
                            content: resolved_reply,
                        });
                        history.push(verification_feedback_message(
                            &verification,
                            ToolVerificationMode::Block,
                        ));
                        let mut final_sink = BufferedStreamSink::default();
                        let resolved_final = match self
                            .complete_buffered_provider_for_run(
                                &mut run,
                                CompletionRequest {
                                    messages: history.clone(),
                                    ..Default::default()
                                },
                                &mut final_sink,
                            )
                            .await
                        {
                            Ok(reply) => reply,
                            Err(err) => {
                                run.finish(AgentRunExit::InternalError).await;
                                return Err(err).context(
                                    "provider stream completion failed after tool verification block",
                                );
                            }
                        };
                        let resolved_final = match self
                            .verify_final_answer(
                                &history,
                                &incoming.channel,
                                resolved_final,
                                &tool_calls,
                            )
                            .await
                        {
                            Ok(reply) => reply,
                            Err(err) => {
                                run.finish(AgentRunExit::InternalError).await;
                                return Err(err).context(
                                    "output verification failed after tool verification block",
                                );
                            }
                        };
                        telemetry.emit("tool_error");
                        replay_text_or_finish_internal_error(&mut run, &resolved_final, sink)
                            .await?;
                        run.finish(AgentRunExit::ToolError(resolved_final.clone()))
                            .await;
                        return Ok(resolved_final);
                    }
                }
            }

            history.push(StoredMessage {
                role: MessageRole::Assistant,
                content: resolved_reply,
            });
            history.push(StoredMessage {
                role: MessageRole::System,
                content: format!(
                    "MCP_TOOL_RESULT_JSON:\n{}",
                    serde_json::to_string(&envelope.message_json)
                        .unwrap_or_else(|_| "{\"ok\":false}".to_string())
                ),
            });
        }

        history.push(StoredMessage {
            role: MessageRole::System,
            content:
                "MCP tool loop reached max iterations. Give a final answer based on available context."
                    .to_string(),
        });
        let mut buffered_sink = BufferedStreamSink::default();
        let resolved_reply = match self
            .complete_buffered_provider_for_run(
                &mut run,
                CompletionRequest {
                    messages: history.clone(),
                    ..Default::default()
                },
                &mut buffered_sink,
            )
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(err).context("provider stream completion failed");
            }
        };
        let resolved_reply = match self
            .verify_final_answer(&history, &incoming.channel, resolved_reply, &tool_calls)
            .await
        {
            Ok(reply) => reply,
            Err(err) => {
                run.finish(AgentRunExit::InternalError).await;
                return Err(err).context("output verification failed after max iterations");
            }
        };
        telemetry.emit("max_iterations");
        replay_text_or_finish_internal_error(&mut run, &resolved_reply, sink).await?;
        run.finish(AgentRunExit::MaxIterations(resolved_reply.clone()))
            .await;

        Ok(resolved_reply)
    }
}

pub(crate) fn build_mcp_system_prompt(tools: &[McpToolInfo]) -> anyhow::Result<String> {
    let tool_defs = tools
        .iter()
        .take(64)
        .map(|t| {
            serde_json::json!({
                "server": t.server,
                "tool": t.name,
                "description": t.description,
                "input_schema": t.input_schema
            })
        })
        .collect::<Vec<_>>();
    let serialized = serde_json::to_string(&tool_defs).context("failed to encode mcp tool list")?;
    Ok(format!(
        "You can use MCP tools.\nWhen a tool call is needed, reply with ONLY JSON (no markdown, no extra text): {{\"server\":\"<server>\",\"tool\":\"<tool>\",\"arguments\":{{...}}}}.\nTime rule: when user asks about current time/date/year/today/now/weekday/zodiac, ALWAYS call {{\"server\":\"{BUILTIN_MCP_SERVER_NAME}\",\"tool\":\"{BUILTIN_MCP_TOOL_CURRENT_TIME}\",\"arguments\":{{}}}} first (or pass timezone).\nAvailable tools: {serialized}\nIf no tool is needed, reply with the final answer directly."
    ))
}

#[cfg(test)]
pub(crate) fn parse_mcp_tool_call(
    reply: &str,
) -> Option<crate::harness::tool_protocol::ParsedToolCall> {
    match ToolProtocol::new(Vec::new(), 0).parse_reply(reply) {
        ToolProposal::Tool(call) => Some(call),
        ToolProposal::ParseError(_) | ToolProposal::FinalAnswer => None,
    }
}
