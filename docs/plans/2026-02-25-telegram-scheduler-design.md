# Telegram 定时任务（自由设置）Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 Telegram bot 支持“用户自然语言创建/修改/取消定时任务”，并由后台稳定执行（一次性与周期任务），同时保证权限、幂等、可观测与可回滚。

**Architecture:** 在现有 `channel.rs + memory.rs + service.rs` 架构上新增一层 `scheduler`：`任务存储（SQLite）` + `触发计算（once/cron）` + `后台 worker 执行器` + `自然语言意图解析`。命令路径和自然语言路径共用同一任务模型，避免双系统维护。

**Tech Stack:** Rust, Tokio, SQLx(SQLite), Serde, Tracing, Axum, `chrono` + `chrono-tz` + `cron`（新增依赖）。

---

## Approach Options

### Option A: 仅命令式（`/task ...`）
- 优点：稳定、可控、实现快。
- 缺点：不够“自由”，自然语言体验弱。

### Option B: 仅自然语言自动创建
- 优点：体验最好。
- 缺点：误触发风险高，debug 成本高。

### Option C（推荐）: 双通道 + 单模型
- 命令式用于强确定性操作（list/pause/resume/delete）。
- 自然语言用于“创建/更新建议”，默认二次确认。
- 底层共用一套 scheduler 表和执行器。

---

### Task 1: 建立任务数据模型与表结构

**Files:**
- Modify: `src/memory.rs`
- Modify: `src/service.rs`
- Create: `tests/scheduler_store.rs`

**Step 1: 新增 SQLite 表**
- `telegram_scheduler_jobs`
- `telegram_scheduler_runs`
- `telegram_scheduler_pending_intents`（自然语言确认态）

建议字段（核心）：
- `job_id TEXT PRIMARY KEY`
- `channel TEXT NOT NULL`（固定 `telegram`，为后续多渠道预留）
- `chat_id INTEGER NOT NULL`
- `message_thread_id INTEGER NULL`
- `owner_user_id INTEGER NOT NULL`
- `status TEXT NOT NULL`（active|paused|completed|canceled）
- `task_kind TEXT NOT NULL`（reminder|agent）
- `payload TEXT NOT NULL`（reminder 文本或 agent prompt）
- `schedule_kind TEXT NOT NULL`（once|cron）
- `timezone TEXT NOT NULL`
- `run_at_unix INTEGER NULL`（once）
- `cron_expr TEXT NULL`（cron）
- `next_run_at_unix INTEGER NULL`
- `last_run_at_unix INTEGER NULL`
- `last_error TEXT NULL`
- `run_count INTEGER NOT NULL DEFAULT 0`
- `max_runs INTEGER NULL`
- `lease_token TEXT NULL`
- `lease_until_unix INTEGER NULL`
- `created_at INTEGER NOT NULL`
- `updated_at INTEGER NOT NULL`

**Step 2: 为内存层增加 scheduler CRUD/claim API**
- `create_job`
- `list_jobs_by_owner`
- `pause_job`
- `resume_job`
- `cancel_job`
- `claim_due_jobs(now, limit, lease_secs)`
- `complete_job_run`
- `fail_job_run`

**Step 3: 写 store 单元测试**
Run: `cargo test scheduler_store -- --nocapture`
Expected:
- 创建/查询/暂停/恢复/删除通过
- claim 只返回到期且未被 lease 的任务
- 任务执行完成后 next_run 正确推进

---

### Task 2: 增加 Telegram Scheduler 配置面

**Files:**
- Modify: `src/config.rs`
- Modify: `src/http.rs`
- Modify: `config/xiaomaolv.example.toml`
- Modify: `config/xiaomaolv.minimax-telegram.toml`
- Modify: `README.md`
- Modify: `README.zh.md`
- Modify: `tests/config_bootstrap.rs`

