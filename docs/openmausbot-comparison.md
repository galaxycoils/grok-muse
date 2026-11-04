# OpenMausBot comparison and reliability review

Reviewed 2026-11-04. Grok Muse baseline: `6f60b76` (v0.3.0, including the upstream Linux CI, bundle-binary and Metal-device fixes).
OpenMausBot reference: [`db8ae8d`](https://github.com/milind-soni/OpenMausBot/tree/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d).
This is a source review plus the checks recorded below, not a certification of native macOS behavior, model quality, or the README performance targets.

## Recommendation

Keep the native Rust/AppKit architecture. Borrow OpenMausBot's product behavior and lifecycle design; replacing it with Electron or copying its TypeScript server is unnecessary. Make the existing runtime dependable before adding more autonomous entry points. No model weight fine-tuning is needed for the issues found here: they are integration, lifecycle, and verification problems.

## Features compared

| Capability | Grok Muse at baseline | OpenMausBot reference | Recommended work |
| --- | --- | --- | --- |
| Bot roster and personas | Persistent profiles, four default bots, CLI creation | Chat-like roster with editing, duplicate, pin, hide, unread | Add profile editing and duplicate after profile settings affect execution |
| Per-bot model choice | Fields exist; runner uses shared routing | Provider availability, model picker, switching | Wire preferences to model routing, validate availability, explain fallback |
| CLI-backed agents | HTTP Meta/xAI routing and local VLM interface | Claude, Codex, Grok CLI drivers; custom engines | Define a Rust driver trait and normalize events; add one CLI driver at a time |
| Channels | Durable messages, owners, mentions; dashboard only listed channels | Separate transcripts, instructions, folders, rosters and responder rules | This patch adds conversation viewing/posting and custom channel creation; editable instructions and folders remain |
| Durable execution | SQLite queue and callable runner | Central runtime admits and executes assignments | First connect exactly one queue owner to execution, with capability checks and cancellation |
| Task controls | Database status changes | Turn interruption and lifecycle handling | Connect controls to the running task; preserve terminal states |
| Context management | Compactor and per-bot memory APIs exist | Runtime integrates thread state and memory | Wire history/compaction/memory into turns with explicit boundaries |
| Permissions and human handoff | Approval tiers, interaction handlers, wheel state | Inline action/question cards and voice approvals | Expose real pending approvals in the companion, correlated to task and request IDs |
| Computers | Native macOS helper and Seatbelt workspaces | Cloud computer, local VM, opted-in host use | Preserve native fast path; add an isolated VM provider before remote computer management |
| Connected apps | MCP bridge abstraction, chat bridge routes | OAuth app marketplace, custom MCP servers | Finish MCP discovery/configuration and tool execution before marketplace UX |
| Schedules/webhooks | In-memory proactive scheduler; chat webhooks | User routines, history, overlap suppression, app-event triggers | Add persisted routine definitions and run receipts through the same task runtime |
| Teams/templates | Default bot profiles and queue backup | Reviewed team import/export, presets, paused routines | Add versioned JSON/Markdown import with preview; omit secrets, history and access grants |
| Voice | No equivalent voice flow found in reviewed crates | TTS providers, automatic reading, calls | Start with native macOS TTS and cancellable playback; add dictation later |
| External control | A2A/AG-UI endpoints and CLI commands | Bounded MCP server and headless team/run/export CLI | Add a narrow MCP facade over the same validated commands |
| Companion access | Loopback dashboard and optional bearer checks | Pairing, mobile/desktop companions, remote access flows | Finish local reliability first; remote pairing/revocation needs a separate design |

Reference material: [README](https://github.com/milind-soni/OpenMausBot/blob/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d/README.md), [runtime contract](https://github.com/milind-soni/OpenMausBot/blob/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d/docs/runtime-contract.md), [custom engines](https://github.com/milind-soni/OpenMausBot/blob/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d/docs/custom-engines.md), [team sharing](https://github.com/milind-soni/OpenMausBot/blob/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d/docs/team-sharing.md), [voice](https://github.com/milind-soni/OpenMausBot/blob/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d/docs/voice-mode.md), [routines](https://github.com/milind-soni/OpenMausBot/blob/db8ae8d69eab2d7f4cb1bf4430099d91bf65cf9d/docs/routine-schedules.md).

These references describe upstream behavior; this review does not claim to have run OpenMausBot. No upstream implementation was copied. Upstream is Apache-2.0 outside its separately licensed enterprise directory; any future code reuse needs appropriate attribution and license review.

## Release blockers found in existing code

1. **Queued work has no production dispatcher.** `OpenBotSupervisor::run_next_for_bot` has no call sites elsewhere in the repository. Neither `run_daemon` nor `run_serve` starts workers. A successful enqueue is not evidence that an agent ran. Add bounded workers owned by the daemon, and verify `message → queue → runner → result → transcript` using a fake backend first.
2. **Capability flags do not constrain the agent loop.** `run_next_for_bot` uses `allow_gui` only to decide whether to acquire the GUI mutex. The loop still snapshots the desktop and can execute GUI actions for non-GUI bots; `allow_shell` is not passed into the loop. Gate tool discovery AND execution before starting workers. On platforms without Seatbelt, shell execution currently falls back to plain `sh` in `workspace_tools.rs`; do not describe that as OS confinement.
3. **Cancellation/pause/reassignment do not stop running work.** Queue status updates have no per-task cancellation handle, and completion can overwrite those statuses. Guard state transitions and retain a running worker's ownership until it actually exits. Do not requeue a still-running task. The new companion avoids offering those controls on running tasks, but the backend still needs correction.
4. **Recovery is invoked by every supervisor open.** CLI roster/queue/channel commands and standalone `gm serve` also open supervisors. They can mistake another live process's work for a crash. Add an exclusive runtime lease; recover only after establishing that the previous owner is gone. Atomic queue admission must also cover multiple SQLite connections, not just clones of one mutex.
5. **Persona/model/context settings are incomplete.** The runner includes display name and role, but not `system_prompt`, and ignores `preferred_provider`/`preferred_model`. The compactor and bot memory store are exposed but not used there. Test these settings against the actual model requests before adding a picker that promises they work.
6. **Crash-safety mutation tracking is not integrated.** `set_mutating_in_flight` exists in the queue but has no production loop calls. The recovery tests set it manually. Persist the marker before a mutating step and clear it after verification; inject a crash in between to prove the task pauses rather than repeats an external action.
7. **Slack verification needs hardening.** The signature helper verifies the HMAC but does not check timestamp freshness or deduplicate deliveries. The route allows unsigned requests when no signing secret is configured. Before enabling Slack outside a trusted local setup, require the secret, enforce a freshness window, ignore bot-generated/replayed events, and persist event IDs. This patch wires the environment secret into server construction; it does not resolve those remaining issues.
8. **Configuration fallback and persistence need hardening.** This patch fixes `BotRoster::open` so invalid JSON returns an error and preserves the original roster instead of overwriting it with defaults. `GmConfig::load_or_default` still silently resets malformed configuration; make that failure explicit too. Roster writes still need atomic replacement and cross-process ownership.

## Improvements implemented in this patch

- Channel conversation view, sending messages, and custom channels with topic, member metadata and default responder. Membership is not an execution permission boundary: mention routing still recognizes any known bot, as before.
- Bearer-token entry for all dashboard requests. Token stays in memory, is cleared from the input after connect, and is never put in URLs or browser storage. Daemon and `gm serve` read `GROK_MUSE_COMPANION_TOKEN` and `GROK_MUSE_SLACK_SIGNING_SECRET`.
- Periodic refresh with no overlapping refresh requests, timeout/error handling, stale response protection, preserved responder selection, retained drafts on failure, and submit buttons disabled during writes.
- Safe DOM text rendering for names, messages, tasks and audit records, replacing unescaped HTML templates.
- Channel IDs normalized consistently. Duplicate creation cannot replace a channel and reset archive/pause state. Missing/archived channels and blank messages are rejected. Paused channels accept discussion without spawning work; direct task submissions to paused/archived channels are rejected.
- Corrupt roster JSON returns an error without replacing the original file.
- Punctuation-separated bot mentions (`@coder,@researcher`) work and repeated mentions are deduplicated.
- Missing history endpoints return 404. SSE clients receive `RESYNC_REQUIRED` when the broadcast receiver lags. The dashboard currently uses polling, not this SSE stream.
- The companion states that automatic queue execution is unavailable, and does not offer pause/cancel on running tasks where the API cannot enforce interruption.

## Suggested implementation sequence

1. **Runtime correctness:** fix blockers 1–6 together. Exit criterion: deterministic tests prove one active task per bot, one GUI owner, enforced capabilities, cancellation, crash recovery, and results saved to the right channel.
2. **Daily use:** bot editing/duplication, working per-bot model choice, conversation search, approval cards, safe attachment upload and artifact download. Exit criterion: a real macOS session completes a task and an approval/handoff cycle without losing work after reconnect.
3. **Automation and integrations:** persistent routines with overlap suppression, reviewed team imports, a bounded MCP facade, then OAuth integrations. Every ingress should use the same task lifecycle.
4. **Voice and computers:** native speech first; isolated VM support next. Track voice latency, interruption and cleanup, rather than adding several providers at once.
5. **Build and release evidence:** pin the sibling Darius dependency (CI currently checks out its moving default branch). Verify real macOS TCC/onboarding, clean-machine install, signing/notarization/update, idle RSS, capture/AX timing and measured end-to-end task latency. Report p50/p95 and success rates with sample sizes. Existing README targets are not measurements from this review. VLM tests include synthetic weights and dummy image buffers; `Auto` mode allows a calibrated fallback without model weights. Validate actual checkpoints in `RealWeights` mode against real screenshot tasks before claiming local-model accuracy.

## Verification

Checked on Linux with Darius `f47a4ce980abecb751c9f5c79e1c424a20100433`:

- `cargo fmt --all -- --check` and JavaScript syntax checking pass.
- `cargo test -p gm-openbot --locked`: 9 tests pass, including live HTTP authentication, channel creation/message persistence, duplicate preservation, mention routing, paused/archived validation, and corrupt roster preservation.
- Focused `gm-openbot` Clippy check passes with warnings denied for that package.
- Chromium against a real token-protected `gm serve` passes channel creation, message persistence, routing, HTML-injection rendering, failed-send draft retention, polling, selected-bot retention, token storage and mobile-overflow checks; no page errors.
- The initial workspace test run completed other crate tests but failed the installer test because the baseline bundle command built only `gm`. The corrected bundle command arrived in upstream `6f60b76` and is preserved here. Subsequent installer attempts hit this environment's 8 GB disk limit during debug-bundle copying/symlink creation. Installer verification remains incomplete here; this PR does not certify it.

See the PR for the final workspace lint result. Native macOS permissions, GUI execution, model-provider calls, signing/notarization, and performance targets require a macOS machine and configured providers; they cannot be certified by the Linux companion checks.
