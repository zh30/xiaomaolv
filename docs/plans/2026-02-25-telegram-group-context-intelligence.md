# Telegram Group Context Intelligence Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make Telegram group behavior more human-like: the bot should speak when context strongly implies it is being addressed, and stay silent (but still observe) when it should not interrupt.

**Architecture:** Introduce a new group trigger decision layer in the Telegram channel pipeline with three outcomes: `Respond`, `ObserveOnly`, `Ignore`. Start with deterministic rule-based scoring, then optionally add a low-cost LLM arbitration step only for ambiguous cases. Keep strict mode as default for safe rollout.

**Tech Stack:** Rust, Tokio, Axum, Serde, SQLx, Tracing.

---

## Approach Options

### Option A: Rule-only (fastest, cheapest)
- Pros: deterministic, low latency, no extra API cost, easy rollback.
- Cons: edge cases are harder to capture; may still feel rigid in some group dynamics.

### Option B: LLM-only decision (highest flexibility)
- Pros: best semantic understanding.
- Cons: higher cost/latency, less predictable, harder to debug.

### Option C (Recommended): Hybrid decision
- Rule engine handles clear cases.
- Optional LLM arbitration runs only when confidence is in a gray zone.
- Best balance of cost, control, and human-like behavior.

---

### Task 1: Baseline Observability Before Behavior Change

**Files:**
- Modify: `src/channel.rs`
- Modify: `README.md`
- Modify: `README.zh.md`

**Step 1: Add structured trigger-decision logging**
- Add reason codes for current behavior (`mentioned`, `replied_to_bot`, `sender_is_bot`, `skip_no_trigger`).

**Step 2: Add diagnostics counters**
- Extend `/v1/channels/telegram/diag` with counters:
  - `group_messages_total`
  - `group_decision_respond_total`
  - `group_decision_observe_total`
  - `group_decision_ignore_total`

**Step 3: Verify**
- Run: `cargo test channel -- --nocapture`
- Expected: existing tests pass; diagnostics JSON includes new counters.

### Task 2: Add Config Surface for Smart Group Trigger

**Files:**
- Modify: `src/config.rs`
- Modify: `src/http.rs`
- Modify: `config/xiaomaolv.example.toml`
- Modify: `config/xiaomaolv.minimax-telegram.toml`
- Modify: `tests/config_bootstrap.rs`
- Modify: `README.md`
- Modify: `README.zh.md`

**Step 1: Add new Telegram config fields**
- `group_trigger_mode` (`strict` | `smart`, default `strict`)
- `group_followup_window_secs` (default `180`)
- `group_cooldown_secs` (default `20`)
- `group_rule_min_score` (default `70`)
- `group_aliases` (CSV string, optional)
- `group_llm_gate_enabled` (default `false`)

**Step 2: Pass config through channel plugin settings**
- Wire all fields from `channels.telegram` into `ChannelPluginConfig.settings`.

**Step 3: Verify**
- Run: `cargo test config_bootstrap -- --nocapture`
- Expected: parse tests cover defaults and explicit overrides.

### Task 3: Implement Group Decision Engine (Rule Layer)

**Files:**
- Modify: `src/channel.rs`
- Modify: `tests/telegram_channel_mode.rs` (or new `tests/telegram_group_decision.rs`)

**Step 1: Create decision model**
- Add internal decision types:
  - `GroupDecisionKind` = `Respond | ObserveOnly | Ignore`
  - `GroupDecision { kind, score, reasons }`

**Step 2: Implement signal extraction**
- Signals:
  - explicit `@bot` mention
  - reply-to-bot
  - recent bot participation in same thread (within `group_followup_window_secs`)
  - alias hit (`group_aliases`)
  - explicit question markers (`?`, `？`)
  - negative signals: message from bot, clear mention to another bot/user, very low-content noise

**Step 3: Replace hard-coded condition**
- Current: `accepted = mentioned || replied_to_bot`
- New: call `evaluate_group_decision(...)` and branch:
  - `Respond` -> existing service path
  - `ObserveOnly` -> do not reply, record observation signal
  - `Ignore` -> skip

**Step 4: Verify**
- Run: `cargo test telegram_group -- --nocapture`
- Expected: new tests prove mode behavior:
  - strict mode remains old behavior
  - smart mode can respond on follow-up without explicit `@`
  - smart mode suppresses unrelated chatter

### Task 4: Implement “Silent Detection” Path

**Files:**
- Modify: `src/channel.rs`
- Modify: `src/service.rs`
- Modify: `src/domain.rs` (if metadata needed)
- Modify: `tests/service_pipeline.rs` (or new focused tests)

**Step 1: Add lightweight observe API**
- Add `MessageService::observe(incoming)` to persist user turn only (no assistant generation).

**Step 2: Avoid polluting normal dialogue context**
- Store observe-only events under a shadow session ID suffix (for example: `:observe`), or add a role/tag marker.

**Step 3: Wire ObserveOnly branch**
- Group messages judged as `ObserveOnly` call `observe(...)` and exit with no outbound Telegram message.

**Step 4: Verify**
- Run: `cargo test service_pipeline -- --nocapture`
- Expected: observe-only path persists user input and never sends assistant output.

### Task 5: Optional LLM Arbitration for Gray-Zone Cases

**Files:**
- Modify: `src/service.rs`
- Modify: `src/channel.rs`
- Modify: `tests/service_pipeline.rs`

**Step 1: Add tiny decision prompt contract**
- Input: latest message + compact context summary.
- Output JSON only:
  - `{"speak": true|false, "confidence": 0..1, "reason": "..."}`

**Step 2: Gate usage**
- Run only when rule score is near threshold.
- Skip when `group_llm_gate_enabled=false`.
- Hard timeout and fallback to rule-only decision.

**Step 3: Verify**
- Run: `cargo test service_pipeline -- --nocapture`
- Expected: failures in LLM arbitration do not break message handling.

### Task 6: Add Safety Guards Against Over-Talking

**Files:**
- Modify: `src/channel.rs`
- Modify: `README.md`
- Modify: `README.zh.md`

**Step 1: Add per-chat cooldown**
- Enforce `group_cooldown_secs` between autonomous responses.

**Step 2: Add consecutive-reply guard**
- Prevent bot from sending repeated unsolicited responses without new strong signal.

**Step 3: Add final fallback**
- If safety checks conflict, choose `ObserveOnly` instead of `Respond`.

**Step 4: Verify**
- Run: `cargo test telegram_group -- --nocapture`
- Expected: cooldown and guard behavior validated by unit tests.

### Task 7: Rollout and Quality Gates

**Files:**
- Modify: `README.md`
- Modify: `README.zh.md`
- Create: `docs/telegram-group-rollout.md`

**Step 1: Controlled rollout strategy**
- Week 1: `strict` + observability only.
- Week 2: `smart` for 1-2 pilot groups.
- Week 3: enable smart mode broadly if metrics are healthy.

**Step 2: Define success metrics**
- `false_positive_rate` (bot interrupts when not needed)
- `miss_rate` (bot should have responded but stayed silent)
- median response latency
- token/cost delta after rollout

**Step 3: Define rollback**
- One-switch rollback: set `group_trigger_mode = "strict"` and restart service.

---

## Acceptance Criteria

- Private chat behavior remains unchanged.
- In group chat, explicit `@bot` and reply-to-bot still always work.
- In smart mode, relevant contextual follow-ups can trigger responses without explicit `@`.
- In smart mode, unrelated chatter is mostly `ObserveOnly` or `Ignore`.
- Diagnostics endpoint can explain why a message was responded to or skipped.
- Strict mode can be restored immediately without code rollback.
