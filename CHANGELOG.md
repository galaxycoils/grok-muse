# Changelog

All notable changes to **Grok-Muse** are documented in this file.

## [v0.3.0] — OpenBot Multi-Agent & Coworker Suite (2026-11-03)

### Added
- **Persistent Multi-Bot Roster & Isolated Workspaces (`crates/gm-openbot`)**:
  - Default AI coworkers (`@muse`, `@researcher`, `@coder`, `@guardian`) plus custom bot profiles (`gm bots list|show|create|rm`).
  - Per-bot isolated workspace directories (`<storage_dir>/bots/<bot_id>/{workspace,inbox,outbox}`) with `0700` permissions and dedicated SQLite FTS5 memory stores.
- **Crash-Safe SQLite WAL FIFO Task Queues (`crates/gm-openbot/src/queue.rs`)**:
  - Durable priority + FIFO task scheduling (`gm queue list|add|pause|resume|cancel|reassign|backup|restore`), crash recovery of interrupted in-flight tasks, and JSONL backup/restore.
- **Multi-Bot Channels, A2A Handoffs & Context Compaction**:
  - Persistent `#general`, `#research`, and `#ops` channels (`gm channels`) with automatic `@<bot_id>` mention parsing and task spawning.
  - Structured Agent-to-Agent (`A2aCoordinator`) artifact handoffs copying deliverables from `<sender>/outbox/` to `<recipient>/inbox/` with loop-depth guards (`max_handoff_depth = 4`).
  - `ContextCompactor` token-budget thread summarization.
- **macOS Kernel Seatbelt (`sandbox-exec`) Confinement & Governance Gateway**:
  - `BotWorkspaceSandbox` (`ReadFile`, `WriteFile` with 200-line unified diff preview, `ListFiles`, `Shell` under `/usr/bin/sandbox-exec`).
  - `SecurityGateway` policy evaluation (`RULE-BLK-001` through `RULE-SEC-006`) and structured `GovernanceAuditRecord` logging (`Permitted`, `Refused`, `Failed`).
- ***"Take the Wheel"* Human Handoff & Opaque Vault Secrets**:
  - Automatic MFA/2FA/CAPTCHA blocker detection (`BlockerDetector`), `WaitingForHuman` state transition, and native Cocoa `🛞 Take the Wheel` sheet + HUD indicator.
  - `VaultSecretResolver` (`secret://keychain/<account>` and `secret://1password/<vault>/<item>/<field>`) + `SecretScrubber` ensuring zero plaintext secret leaks.
- **Loopback Companion Web / AG-UI SSE Server & Chat Bridges**:
  - Built-in `axum` server (`gm serve`, default `127.0.0.1:7433`) serving the Companion Web UI (`/`), Governance Audit Dashboard (`/admin/audit`), A2A Agent Card (`/.well-known/agent.json`), AG-UI SSE stream (`/ag-ui/events`), and HMAC-SHA256 verified Slack / Telegram / Discord webhook bridges.
- **`v0.3.0` OpenBot Evaluation Suite (`crates/gm-eval/src/openbot_suite.rs`)**:
  - 5-scenario automated verification suite (`ob_queue_01`, `ob_sandbox_02`, `ob_wheel_03`, `ob_a2a_04`, `ob_agui_05`) integrated into `xtask eval` and `xtask test-all`.

---

## [v0.2.0] — Deep Runtime, SoM Vision, Candle Metal VLM & MCP Bridge (2026-11-03)

### Added
- **Set-of-Marks (SoM) Vision Grounding & Stagnation Escalation (`crates/gm-agent`)**:
  - Annotated screenshot compositor (`#1..#N` badges) and automatic AX digest stagnation escalation to `CuVisionStep`.
- **Native Candle Metal Local VLM Engine (`crates/gm-vlm-candle`)**:
  - Offline inference and HuggingFace Hub model management (`gm models list|pull|rm`) for `Qwen2-VL-2B-Instruct` and `SmolVLM2`.
- **POSIX Shared Memory (`shm_open` + `mmap`) Frame Ring & Event-Driven AX Observer (`crates/gm-platform`)**:
  - Zero-copy frame transport and `AXObserverCreate` cache invalidation.
- **Dual-Socket IPC Control Server, Interactive Cocoa Sheets & MCP Bridge**:
  - Live token streaming, interactive `NSAlert` approval sheets for `ActionTier::Irreversible`, and `darius-tools` MCP tool bridge.

---

## [v0.1.0] — Initial Certified macOS Release (2026-11-03)

### Added
- Native macOS computer-use agent (`GrokMuse.app` + `GrokMuseCUHelper.app`) with ScreenCaptureKit, Accessibility (`AXUIElement`), CoreGraphics input synthesis, EventKit Calendar/Reminders, SQLite FTS5 personal memory, proactive Quiet Gate engine, and 8-gate `xtask test-all` certification.
