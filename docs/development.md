# xiaomaolv Development Guide

这份文档是项目开发入口，适合新贡献者、未来维护者和接手任务的 agent 先读。更细的专题文档仍保留在 `docs/` 与 `docs/plans/` 中；这里负责把开发路径串起来。

## 1. 项目定位

`xiaomaolv` 是一个 Rust AI gateway service，负责把 Telegram/HTTP 消息路由到 AI provider，同时管理记忆、MCP 工具、Skills、Code Mode、Prompt Evolution 和可恢复的 Loop Engineering harness。

核心数据流：

```text
Telegram/HTTP -> Channel -> Service -> Memory -> Provider -> StreamSink -> Channel -> Response
                    |
                    +-> Scheduler
                    +-> MCP Runtime / Skills / Code Mode / Harness

HTTP/Telegram/Signal -> LoopEngine -> Workflow DAG -> Leased Worker -> Checkpoint/Artifact
                            +-> Self-test / Structural Replay / Goal event SSE
```

开发时优先保持这个边界：channel 处理协议，service 编排业务流，provider 处理模型传输，memory 处理存储与召回。

## 2. 本地环境

推荐先从 MVP 配置跑通：

```bash
cp .env.realtest.example .env.realtest
./scripts/run_mvp_minimax_telegram.sh
```

常用环境变量：

- `MINIMAX_API_KEY`：MiniMax/OpenAI-compatible provider key。
- `TELEGRAM_BOT_TOKEN`：Telegram BotFather token。
- `MINIMAX_MODEL`：模型名，默认 `MiniMax-M2.5-highspeed`。
- `TELEGRAM_ADMIN_USER_IDS`：私聊访问、`/mcp`、`/skills`、`/goal`、`/resume` 管理员白名单。
- `XIAOMAOLV_APP_API_KEY`：Loop Engineering operator 控制面必需，也用于其他受保护 HTTP API。
- `XIAOMAOLV_HARNESS_INGEST_API_KEY`：可选；只允许外部来源调用 `POST /v1/harness/signals`，不能操作 Goal。

开发热重载：

```bash
./scripts/run_mvp_minimax_telegram.sh --hot-reload
```

