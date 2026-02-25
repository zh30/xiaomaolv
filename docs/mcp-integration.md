# MCP Integration Guide

This document describes the current MCP integration in xiaomaolv, including installation, config scope model, and runtime APIs.

## 1. Design Goals

- Claude-style install ergonomics (`mcp add <name> -- <cmd> <args...>`)
- Keep onboarding simple (user-level defaults, project override when needed)
- Keep runtime explicit and observable (list servers/tools and call tools over HTTP)
- Keep compatibility for both stdio and HTTP transport MCP servers

## 2. CLI Commands

Main command group:

```bash
xiaomaolv mcp --help
```

### 2.1 Add server

Claude-style (recommended):

```bash
xiaomaolv mcp add tavily --scope user -- npx -y @tavily/mcp-server
```

Equivalent explicit style:

```bash
xiaomaolv mcp add tavily --scope user --command npx --arg -y --arg @tavily/mcp-server
```

HTTP transport:

```bash
xiaomaolv mcp add internal-http --transport http --url http://127.0.0.1:8787/mcp \
  --header "Authorization=Bearer <TOKEN>"
```

Notes:

- `--scope user` is default and recommended for personal machine setup.
- `--scope project` is useful for repository-local team setup.
- For stdio transport, use command fields only.
- For HTTP transport, use `--url` and optional `--header`.

### 2.2 List servers

```bash
xiaomaolv mcp ls --scope merged
xiaomaolv mcp ls --scope user
xiaomaolv mcp ls --scope project
```

### 2.3 Test server

```bash
xiaomaolv mcp test tavily
```

This runs MCP initialize + tools/list and prints discovered tools.

### 2.4 Remove server

```bash
xiaomaolv mcp rm tavily --scope all
```

`--scope` options: `all` (default), `user`, `project`.

### 2.5 Telegram `/mcp` commands

When Telegram command mode is enabled, bot command mapping is:

- `/whoami` (show current Telegram user ID for allowlist config)
- `/mcp ls [--scope merged|user|project]`
- `/mcp test <name>`
- `/mcp rm <name> [--scope all|user|project]`
- `/mcp add <name> ...` (supports the same flags as CLI, including `-- <command> <args...>`)

Defaults:

- `/mcp` only accepts private chat
- only users in `TELEGRAM_ADMIN_USER_IDS` can execute (`admin_user_ids = "${TELEGRAM_ADMIN_USER_IDS:-}"`)
- unknown private slash commands return help guidance

After successful `/mcp add` and `/mcp rm`, runtime MCP registry is hot reloaded immediately (no process restart required).

## 3. Config Scope and Precedence

### 3.1 Default file locations

- User scope: `$XDG_CONFIG_HOME/xiaomaolv/mcp.toml`
- On macOS default this resolves to `~/Library/Application Support/xiaomaolv/mcp.toml`
- Project scope: `./.xiaomaolv/mcp.toml`

### 3.2 Override via env

- `XIAOMAOLV_MCP_USER_CONFIG`
- `XIAOMAOLV_MCP_PROJECT_CONFIG`

### 3.3 Merge rule

When both scopes define the same server name, project scope wins.

## 4. Runtime HTTP API

After `xiaomaolv serve`, MCP runtime endpoints are available:

- `GET /v1/mcp/servers`
- `GET /v1/mcp/tools?server=<optional_name>`
- `POST /v1/mcp/tools/{server}/{tool}`

Examples:

```bash
curl -sS http://127.0.0.1:8080/v1/mcp/servers
curl -sS "http://127.0.0.1:8080/v1/mcp/tools?server=tavily"
curl -sS -X POST http://127.0.0.1:8080/v1/mcp/tools/tavily/search \
  -H 'content-type: application/json' \
  -d '{"query":"Rust async best practices"}'
```

## 5. Install Pattern Similarity to Claude Code

xiaomaolv follows the same core UX pattern:

- install a named MCP server once
- keep install scoped to user or project
- inspect with list/test commands
- use runtime tool calling without hand-editing large config blocks

Equivalent habit mapping:

- Claude-style install: `mcp add <name> -- <command> <args...>`
- xiaomaolv install: `xiaomaolv mcp add <name> -- <command> <args...>`

## 6. Agent Auto Tool Loop

xiaomaolv now includes built-in model-driven MCP tool orchestration in `MessageService`.

When enabled, the service:

1. loads tool inventory from MCP runtime
2. asks the model to emit strict JSON tool calls
3. executes MCP calls through runtime
4. injects tool results back into context
5. stops when model returns final answer or max loop count is reached

Related config:

```toml
[agent]
mcp_enabled = true
mcp_max_iterations = 4
mcp_max_tool_result_chars = 4000
```

Operational notes:

- streaming channels still work; when MCP loop path is active, final answer is emitted as one delta
- if MCP discovery fails, service falls back to plain provider completion
- to hard-disable tool orchestration, set `mcp_enabled = false`
