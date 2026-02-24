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
- `<think>...</think>` 自动渲染为 Telegram 可折叠 spoiler
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

### 3) 一键启动 MVP

```bash
./scripts/run_mvp_minimax_telegram.sh
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
mode = "webhook"
webhook_secret = "${TELEGRAM_WEBHOOK_SECRET}"
streaming_enabled = true
streaming_edit_interval_ms = 900
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
- Telegram 流式：
  - `streaming_enabled = true`
  - `streaming_edit_interval_ms = 900`
- 记忆模式：
  - `backend = "sqlite-only"`（默认）
  - `backend = "hybrid-sqlite-zvec"`（可选）

<a id="hybrid-memory"></a>
## 可选：启用混合记忆（SQLite + zvec sidecar）

### 一键方式

```bash
./scripts/run_mvp_minimax_telegram.sh --hybrid-memory
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
