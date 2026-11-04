# Public-release implementation status

This branch implements the task/runtime and native-client foundation of the public-release plan. It is a draft development build. The complete public-release roadmap is not finished.

## Implemented

- One OS-locked runtime owner per storage directory. Opening a client does not run recovery. Both daemon and standalone companion modes dispatch bounded workers.
- Transactional task claims across SQLite connections, guarded transitions, worker-acknowledged pause/cancellation, immutable terminal completion, separate attempt identifiers, outcome checkpoints and persisted event replay.
- Write-ahead markers for external effects. Ambiguous effects require human review and a resume note; automatic retries cannot replay them.
- Task-scoped helper sessions reject stale actions across retries. Cancelled RPCs discard partial connections; task cancellation uses a separate connection and emergency stops remain latched until helper restart.
- Bot personas, provider/model preferences, per-bot memory retrieval and independent task budgets. Shared daily admission reserves estimated request exposure before dispatch; failed/cancelled requests conservatively retain exposure.
- Automatic mock helper/model fallbacks are removed. Missing providers fail explicitly; demo inference requires `GM_FORCE_MOCK_LLM=1`.
- GUI/shell/MCP permission enforcement and restricted delegation. Host desktop control and screenshot egress default to disabled. Scoped native file tools use directory capabilities; brokered artifact transfer cannot overwrite inbox files or follow paths outside its grants.
- A shared Rust/egui native UI with WGPU and AccessKit, task controls, approvals, handoff notes, conversation search, streamed responses and acknowledged draft clearing. `gm-ui` defaults to this interface; legacy AppKit commands remain available.
- macOS Keychain plus Windows Credential Manager/Linux Secret Service integration. No plaintext credential-file fallback is added.
- Required companion bearer authentication, loopback binding, bounded HTTP bodies and browser security headers. Slack requires a fresh signed request, a configured channel allowlist and a unique persisted delivery identifier.
- Offline QEMU workspace transport with a trusted Ed25519 manifest, image checksum/architecture validation, ephemeral snapshot writes, no host mounts, no network adapter, bounded file exchange and bounded guest output. No host-shell fallback exists.
- Production local VLM startup requires checkpoint weights and a tokenizer. Synthetic offline fixtures explicitly select `SyntheticTest`.
- Exact updater checksum asset matching, atomic private roster writes, cross-process roster locks, strict configuration loading, pinned Rust/Darius inputs, Windows CI coverage and a project license.

## Running locally

Run `tools/bootstrap.sh` to verify the pinned sibling Darius checkout and locked workspace dependencies. It preserves an existing checkout at a different revision and explains the mismatch.

Build the binaries with `cargo build --workspace --bins`. Launch `gm-ui` for the shared native interface; `gm-ui --config /absolute/path/config.json native` selects a configuration explicitly.

`gm --config /absolute/path/config.json daemon` owns the runtime and can host its companion. Alternatively, `gm --config /absolute/path/config.json serve` runs the companion and its workers. These cannot own the same storage directory simultaneously. `gm do` submits to an existing daemon, or takes an exclusive lease for standalone execution. A submission acknowledgment is not task completion.

The companion token comes from `GROK_MUSE_COMPANION_TOKEN`, or a private `<storage_dir>/companion-token` file generated on first startup. Paste the token into the companion connection form. Tokens are not placed in browser storage. Bind to an explicit loopback IP; remote access requires a future TLS pairing gateway.

Credential injection defaults to disabled. The experimental `safety.allow_secret_injection` setting is separate from desktop control and still requires per-action approval; use human handoff for production credential entry.

Desktop observation/control additionally requires `safety.allow_host_desktop = true` and OS permissions. Screenshot egress has separate `providers.allow_screenshot_egress_meta` / `allow_screenshot_egress_xai` settings; both default to false.

## Workspace VM status

The transport is an offline implementation requiring a separately provisioned and tested image. It is **not** a bundled, certified VM product. `workspace_vm.enabled` defaults to false. Configuration requires `qemu_path`, `image_path`, `manifest_path`, `signature_path`, and a trusted `trusted_public_key_hex`. The exact manifest bytes must be signed with Ed25519; the signature file contains its hexadecimal signature. A v1 manifest is `{ "version": 1, "architecture": "x86_64", "sha256": "<image SHA-256>" }` (use `aarch64` on that architecture).

The image must implement the `org.grokmuse.agent` virtio-serial protocol. The guest implementation is in `tools/workspace-guest/agent.py`; it must run from a protected, provisioned guest service with a writable `/workspace`. Host inputs are capped at 1,000 files/8 MiB and symlinks are rejected. Guest command output is bounded. Commands run against an ephemeral snapshot and never automatically write changes back to host files.

No signed guest image or firmware distribution is included, hardware boot has not been certified, and ARM firmware provisioning remains unfinished. Guest networking, persistent sessions, artifact export/review, CLI installations/login flows, resource controls and the signed image publishing/update pipeline remain release blockers. The transport does not currently enable Claude/Codex/Grok CLI accounts.

## Remaining implementation and verification

| Area | Remaining work |
| --- | --- |
| Desktop parity | Implement Windows capture/UI Automation/input and Linux portal/PipeWire/AT-SPI/input. Current backends explicitly report unavailable, without demo screens, mail or successful no-op actions. Test real desktop sessions, permission revocation and multi-monitor scaling. |
| Native product experience | Finish onboarding/settings, persistent conversation history, channel/bot editing, attachments/artifacts, accessibility certification, IME and native services integration on all three OSes. |
| Agent workflows | Complete research/citation verification, Git worktree/patch/test flows, richer tool schemas, real checkpoint/model compatibility and representative end-to-end evaluations. |
| Ecosystem | Add API/CLI provider drivers beyond the existing routes, templates/import/export, durable timezone-aware routines, OAuth/MCP connector setup, remote pairing and optional voice. |
| Data/operations | Finish versioned migration/retention policy, diagnostics export, recovery UX, exact per-provider configurable pricing and persistent usage accounting across restarts. |
| Releases | Implement authenticated update manifests/rollback and signed installers for each OS/architecture; validate install/update/uninstall on clean machines. Checksums alone do not authenticate a publisher. |
| Quality targets | Run the real 30-task-per-OS/three-workflow release suite repeatedly, measure performance/resource budgets and publish results. Offline fixtures do not certify these targets. |

Do not advertise public desktop parity, VM isolation certification, model quality, signing/notarization or the README performance targets as completed based on the current unit tests.

## Verification in this workspace

Linux Clippy passes for the full workspace and all targets with warnings treated as errors. The final `cargo test --workspace -j 2 --no-fail-fast` run passes all 188 tests, including installer, helper session/cancelled-RPC, native chat-correlation, and offline model fixtures. Formatting, JavaScript/Python syntax, shell syntax, and whitespace checks also pass.

A native application was launched under a private Xvfb display using a software Vulkan driver. Native UI → daemon IPC → a local HTTP fixture provider → scoped file write → completed queue state passed. The fixture also emitted split UTF-8 SSE data successfully. The UI test exposed duplicate stream/reply delivery; correlated message IDs and regression tests now cover both delivery orders and unrelated clients. No paid model was used.

This virtual-display test does not certify real desktop automation or hardware rendering. The software-rendered UI used about 120 MiB RSS before adding daemon/helper memory, exceeding the existing whole-process-tree UI goal; native GPU measurements and optimization remain necessary. macOS/Windows native flows, signed VM boot, real provider quality, platform signing and public-release acceptance gates remain unverified.
