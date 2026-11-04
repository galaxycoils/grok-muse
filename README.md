# Grok-Muse Hybrid Agent (`v0.3.0` — OpenBot Multi-Agent & Coworker Suite)

> **Fast computer-use + proactive personal agent + OpenBot multi-agent coworker suite for macOS.**  
> Built in Rust with native Rust desktop rendering, macOS ScreenCaptureKit/Accessibility integration, scoped workspace file access, an offline workspace VM transport, Candle model support, and hybrid LLM routing (Meta Muse + xAI Grok + Local VLM).

The current branch adds a durable task dispatcher, native desktop client, persistent conversations, bot/channel editing, attachments, scheduled routines, durable usage accounting and authenticated update staging. Windows UI Automation/GDI/input and Linux X11/AT-SPI plus Wayland portal/PipeWire backends are implemented; native Windows/macOS and real Wayland certification remain outstanding. The VM transport requires a separately provisioned signed image. See [implementation status](docs/implementation-status.md) for implemented behavior, setup, and remaining public-release work. Performance/quality numbers below are targets, not certified results.

---

## 📑 Table of Contents
1. [Goal State & Metrics (G1–G8)](#goal-state--metrics-g1g8)
2. [What's New in `v0.3.0` (OpenBot Coworker Suite)](#whats-new-in-v030-openbot-coworker-suite)
3. [System Architecture](#system-architecture)
4. [Workspace Crates](#workspace-crates)
5. [Installation & Packaging (DMG)](#installation--packaging-dmg)
6. [LaunchAgent Service Management](#launchagent-service-management)
7. [macOS Onboarding & Permissions](#macos-onboarding--permissions)
8. [Keychain & Opaque Vault Secret Management](#keychain--opaque-vault-secret-management)
9. [CLI Command Reference](#cli-command-reference)
10. [OpenBot Multi-Agent Coworkers, Queue, Channels & Companion Web UI](#openbot-multi-agent-coworkers-queue-channels--companion-web-ui)
11. [Proactive Engine & Quiet Gate](#proactive-engine--quiet-gate)
12. [Memory Upkeep Engine](#memory-upkeep-engine)
13. [Desktop Shell & Keyboard Shortcuts](#desktop-shell--keyboard-shortcuts)
14. [Benchmarking & Evaluation (`xtask`)](#benchmarking--evaluation-xtask)
15. [Continuous Integration (CI)](#continuous-integration-ci)

---

## What's New in `v0.3.0` (OpenBot Coworker Suite)

Grok-Muse `v0.3.0` integrates the **OpenBot Multi-Agent & Coworker Suite** (`crates/gm-openbot`) alongside the `v0.2.0` Set-of-Marks (SoM) vision grounding, MCP tool bridge, and native Candle Metal VLM engine (`crates/gm-vlm-candle`):

1. **Persistent Multi-Bot Roster & Isolated Per-Bot Workspaces**:
   - Ships with 4 specialized AI teammates out of the box: `@muse` (*General Desktop Operator*), `@researcher` (*Deep Knowledge & Web Researcher*), `@coder` (*Sandboxed Software Engineer*), and `@guardian` (*Security & Governance Analyst*), plus custom bot creation via `gm bots create`.
   - Each bot gets an isolated directory tree (`<storage_dir>/bots/<bot_id>/{workspace,inbox,outbox}`) and dedicated SQLite FTS5 memory store.
2. **Crash-Safe SQLite WAL FIFO Task Queues**:
   - Priority-ordered durable task queues (`gm queue`) with `pause`, `resume`, `cancel`, `reassign`, JSONL `backup`/`restore`, and automatic crash recovery of interrupted in-flight tasks on daemon startup.
3. **Multi-Bot Channels, A2A Artifact Handoffs & Context Compaction**:
   - Persistent `#general`, `#research`, and `#ops` channels (`gm channels`) with automatic `@<bot_id>` mention parsing and task spawning.
   - Structured Agent-to-Agent (`A2aCoordinator`) handoffs that copy artifacts from `<from_bot>/outbox/` into `<to_bot>/inbox/` with loop-depth guards (`max_handoff_depth = 4`), paired with `ContextCompactor` token-budget summarization.
4. **Scoped Workspace Tools & Governance Gateway**:
   - Native file tools use directory capabilities. `WorkspaceShell` requires the separately provisioned offline VM transport; there is no host-shell fallback.
   - `SecurityGateway` evaluates every GUI, MCP, and workspace action against explicit policy rules (`RULE-BLK-001` through `RULE-SEC-006`) and logs `Permitted`, `Refused`, and `Failed` records with rule citations.
5. ***"Take the Wheel"* Human Handoff & Opaque Vault Secrets**:
   - Detects 2FA/MFA/CAPTCHA/login walls, pauses the bot (`WaitingForHuman`), surfaces a `🛞 Take the Wheel` HUD/alert prompt, and resumes seamlessly once the human hands control back.
   - Supports opaque `secret://keychain/<account>` and `secret://1password/<vault>/<item>/<field>` secret leases injected directly into target fields with registered-secret scrubbing. Credential injection requires explicit approval; leakage resistance still requires adversarial verification.
6. **Loopback Companion Web / AG-UI SSE Server & Chat Bridges**:
   - Built-in `axum` server (`gm serve`, default `127.0.0.1:7433`) serving the live Companion Web UI (`/`), Governance Audit Dashboard (`/admin/audit`), A2A Agent Card (`/.well-known/agent.json`), AG-UI SSE stream (`/ag-ui/events`), and HMAC-SHA256 verified Slack / Telegram / Discord webhook bridges.

---

## Goal State & Metrics (G1–G8)

| Metric | Target | Description & Measurement |
|---|---|---|
| **G1 Native** | Zero Electron/Chromium | Signed App Bundle = daemon + CU helper (`.app`) + shared native Rust UI (`gm-ui`), with legacy AppKit commands retained. |
| **G2 Footprint** | `< 50 MB` idle RSS | Whole process tree idle RSS measured via `footprint`/`ps` (< 120 MB with UI open). |
| **G3 Hot Path** | `< 2 ms` capture, `< 30 ms` AX | ScreenCaptureKit frame fetch < 2ms, AX snapshot < 30ms, JPEG q=70 < 15ms, action dispatch < 5ms. |
| **G4 Agent Step** | `< 1.5 s` median | Fast route median step latency including routed model inference. |
| **G5 Quality** | `≥ 70%` success, **0** unconfirmed | 25-task macOS eval suite (Finder, Safari, Notes, Mail, Settings, Electron, Canvas). |
| **G6 Proactive** | `≥ 80%` useful, `≤ 3`/day | Dogfood precision rating, daily notification cap, daily spend cap ($0.50). |
| **G7 Safety** | Approval tiers, `< 100 ms` kill | Read / Reversible / Irreversible tiers, untrusted screen text, password redaction, append-only audit. |
| **G8 Routing** | Hybrid router & failover | Muse Spark / Wonder + Grok 4.x Vision behind single OpenAI-compatible interface with budget enforcement. |

---

## System Architecture

```text
 ┌─────────────────────────────────────────────────────────────┐
 │           gm-ui: Native Rust UI (WGPU / AccessKit)           │
 │      • Status Item Tray Menu     • Floating Action Overlay  │
 │      • Floating Chat Window      • Global Hotkey Monitors   │
 └──────────────────────────────┬──────────────────────────────┘
                                │ UDS JSON-RPC (/tmp/grok-muse.sock)
 ┌──────────────────────────────▼──────────────────────────────┐
 │                  gm-daemon (Unprivileged)                   │
 │   ┌─────────────────┐ ┌────────────────┐ ┌────────────────┐ │
 │   │ Agent Loop      │ │ Proactive      │ │ LLM Router     │ │
 │   │ • Verify/Retry  │ │ • Quiet Gate   │ │ • Muse Spark   │ │
 │   │ • Trajectories  │ │ • Dogfood Log  │ │ • Grok 4.x     │ │
 │   └────────┬────────┘ └───────┬────────┘ └────────┬───────┘ │
 │            │                  │                   │         │
 │   ┌────────▼────────┐ ┌───────▼────────┐ ┌────────▼───────┐ │
 │   │ Personal Memory │ │ Audit Logger   │ │ Budget Manager │ │
 │   │ (SQLite FTS5)   │ │ (audit.jsonl)  │ │ (Job/Day Caps) │ │
 │   └─────────────────┘ └────────────────┘ └────────────────┘ │
 └──────────────────────────────┬──────────────────────────────┘
                                │ UDS JSON-RPC (/tmp/grok-muse-helper.sock)
 ┌──────────────────────────────▼──────────────────────────────┐
 │       GrokMuseCUHelper.app (Per-user Native Helper)      │
 │      • ScreenCaptureKit: Zero-copy in-memory/file frame     │
 │      • Accessibility (AXUIElement): Window tree & tokens    │
 │      • Input (CGEvent / AXPress): Low-latency action dispatch│
 │      • EventKit: Native Calendar events & Reminders         │
 └─────────────────────────────────────────────────────────────┘
```

---

## Workspace Crates

- **[`crates/gm-core`](crates/gm-core)**: Shared domain types (`AxSnapshot`, `ComputerAction`, `MailItem`, `CalendarItem`), IPC protocol, config schemas.
- **[`crates/gm-platform`](crates/gm-platform)**: `Platform` trait (`capture`, `ax_snapshot`, `act`, `windows`, `user_idle`, `permissions`), native `MacPlatform`, and offline `MockPlatform`.
- **[`crates/gm-cu-helper`](crates/gm-cu-helper)**: Per-user macOS helper binary running as `GrokMuseCUHelper.app` to hold stable TCC permissions across daemon updates.
- **[`crates/gm-llm`](crates/gm-llm)**: OpenAI-compatible wire client, provider router (Meta Muse + xAI Grok), budget governor, Keychain secret integration, and pluggable `LocalVlmBackend` interface with `local-vlm` feature flag (Action A12 prep, Candle/MLX ready).
- **[`crates/gm-safety`](crates/gm-safety)**: Approval tier classifier, emergency kill switch, secure text field redaction, blocklist manager, append-only `AuditLogger`, weekly/monthly report synthesis.
- **[`crates/gm-memory`](crates/gm-memory)**: SQLite FTS memory store, deduplication, knowledge clustering, and stale observation pruning.
- **[`crates/gm-goals`](crates/gm-goals)**: Goal state machine, proactive scheduler, quiet gate, calendar/mail patrol, nightly review, feed stream, and ideas generator.
- **[`crates/gm-agent`](crates/gm-agent)**: Autonomous computer-use loop, Set-of-Marks (SoM) vision compositor, stagnation detector, MCP tool bridge, Seatbelt-confined workspace tools (`ReadFile`, `WriteFile` with 200-line unified diff preview, `ListFiles`, `Shell`), *"Take the Wheel"* human handoff, and trajectory recorder.
- **[`crates/gm-openbot`](crates/gm-openbot)**: OpenBot multi-agent supervisor, persistent `BotRoster`, crash-safe SQLite `DurableTaskQueue`, multi-bot `ChannelStore`, `A2aCoordinator` artifact handoff engine, `ContextCompactor`, `BridgeRouter` (Telegram/Slack/Discord), and loopback `CompanionWebServer` (AG-UI SSE + `/admin/audit`).
- **[`crates/gm-vlm-candle`](crates/gm-vlm-candle)**: Native Rust + Metal local Vision-Language Model inference engine (`Qwen2-VL-2B-Instruct` & `SmolVLM2`) with HuggingFace Hub pull/cache management.
- **[`crates/gm-daemon`](crates/gm-daemon)**: Unprivileged background daemon, dual-socket IPC control server, primary CLI (`gm`), and interactive terminal REPL (`gm chat`).
- **[`crates/gm-ui`](crates/gm-ui)**: AppKit desktop shell (tray, floating windows, action overlay, interactive approval & *"Take the Wheel"* sheets, global hotkeys).
- **[`crates/gm-eval`](crates/gm-eval)**: 25-task macOS evaluation runner (G5), `v0.2.0` capability suite, and `v0.3.0` OpenBot evaluation suite (`ob_queue_01`–`ob_agui_05`).
- **[`xtask`](xtask)**: Release packaging (`.dmg`), automated DMG verification (`verify-dmg`), hardened runtime signing, Apple notarization (`notarize`), and automated 8-gate certification (`test-all`).


---

## Installation & Packaging (DMG)

### Source checkout prerequisites

The workspace currently expects the separate Darius repository beside this checkout
(the same layout used by CI):

```bash
git clone https://github.com/galaxycoils/darius.git Darius
git clone https://github.com/galaxycoils/grok-muse.git grok-muse
cd grok-muse
cargo test -p gm-openbot --locked
```

The dependency is not pinned to a Darius revision yet; this is tracked in the
[reliability review](docs/openmausbot-comparison.md).

### Building the DMG
```bash
# Build binaries, bundle .app packages, codesign, and create target/dist/Grok-Muse.dmg
cargo run --package xtask -- dmg
```

### Installing on macOS
1. Open `target/dist/Grok-Muse.dmg`.
2. Drag `GrokMuse.app` to `/Applications`.
3. Launch `Grok-Muse` or run the CLI onboarding:
   ```bash
   gm onboard
   ```
4. Follow the interactive prompts to grant required permissions in **macOS System Settings**.

---

## LaunchAgent Service Management

Grok-Muse registers separate LaunchAgents for `gm-daemon` and `gm-cu-helper` to maintain independent lifecycles without requiring persistent Terminal windows:

```bash
# Install and bootstrap user LaunchAgents
gm service install

# Check LaunchAgent registration and active PID status
gm service status

# Uninstall and stop LaunchAgents
gm service uninstall
```

Plists are installed in `~/Library/LaunchAgents/`:
- `co.anysphere.grok-muse.daemon.plist`
- `co.anysphere.grok-muse.helper.plist`

---

## macOS Onboarding & Permissions

Grok-Muse provides automated inspection and deep links into macOS System Settings for all required privacy permissions:

```bash
# Check all permissions and print live status table
gm permissions

# Output permission status as JSON
gm permissions --json

# Automatically open System Settings for any missing permissions
gm permissions --open-missing

# Open a specific privacy pane
gm permissions --open screen          # Screen Recording
gm permissions --open accessibility   # Accessibility (AX)
gm permissions --open calendar        # Calendars
gm permissions --open reminders       # Reminders
```

---

## Keychain Secret Management

API keys for Meta Model API (Muse) and xAI API (Grok) are stored securely in the native **macOS Keychain** (`kSecClassGenericPassword`), avoiding plaintext keys in config files:

- **Service Name:** `grok-muse`
- **Account Names:** `meta_api_key`, `xai_api_key`

### Adding Secrets via macOS CLI
```bash
# Store Meta Muse API key
security add-generic-password -s "grok-muse" -a "meta_api_key" -w "your-meta-key" -U

# Store xAI Grok API key
security add-generic-password -s "grok-muse" -a "xai_api_key" -w "your-xai-key" -U
```

### Environment Fallbacks (CI / Headless)
If Keychain entries are absent (e.g., in automated CI environments), Grok-Muse automatically falls back to environment variables:
- `GROK_MUSE_META_KEY` or `META_API_KEY`
- `GROK_MUSE_XAI_KEY` or `XAI_API_KEY`

---

## CLI Command Reference

| Command | Arguments | Description |
|---|---|---|
| `gm do` | `"<instruction>"` | Execute an autonomous computer-use task via AX and visual fallback. |
| `gm daemon` | `[--config <path>]` | Start the background orchestrator and proactive scheduler. |
| `gm status` | — | Display system health, IPC connectivity, and TCC permissions. |
| `gm kill` | — | Trigger the emergency kill switch (< 100 ms halt). |
| `gm permissions` | `[--open-missing] [--open <pane>] [--json]` | Inspect and request macOS TCC privacy permissions. |
| `gm onboard` | `[--check-only]` | First-run onboarding verification and setup. |
| `gm service` | `<install\|status\|uninstall>` | Manage LaunchAgent background services. |
| `gm dogfood` | `summary \| rate \| feedback` | Proactive notice rating and G6 dogfood gate tracking. |
| `gm stats` | `[--json]` | Output unified health, permissions, and metrics. |
| `gm review` | `[--date YYYY-MM-DD]` | Synthesize structured nightly review markdown. |
| `gm audit` | `[--period <weekly\|monthly>] [--json]` | Synthesize safety compliance, spend by provider, CU actions, and task success. |
| `gm upkeep` | `[--stale-days <N>] [--json]` | Run memory upkeep (deduplicate facts, cluster topics, expire stale observations). |
| `gm feed` | `[--limit <N>] [--json]` | Terminal timeline stream of recent observations, patrol insights, and tasks. |
| `gm ideas` | `[--json]` | Proactive recommendations from calendar gaps, unread threads, and memory. |
| `gm chat` | `[--system "<prompt>"]` | Interactive terminal REPL for conversation, memory retrieval, status, and inline tasks. |
| `gm models` | `<list\|pull\|rm> [--json]` | Manage offline local Vision-Language Models (`qwen2-vl-2b`, `smolvlm2-2.2b`). |
| `gm bots` | `<list\|show\|create\|rm> [--json]` | Manage OpenBot persistent agent roster and isolated per-bot workspaces. |
| `gm queue` | `<list\|add\|pause\|resume\|cancel\|reassign\|backup\|restore>` | Manage crash-safe SQLite FIFO task queues across bots. |
| `gm channels` | `<list\|post\|history\|assign\|archive\|restore>` | Post to multi-bot channels (`#general`, `#research`, `#ops`) and trigger `@bot` mentions. |
| `gm serve` | `[--bind 127.0.0.1:7433]` | Launch the loopback OpenBot Companion Web UI, AG-UI SSE stream, and `/admin/audit` dashboard. |

---

## OpenBot Multi-Agent Coworkers, Queue, Channels & Companion Web UI

The companion now supports channel conversations and creating channels with a topic,
members, and default responder. It refreshes automatically, preserves failed sends,
and supports the server's optional bearer token. **Automatic queue execution is not
connected yet:** messages and tasks are persisted, but `gm serve` does not run agents.
See the [OpenMausBot comparison and reliability review](docs/openmausbot-comparison.md)
for the feature roadmap and runtime blockers.

For token-protected local use, set `GROK_MUSE_COMPANION_TOKEN` in the server process's
environment before starting `gm serve` or `gm daemon`, then enter that token in the
companion's Connect form. The page keeps it only until reload. Keep the default
loopback bind; remote access/pairing is not established by this setting. Slack's
optional signing secret uses `GROK_MUSE_SLACK_SIGNING_SECRET`; replay protection is
still outstanding (see the review).

```bash
# List the persistent bot roster (muse, researcher, coder, guardian)
gm bots list

# Enqueue a prioritized task for @coder
gm queue add --bot coder --priority 10 "Write unit tests and save summary in outbox/"

# Post a message in #general mentioning @coder and @researcher
gm channels post general "Hey @coder benchmark the hotpath and hand off report to @researcher"

# Export or restore durable SQLite queue state
gm queue backup /tmp/openbot_queue_backup.jsonl
gm queue restore /tmp/openbot_queue_backup.jsonl

# Launch the loopback Companion Web / AG-UI / Governance Audit server
gm serve --bind 127.0.0.1:7433
```


---

## Proactive Engine & Quiet Gate

The proactive engine continuously monitors local state while enforcing strict quiet gate rules to protect user attention:

1. **Quiet Hours**: Suppresses all non-urgent notices between **22:00 and 08:00** local time.
2. **Daily Notice Cap**: Enforces a strict maximum of **3 interruptions per day** (dogfood G6 gate).
3. **Daily Spend Cap**: Hard cap of **$0.50 USD per day** across proactive patrol jobs.
4. **Distinct User Feedback Semantics**:
   - `done`: Item resolved; permanently suppressed from dedup.
   - `later`: Snoozed for 4 hours.
   - `stop`: Topic opt-out; permanently mutes this topic.
5. **Urgency Bypass**: Flagged or critical items (e.g., server outages, imminent meetings) bypass quiet hours.

---

## Memory Upkeep Engine

Personal memory is stored in SQLite with full-text search (FTS5). The upkeep engine runs periodically to keep knowledge clean and relevant:

```bash
# Run on-demand memory upkeep
gm upkeep

# Custom retention period for transient observations
gm upkeep --stale-days 7 --json
```

- **Periodic Background Execution**: In addition to manual runs via `gm upkeep`, `gm-daemon` registers an hourly background maintenance job (`schedule_memory_upkeep`) on the proactive scheduler.
- **Fact Deduplication**: Calculates token Jaccard similarity (≥ 0.85) and normalized string matching. Automatically merges tags from duplicate records and retains the highest-importance entry without duplicates.
- **Knowledge Clustering**: Discovers shared themes across records and applies cluster tags (`cluster:<topic>`), linking related notes together.
- **Stale Observation Pruning**: Automatically purges transient episodes and observations older than the retention threshold with low importance (≤ 0.4).

---

## Desktop Shell & Keyboard Shortcuts

Launch the native AppKit desktop shell:
```bash
gm-ui tray
```

### Global Keyboard Shortcuts
- **`Cmd + Shift + Space`**: Instant Summon Assistant (floating chat window).
- **`Cmd + Alt + Escape`**: Emergency Kill Switch (immediately stops all agent actions).

### Visual Action Overlay
A high-contrast, floating HUD overlay positioned on the frontmost screen indicating active agent state:
- 🔵 **Active**: Currently executing AX or pixel actions.
- 🟡 **Paused**: User coexistence pause active (user input detected within last 1.5s).
- 🛑 **Stopped**: Emergency kill switch activated.

---

## Benchmarking & Evaluation (`xtask`)

Grok-Muse includes an `xtask` binary implementing rigorous quality and performance gates:

```bash
# 1. Benchmark G3 local hot-path budgets (< 2ms capture, < 30ms AX, < 15ms JPEG, < 5ms dispatch)
cargo run --package xtask -- bench-hotpath

# 2. Benchmark G2 idle RSS footprint (< 50MB idle tree)
cargo run --package xtask -- bench-rss

# 3. Benchmark G4 LLM latency gate (< 1.5s median, < 4.0s p95) measuring TTFT & tokens/sec
cargo run --package xtask -- bench-llm

# 4. Verify release DMG bundle integrity, codesign, and /Applications symlink
cargo run --package xtask -- verify-dmg

# 5. Run G5 25-task evaluation suite (Finder, Safari, Notes, Mail, Settings, Electron, Canvas)
cargo run --package xtask -- eval
```

---

## Interactive Terminal REPL (`gm chat`)

Direct conversational interaction with Grok-Muse supporting SQLite personal memory retrieval, live status inspection, and inline computer-use task dispatch:

```bash
# Start interactive REPL
gm chat

# Override default assistant system prompt
gm chat --system "You are an automated coding reviewer."
```

### REPL Commands
- **`/memory <query>`**: Search personal memory records using SQLite FTS5.
- **`/status`**: Inspect live TCC privacy permissions, CU helper socket status, and spend budget.
- **`/do <instruction>`**: Dispatch an autonomous computer-use task directly from within the conversation.
- **`/clear`**: Clear current chat history while preserving the system prompt.
- **`/history`**: Display total message count and conversation turn statistics.
- **`/help`**: Show REPL command reference.
- **`/exit` or `/quit`**: Cleanly exit the REPL session.

---

## Local Vision-Language Model Interface (Action A12 Prep)

Grok-Muse provides a pluggable `LocalVlmBackend` trait in `crates/gm-llm` for offline local model grounding and visual triage (Candle/MLX ready):

```bash
# Compile and test with local-vlm feature flag enabled
cargo test --package gm-llm --features local-vlm
```
- **`LocalVlmBackend`**: Pluggable interface for `ground()`, `triage()`, and `generate()`.
- **`CandleMlxBridgeBackend`**: Pre-scaffolded bridge supporting Metal / CPU device execution and quantized weights (`Fp16`, `Q4_0`, `Q4_K`).
- **`LocalVlmLlmAdapter`**: Adapter exposing local VLMs through the standard `LlmBackend` interface.

---

## Continuous Integration (CI)

Grok-Muse is continuously tested on both **macOS (`macos-14`)** and **Linux (`ubuntu-latest`)** via GitHub Actions:
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace` (or `cargo test --workspace`)
- `cargo run -p xtask -- eval` (25-task evaluation gate)
- `cargo run -p xtask -- bench-hotpath` (native hot-path budgets on macOS)
- `cargo run -p xtask -- bench-llm` (G4 latency gate calibration)
- `cargo run -p xtask -- verify-dmg` (macOS release bundle verification)


---

## License

MIT License. See [LICENSE](LICENSE) for details.
