# xiaomaolv (小毛驴)

一个 Rust 实现的高性能 AI 网关。目标是让你只配置 Provider 和消息通道，就能快速跑起来。

## 目录

- [你可以直接得到什么](#features)
- [快速开始（推荐：MiniMax + Telegram）](#quick-start)
- [文档导航](#docs-index)
- [运行模式说明](#run-modes)
- [配置速览](#config-overview)
- [可选：启用混合记忆（SQLite + zvec sidecar）](#hybrid-memory)
- [HTTP API](#http-api)
- [插件扩展（Provider / Channel）](#plugin-system)
- [本地开发](#local-dev)
- [性能冒烟测试](#perf-smoke)
- [安全建议（开源前必看）](#security)

<a id="features"></a>
## 你可以直接得到什么

- OpenAI-compatible Provider 抽象（MiniMax/OpenAI/其他兼容接口）
- 消息通道：HTTP + Telegram
- Telegram 双模式：`polling`（默认）和 `webhook`（可选）
- Telegram 流式回复（通过 `editMessageText` 增量更新）
- Telegram 回复统一使用 `MarkdownV2` 渲染（支持加粗/斜体/代码/链接/列表/引用）
- Telegram 启动在线状态（通过 `setMyShortDescription`，可配置）
- Telegram `/` 命令：`/start`、`/help`、`/whoami`、`/mcp ...`（私聊管理员控制）
- Telegram 群组支持：
  - `strict` 模式：仅在 `@bot_username` 或 reply-to-bot 时回复
  - `smart` 模式：基于上下文规则的 `Respond/ObserveOnly/Ignore` 决策
- 群组里如果用户 reply 了 bot 消息，bot 会引用该用户消息继续回复（`reply_to_message_id`）
- 群组会话分组：优先按 `message_thread_id`（话题），否则按 `reply_to_message_id`
- Telegram 回复会剥离 `<think>...</think>`，仅发送正文
- 记忆系统：`sqlite-only`（默认）和 `hybrid-sqlite-zvec`（可选）
- 插件式扩展 API（Provider/Channel/Memory）

<a id="quick-start"></a>
## 快速开始（推荐：MiniMax + Telegram）

### 1) 准备环境

- Rust（建议 stable）
- Telegram Bot Token（来自 `@BotFather`）
- MiniMax API Key（OpenAI 兼容接口）

### 2) 填写环境变量

```bash
cp .env.realtest.example .env.realtest
```

编辑 `.env.realtest`，至少填这两个值：

- `MINIMAX_API_KEY`
- `TELEGRAM_BOT_TOKEN`

可选模型覆盖：

- `MINIMAX_MODEL`（默认：`MiniMax-M2.5-highspeed`）
- `TELEGRAM_BOT_USERNAME`（不带 `@`，建议填写，用于群组@匹配）
- `TELEGRAM_ADMIN_USER_IDS`（私聊访问 + `/mcp` 管理员用户 ID，逗号分隔，如 `123456789,987654321`）

### 3) 一键启动 MVP

```bash
./scripts/run_mvp_minimax_telegram.sh
```

开发热重载模式（代码变更后自动重编译并重启）：

```bash
./scripts/run_mvp_minimax_telegram.sh --hot-reload
```

脚本会自动使用：

- 配置文件：`config/xiaomaolv.minimax-telegram.toml`
- 数据库：`sqlite://xiaomaolv.db`
- Telegram 模式：`polling`（不需要公网 URL）

### 4) 验证服务

```bash
curl -sS http://127.0.0.1:8080/health
curl -sS http://127.0.0.1:8080/v1/channels/telegram/mode
```

然后直接在 Telegram 给你的 Bot 发消息即可。

<a id="docs-index"></a>
## 文档导航

如果你已经能跑通 MVP，下面这些文档按使用频率排列：

- `docs/real-test-minimax-telegram.md`：真实 MiniMax + Telegram 联调（含 webhook 配置与验证）
- `docs/zvec-sidecar.md`：zvec sidecar 协议、启动方式与兼容行为
- `config/xiaomaolv.minimax-telegram.toml`：MVP 推荐配置（可直接拷贝改值）
- `config/xiaomaolv.example.toml`：通用模板（适合自定义 Provider/Channel）
- `scripts/perf_smoke.sh`：机器性能冒烟测试脚本（评估最低部署规格）

<a id="run-modes"></a>
## 运行模式说明

### Telegram `polling`（默认）

- 不需要公网地址
- 服务内部通过 `getUpdates` 拉取消息
- 启动时会先 `deleteWebhook` 避免 webhook/polling 冲突

### Telegram `webhook`（可选）

适用于生产公网部署：

1. 在配置中启用 webhook：

```toml
[channels.telegram]
enabled = true
bot_token = "${TELEGRAM_BOT_TOKEN}"
bot_username = "${TELEGRAM_BOT_USERNAME:-}"
mode = "webhook"
webhook_secret = "${TELEGRAM_WEBHOOK_SECRET}"
streaming_enabled = true
streaming_edit_interval_ms = 900
startup_online_enabled = true
startup_online_text = "${TELEGRAM_STARTUP_ONLINE_TEXT:-online}"
commands_enabled = true
commands_auto_register = true
commands_private_only = true
admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"
group_trigger_mode = "${TELEGRAM_GROUP_TRIGGER_MODE:-smart}"
group_followup_window_secs = 180
group_cooldown_secs = 20
group_rule_min_score = 70
group_llm_gate_enabled = false
scheduler_enabled = true
scheduler_tick_secs = 2
scheduler_batch_size = 8
scheduler_lease_secs = 30
scheduler_default_timezone = "${TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE:-Asia/Shanghai}"
scheduler_nl_enabled = true
scheduler_nl_min_confidence = 0.78
scheduler_require_confirm = true
scheduler_max_jobs_per_owner = 64
```

2. 在 `.env.realtest` 中补齐：

- `TELEGRAM_WEBHOOK_SECRET`
- `PUBLIC_BASE_URL`（公网 HTTPS 地址）

3. 注册 webhook：

```bash
set -a
source .env.realtest
set +a
./scripts/set_telegram_webhook.sh
```

Webhook 回调地址：

`POST /v1/telegram/webhook/{webhook_secret}`

<a id="config-overview"></a>
## 配置速览

默认示例文件：

- `config/xiaomaolv.example.toml`（通用模板）
- `config/xiaomaolv.minimax-telegram.toml`（MVP 模板）

关键项：

- Provider：`[providers.<name>]`
- 默认 Provider：`[app].default_provider`
- MiniMax 模型（MVP 模板）：`model = "${MINIMAX_MODEL:-MiniMax-M2.5-highspeed}"`
- Telegram 流式：
  - `streaming_enabled = true`
  - `streaming_edit_interval_ms = 900`
  - `bot_username = "your_bot_username"`（用于@匹配与群组决策上下文）
  - `group_trigger_mode = "${TELEGRAM_GROUP_TRIGGER_MODE:-smart}"`（`strict` 仅@/reply触发；`smart` 上下文触发；默认 `smart`）
  - `group_followup_window_secs = 180`（smart 模式下“最近上下文”窗口）
  - `group_cooldown_secs = 20`（smart 模式自动发言冷却）
  - `group_rule_min_score = 70`（smart 规则阈值）
  - `smart` 模式会基于群上下文自动学习召唤别名（不需要手工配置）
  - `group_llm_gate_enabled = false`（预留：灰区判定开关）
  - 定时任务：
    - `scheduler_enabled = true`
    - `scheduler_tick_secs = 2`
    - `scheduler_batch_size = 8`
    - `scheduler_lease_secs = 30`
    - `scheduler_default_timezone = "${TELEGRAM_SCHEDULER_DEFAULT_TIMEZONE:-Asia/Shanghai}"`
    - `scheduler_nl_enabled = true`
    - `scheduler_nl_min_confidence = 0.78`
    - `scheduler_require_confirm = true`
    - `scheduler_max_jobs_per_owner = 64`
    - 私聊管理员命令：`/task list|add|every|pause|resume|del`
    - 当 `scheduler_nl_enabled=true` 时，支持私聊自然语言创建/修改定时任务
    - 当 `scheduler_require_confirm=true` 时，先生成草案，等待用户回复“确认”或“取消”
    - 自然语言也支持管理已有任务（暂停/恢复/删除，需带任务 ID，例如：`暂停任务 task-...`）
  - `startup_online_enabled = true|false`（启动时写入 Bot 在线状态文案）
  - `startup_online_text = "online"`（调用 Telegram `setMyShortDescription`）
  - `commands_enabled = true|false`（是否启用 `/` 命令处理）
  - `commands_auto_register = true|false`（启动时是否自动注册 Telegram 命令菜单）
  - `commands_private_only = true|false`（为 true 时 `/mcp` 仅允许私聊）
  - `admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"`（私聊访问 + `/mcp` 白名单，推荐在 `.env.realtest` 配置）
- 记忆模式：
  - `backend = "sqlite-only"`（默认）
  - `backend = "hybrid-sqlite-zvec"`（可选）

<a id="hybrid-memory"></a>
## 可选：启用混合记忆（SQLite + zvec sidecar）

### 一键方式

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory
```

混合记忆 + 热重载：

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory --hot-reload
```

### 手动方式

```bash
./scripts/run_zvec_sidecar.sh
```

详见：`docs/zvec-sidecar.md`

<a id="http-api"></a>
## HTTP API

核心接口：

- `GET /health`
- `POST /v1/messages`
- `GET /v1/channels/{channel}/mode`
- `POST /v1/channels/{channel}/inbound`
- `POST /v1/channels/{channel}/inbound/{secret}`
- `POST /v1/telegram/webhook/{secret}`

示例：

```bash
curl -X POST http://127.0.0.1:8080/v1/messages \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo-1","user_id":"u1","text":"你好"}'
```

<a id="plugin-system"></a>
## 插件扩展（Provider / Channel）

你可以像传统插件系统一样扩展：

- Provider：实现 `ProviderFactory`，注册到 `ProviderRegistry`
- Channel：实现 `ChannelFactory`，注册到 `ChannelRegistry`
- 自定义路由构建：`build_router_with_registries(...)`
- 需要后台 worker 的场景：`build_app_runtime_with_registries(...)`

参考测试：

- `tests/provider_plugin_api.rs`
- `tests/channel_plugin_api.rs`
- `tests/telegram_channel_mode.rs`

<a id="local-dev"></a>
## 本地开发

```bash
cargo fmt --all
cargo test -- --nocapture
```

<a id="perf-smoke"></a>
## 性能冒烟测试

快速评估“这台机器是否能稳跑 xiaomaolv”：

```bash
./scripts/perf_smoke.sh
```

该脚本会：

- 自动启动本地 mock provider（不依赖真实 AI Key）
- 自动启动 `xiaomaolv`（release 模式）
- 压测 `/health` 和 `/v1/messages`
- 输出吞吐、延迟、失败率与机器建议规格

压测已运行服务：

```bash
./scripts/perf_smoke.sh --running http://127.0.0.1:8080
```

自定义压力参数：

```bash
MSG_C=32 MSG_N=2000 HEALTH_C=200 ./scripts/perf_smoke.sh
```

<a id="security"></a>
## 安全建议（开源前必看）

- 不要提交任何真实密钥文件（如 `.env.realtest`）
- 使用模板文件：`.env.realtest.example`
- 如果密钥曾出现在本地仓库历史中，请先轮换密钥再开源