混合记忆 sidecar：

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory
```

## 3. 常用命令

格式化：

```bash
cargo fmt --all
```

Lint：

```bash
cargo clippy --all-targets -- -D warnings
```

全量测试：

```bash
cargo test --all-targets
```

单个集成测试：

```bash
cargo test --test <test_name> -- --nocapture
```

生产构建：

```bash
cargo build --release
```

合并或发 PR 前至少跑：

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## 4. 模块地图

- `src/main.rs`：CLI 入口，负责 `serve`、`init`、MCP/Skills 命令分发。
- `src/http.rs`：HTTP composition root，router、runtime state、setup UI、HTTP API、rate limit、auth。
- `src/http/harness_control.rs`：Loop Engineering operator/ingest 鉴权、Goal/Signal/Self-test/Replay/Artifact API 与 SSE。
- `src/service.rs`：消息主编排层，负责 memory、MCP loop、Skills、scheduler intent、harness、streaming 流程。
- `src/provider.rs`：`ChatProvider`、stream sink、OpenAI-compatible provider、重试和流式解析。
- `src/memory.rs`：SQLite store、memory backend、hybrid zvec backend、scheduler/swarm/harness 持久化。
- `src/channel.rs` 与 `src/channel/`：Telegram channel、group policy、commands、streaming、workers。
- `src/scheduler.rs`：纯 scheduler domain logic，包含状态机和 schedule 计算。
- `src/mcp.rs` / `src/mcp_commands.rs`：MCP 配置、runtime、CLI/Telegram 命令。
- `src/skills.rs` / `src/skills_commands.rs`：Skills registry、runtime selection、CLI/Telegram 命令。
- `src/code_mode.rs`：Code Mode planner、policy、executor、subprocess protocol。
- `src/harness/`：run lifecycle、trajectory、observability、verification、context compaction、ToolProtocol、OutputExit 与 Prompt Evolution。
- `src/harness/loop_engine/`：Goal/Workflow/WorkItem/Attempt/Checkpoint、租约/fencing、Signal、Self-test、Replay、Artifact 与 worker。
- `tests/`：跨模块集成测试和 HTTP/API/service 行为回归。

## 5. 架构边界

开发时按这些规则切分责任：

- 协议解析、Telegram API payload、HTTP status 等留在 channel/http 层。
- 业务流编排放在 `service.rs`，但避免把 transport 细节带进去。
- 存储 schema、查询、召回策略放在 `memory.rs`。
- Provider 只处理模型请求、重试、响应规范化。
- Scheduler 的状态转移和时间计算尽量保持纯函数，方便单测。
- Harness 功能要区分被动观测和会影响输出的行为，并在 config/docs 中讲清楚。
- Loop Engine 中 provider/tool 调用不得发生在 SQLite transaction 内；dispatch 前必须完成精确 revision/hash 审批。
- 新 `WorkHandler` 必须注册、声明 effect class、定义幂等/恢复语义并受预算约束。本版本禁止 `external_write` handler。
- Self-test 的生产套件保持只读；会修改/崩溃注入的测试只能在临时 SQLite 和 fake provider/handler 上运行。
- Signal ingest key 只用于外部写入，不能与 `app.api_key` 混用，也不能允许调用者声明 `internal` trust。

避免在 runtime 路径上使用 `unwrap`/`expect`。测试里可以用，但用户请求路径要返回带上下文的错误。

## 6. 配置开发

默认模板：

- `config/xiaomaolv.minimax-telegram.toml`：MVP 推荐配置。
- `config/xiaomaolv.example.toml`：通用模板。
- `.env.realtest.example`：安全的 env 模板。

新增配置项时请同步：

1. `src/config.rs` 中的结构体、默认值和 env placeholder 解析。
2. 相关 config 模板和 `.env.realtest.example`。
3. `README.md` / `README.zh.md`，如果是用户可见行为。
4. `tests/config_bootstrap.rs`，覆盖解析、默认值和 env fallback。
5. 如果 setup UI 需要展示，更新 `src/http.rs` 的 `CONFIG_UI_FIELDS` 和 `tests/config_ui.rs`。

Loop Engine 配置约束：

- `worker_enabled = true` 要求 `loop_engine.enabled = true`。
- `worker_poll_interval_secs` 为 `1..=60`，`worker_lease_secs` 为 `1..=3600`，`worker_max_parallel` 为 `1..=16`。
- `self_test_interval_secs` 为 `0`（关闭）或 `10..=2592000`。
- `enable_trajectory = true` 才会记录主消息路径的 provider frames。

## 7. 测试选择

按改动范围选择最小但足够的测试集：

- 配置解析：`cargo test --test config_bootstrap -- --nocapture`
- Setup/config UI：`cargo test --test config_ui -- --nocapture`
- HTTP API/auth/rate limit：`cargo test --test http_api -- --nocapture`
- MCP loop：`cargo test --test service_mcp_loop -- --nocapture`
- Service pipeline：`cargo test --test service_pipeline -- --nocapture`
- Harness eval：`cargo test --test harness_eval -- --nocapture`
- Loop Engine domain/store/worker：`cargo test --test harness_loop_engine -- --nocapture`
- Loop Engine HTTP/auth/lifecycle：`cargo test --test http_loop_engine_api -- --nocapture`
- Provider frame capture：`cargo test --test service_harness_trajectory -- --nocapture`
- Loop Engine → Prompt Evolution adapter：`cargo test --test harness_evolution_engine -- --nocapture`
- Memory/schema：`cargo test --test memory_store -- --nocapture`
- Scheduler：`cargo test --test scheduler_domain -- --nocapture && cargo test --test scheduler_store -- --nocapture`
- Telegram mode/channel plugin：`cargo test --test telegram_channel_mode -- --nocapture && cargo test --test channel_plugin_api -- --nocapture`

如果改动跨越多个边界，最后跑 `cargo test --all-targets`。

## 8. 常见开发流程

### 修改 HTTP/API

1. 先在 `tests/http_api.rs` 或 `tests/config_ui.rs` 写行为测试。
2. 在 `src/http.rs` 实现 handler、auth、rate limit 或 response shape。
3. 如果用户文档变化，同步 README。
4. 跑相关测试和 clippy。

### 修改 Telegram 行为

1. 先判断是 group decision、command、streaming 还是 worker。
2. 优先在 `src/channel/tests.rs` 或对应集成测试中加覆盖。
3. 保持 Telegram payload 细节在 channel 层，不要泄到 service。
4. 如果涉及真实联调，参考 `docs/real-test-minimax-telegram.md` 和 `scripts/debug_telegram_polling.sh`。

### 修改 Memory 或 Hybrid zvec

1. 先看 `docs/zvec-sidecar.md`。
2. schema 或查询变更要覆盖 `tests/memory_store.rs`。
3. hybrid 行为要覆盖 `tests/hybrid_memory_sidecar.rs`。
4. 注意 context budget、keyword fallback 和 `hybrid_min_score` 的行为不能静默变化。

### 修改 Agent Harness

1. 先看 `docs/agent-harness-eval.md`；历史演进背景见 `docs/plans/`，不要把旧 roadmap 当当前契约。
2. 被动功能和输出影响功能要分开测试、分开配置说明。
3. trajectory 字段变更要覆盖 storage 和 HTTP query。
4. compaction/verification 改动要跑 `tests/harness_eval.rs`。

### 修改 Loop Engineering

1. 先读 `docs/loop-engineering-harness.md` 与 `src/harness/loop_engine/domain.rs` 的状态/验证规则。
2. Goal 状态、approval hash、effect manifest、budget 或 checkpoint 语义变化必须覆盖 `tests/harness_loop_engine.rs`。
3. HTTP route、operator/ingest scope、event cursor 或 SSE 变化必须在
   `tests/http_loop_engine_api.rs` 增加对应覆盖；当前 SSE/disabled-mode 仍缺专项断言。
4. Provider frame 变化要覆盖 plain、Code Mode、MCP non-stream/stream 路径，并保证 payload 上限。
5. 与 Prompt Evolution 对接时 Artifact 只能保存 candidate/deployment reference fields，
   不得复制 Prompt；审批、激活和回滚仍由 Evolution API 完成。
6. 同步 `README.md`、`README.zh.md`、本 runbook、配置模板及 `tests/config_bootstrap.rs`。

### 修改 MCP 或 Skills

1. MCP 看 `docs/mcp-integration.md`。
2. 配置 merge、CLI command、runtime HTTP endpoint 分别加测试。
3. Code Mode 可调用 MCP 工具时，必须检查 `code_mode_capabilities`。
4. Skills 选择逻辑要覆盖 exact match、semantic match、off/always mode。

## 9. 文档索引

- `README.md` / `README.zh.md`：使用入口、Quick Start、HTTP API、配置速览。
- `AGENTS.md`：给 coding agent 的构建、测试、架构速览。
- `docs/engineering-quality.md`：质量门禁、架构边界、性能基线。
- `docs/real-test-minimax-telegram.md`：真实 MiniMax + Telegram 联调。
- `docs/mcp-integration.md`：MCP 安装、管理和 runtime HTTP API。
- `docs/code-mode-observability.md`：Code Mode 诊断和 Prometheus 指标。
- `docs/agent-harness-eval.md`：harness eval subset 的运行方式。
- `docs/self-evolving-harness.md`：Prompt candidate 的 shadow eval、人工激活与回滚。
- `docs/loop-engineering-harness.md`：Durable Goal、恢复、Signal、Self-test、Replay、Artifact 与 Desktop API。
- `docs/zvec-sidecar.md`：zvec sidecar 协议和 hybrid memory。
- `docs/plans/`：历史设计和路线图，不一定代表当前未完成事项。

## 10. PR Checklist

提交前确认：

- 改动范围清楚，没有带入本地密钥、runtime config 或验证产物。
- 用户可见行为已同步 README/README.zh。
- 新配置项有默认值、模板值和 bootstrap 测试。
- 新 API 有 auth、rate limit、错误路径测试。
- 新存储字段有 schema、读写、查询测试。
- Loop Engine 改动保持 approval binding、lease fencing、checkpoint reconciliation 与 provider budget 不变量。
- 外部 Signal scope、Self-test 只读边界、Replay 零 live-tool 边界未被放宽。
- `cargo fmt --all --check` 通过。
- `cargo clippy --all-targets -- -D warnings` 通过。
- `cargo test --all-targets` 通过。

## 11. 本地注意事项

- 不要提交 `.env.realtest`、真实 token 或私有 runtime config。
- `config/.xiaomaolv.minimax-telegram.runtime.toml` 是脚本生成的 runtime config，不应作为功能改动提交。
- `.factory/validation/...` 是本地验证产物；除非任务明确要求，否则不要纳入 PR。