**Step 1: 在 `TelegramChannelConfig` 新增字段（并提供默认值）**
- `scheduler_enabled`（default: `true`）
- `scheduler_tick_secs`（default: `2`）
- `scheduler_batch_size`（default: `8`）
- `scheduler_lease_secs`（default: `30`）
- `scheduler_default_timezone`（default: `Asia/Shanghai`）
- `scheduler_nl_enabled`（default: `true`）
- `scheduler_nl_min_confidence`（default: `0.78`）
- `scheduler_require_confirm`（default: `true`）
- `scheduler_max_jobs_per_owner`（default: `64`）

**Step 2: 配置透传到 telegram channel settings**
- 在 `http.rs` 的 `load_channel_plugins` 注入以上 key。

**Step 3: 配置解析测试**
Run: `cargo test config_bootstrap -- --nocapture`
Expected:
- 默认值正确
- 支持 `${ENV:-default}` 占位符

---

### Task 3: 增加 Scheduler 领域服务

**Files:**
- Create: `src/scheduler.rs`
- Modify: `src/lib.rs`
- Modify: `src/service.rs`
- Create: `tests/scheduler_domain.rs`

**Step 1: 建立领域类型**
- `SchedulerJob`
- `ScheduleSpec`（Once/Cron）
- `TaskKind`（Reminder/Agent）
- `JobExecutionResult`

**Step 2: 时间计算逻辑**
- `compute_next_run(spec, now, timezone)`
- once：首次到点后 `completed`
- cron：计算下一个触发时间

**Step 3: 失败重试策略**
- 指数退避：`min(2^n, 300s)`
- 写入 `last_error`
- 超过阈值进入 `paused`（防止错误风暴）

**Step 4: 测试**
Run: `cargo test scheduler_domain -- --nocapture`
Expected:
- once/cron next_run 计算正确
- DST 切换不出现倒退/重复执行

---

### Task 4: 在 Telegram 背景 worker 挂载定时执行循环

**Files:**
- Modify: `src/channel.rs`
- Create: `tests/telegram_scheduler_worker.rs`

**Step 1: 在 `start_background` 增加 scheduler loop**
- 无论 `polling` 还是 `webhook`，只要 `scheduler_enabled=true` 都运行。
- 与 polling 解耦：使用子任务并行，避免 `getUpdates` 长轮询阻塞定时触发。

**Step 2: 执行路径**
- 每个 tick：claim due jobs -> 执行 -> 更新状态。
- `task_kind=reminder`：直接发消息。
- `task_kind=agent`：构造 `IncomingMessage` 调 `service.handle(...)` 得到正文再发送。

**Step 3: 幂等与并发控制**
- lease + run 记录，防止多实例重复发送。
- 每 tick 最大执行数受 `scheduler_batch_size` 限制。

**Step 4: 测试**
Run: `cargo test telegram_scheduler_worker -- --nocapture`
Expected:
- 到期任务被执行一次
- pause/cancel 任务不执行
- 失败后按退避重试

---

### Task 5: 命令通道（强确定性管理）

**Files:**
- Modify: `src/channel.rs`
- Modify: `README.md`
- Modify: `README.zh.md`
- Create: `tests/telegram_task_commands.rs`

**Step 1: 扩展命令枚举与解析**
- `TelegramSlashCommand::Task { tail: String }`
- 支持 `/task` 子命令：
  - `/task add <time_expr> | <content>`
  - `/task every <cron> | <content>`
  - `/task list`
  - `/task pause <job_id>`
  - `/task resume <job_id>`
  - `/task del <job_id>`
  - `/task tz <IANA_TZ>`

**Step 2: 权限策略**
- 与现有私聊管理员策略一致：
  - 私聊且 `TELEGRAM_ADMIN_USER_IDS` 命中才可管理任务。

**Step 3: 帮助文案与 BotCommand 注册**
- 更新 `/help` 和 `setMyCommands`。

