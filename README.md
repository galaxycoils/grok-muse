# Grok-Muse Hybrid Agent

Fast computer-use + proactive personal agent for macOS.

## Architecture

- **`gm-core`**: Core domain types, IPC protocol definitions, configuration.
- **`gm-platform`**: `Platform` trait (`capture`, `ax_snapshot`, `act`, `windows`, `user_idle`, `permissions`), macOS backend, and mock implementation.
- **`gm-cu-helper`**: Native macOS helper daemon holding TCC privileges (Screen Recording, Accessibility) implementing `Platform` over Unix domain socket IPC.
- **`gm-llm`**: OpenAI-compatible client, hybrid routing (Meta Muse + xAI Grok), spend budgets, failover.
- **`gm-safety`**: Approval tiers, emergency kill-switch, untrusted data separation, redaction, audit logging.
- **`gm-memory`**: Proactive agent memory, SQLite FTS integration.
- **`gm-goals`**: Goal engine, follow-up tracker, quiet gate, cron scheduler.
- **`gm-agent`**: Multi-step computer-use loop, verify-after-act, trajectory recorder.
- **`gm-daemon`**: Unprivileged central background daemon.
- **`gm-ui`**: Desktop shell (tray, global hotkeys, action overlay, interactive chat).
- **`gm-eval`**: 25-task macOS evaluation harness and offline record/replay fixtures.
- **`xtask`**: Workspace build tasks, codesigning, notarization, benchmark gates.

## Development

```bash
# Check all crates
cargo check --workspace

# Run tests
cargo test --workspace
# or
cargo nextest run
```