**Step 4: 测试**
Run: `cargo test telegram_task_commands -- --nocapture`
Expected:
- 参数错误有明确提示
- 非管理员私聊直接拒绝
- 正常命令路径返回任务摘要

---

### Task 6: 自然语言“自由设置”通道（上下文感知）

**Files:**
- Modify: `src/channel.rs`
- Modify: `src/service.rs`
- Create: `tests/scheduler_intent_parse.rs`

**Step 1: 意图识别协议（JSON-only）**
在 `service.rs` 新增一个轻量解析调用（可复用 provider）：
- 输入：用户原话 + 时区 + 当前时间。
- 输出（严格 JSON）：
```json
{
  "action": "create|update|cancel|list|none",
  "confidence": 0.0,
  "task_kind": "reminder|agent",
  "payload": "...",
  "schedule_kind": "once|cron",
  "run_at": "2026-02-26T09:00:00+08:00",
  "cron_expr": "0 9 * * *",
  "timezone": "Asia/Shanghai"
}
```

**Step 2: 触发规则**
- 仅在私聊生效。
- `confidence >= scheduler_nl_min_confidence` 才进入 scheduler 处理。
- `scheduler_require_confirm=true` 时，先回显“任务草案”，用户回复 `确认` 才落库。

**Step 3: 实时纠正支持**
- 用户回复“不是每周一，是每周二 9 点”时，匹配到待确认草案并覆盖更新。

**Step 4: 测试**
Run: `cargo test scheduler_intent_parse -- --nocapture`
Expected:
- 高置信度创建成功
- 低置信度回落为普通对话
- 二次确认流程可完成/取消

---

### Task 7: 诊断、风控与可观测性

**Files:**
- Modify: `src/channel.rs`
- Modify: `README.md`
- Modify: `README.zh.md`

**Step 1: 扩展 `/v1/channels/telegram/diag`**
增加：
- `scheduler_enabled`
- `scheduler_tick_secs`
- `jobs_total`
- `jobs_active`
- `jobs_due`
- `scheduler_runs_ok_total`
- `scheduler_runs_err_total`
- `last_scheduler_error`

**Step 2: 风控策略**
- 同一 owner 的任务上限（`scheduler_max_jobs_per_owner`）。
- 同一 chat 的最小发送间隔（防止定时洪泛）。
- payload 长度与 markdown 兼容校验（沿用现有 MarkdownV2 渲染）。

**Step 3: 回滚开关**
- 一键回滚：`scheduler_enabled=false`（不删数据，仅停执行器）。

---

### Task 8: 端到端验收

**Files:**
- Create: `tests/telegram_scheduler_e2e.rs`

**Step 1: 场景 1（一次性提醒）**
- 私聊管理员发：“明天早上9点提醒我开会”。
- 预期：生成待确认 -> 确认后入库 -> 到期发送一次 -> 状态 completed。

**Step 2: 场景 2（周期任务）**
- “每周一 10:00 给我发本周待办总结”。
- 预期：按 cron 周期发送。

**Step 3: 场景 3（纠正）**
- “改成每周二 9 点”。
- 预期：同一 job 更新 next_run。

**Step 4: 场景 4（权限）**
- 非管理员私聊尝试建任务。
- 预期：拒绝并返回无权限提示。

Run: `cargo test telegram_scheduler_e2e -- --nocapture`
Expected: 全部通过。

---

## Acceptance Criteria

- 用户可在私聊中通过自然语言创建/修改/取消定时任务。
- 管理命令 `/task ...` 全量可用，且权限与当前 admin 策略一致。
- 任务执行具备幂等保护（lease + run 记录），不会重复触发。
- once/cron 都支持，时区可配置，DST 下行为可预测。
- 失败重试、暂停、恢复、删除完整可用。
- 诊断接口可直接看 scheduler 健康状态与错误原因。
- 关闭 `scheduler_enabled` 可立即停用且无需删库。
