# Public-release implementation status

This branch implements the task/runtime foundation, native daily-use features, scheduled routines and desktop backends from the public-release plan. It is a draft development build. The complete public-release roadmap is not finished.

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

## Added in the remaining-plan implementation

- Windows UI Automation traversal and validated Invoke targets, GDI virtual-desktop capture with per-monitor DPI awareness, Unicode SendInput, idle detection and native notifications. GDI is the compatibility capture path; protected desktops, elevated targets and native hardware behavior still need Windows certification.
- Linux X11 capture, window metadata, XTest input and idle detection; AT-SPI traversal, action tokens and editable text. Wayland uses an explicitly consented RemoteDesktop + ScreenCast portal session and PipeWire capture of that same selected monitor. No XWayland input fallback is used. Portal revocation invalidates the session. A working AT-SPI foreground window is required for the agent's desktop safety checks.
- Explicit capture-to-desktop geometry, including negative monitor origins and resized images. Set-of-Marks draws in image space and resolves to desktop space. Models can mark raw coordinates with `coordinate_space: "image"`; invalid coordinates and malformed screenshot data fail explicitly.
- Attempt-owned cancellation tokens and task-scoped screenshot/accessibility RPCs prevent stale attempts from observing or replacing a new attempt's targets. Unix helper ownership stays locked across active connections, so another startup cannot unlink a live helper socket; spawned helper processes are reaped.
- Native first-run/settings screens, masked API-key entry backed by OS credential stores, atomic configuration saves, bot profile/model/capability editing, channels, task results, drag-and-drop attachments and inert text artifact previews. Attachments are bounded, uniquely named inbox copies; binary previews are not implemented.
- Private native conversation/draft persistence with a single writer lease, bounded retention and clear-history controls. Requests carry up to 24 prior user/assistant turns within 64 KiB for conversation continuity; each client supplies its own context. No other client's streamed tokens are incorporated.
- Daily routines with IANA timezones and weekday selection, persisted in SQLite. Queue insertion and schedule advancement share one transaction. Missed runs coalesce into one task; queued/running/paused/handoff tasks suppress overlap. Repeated local times run on the earlier offset once; skipped times advance to the next valid local minute. The daemon must remain running; this does not wake a powered-off machine.
- Persistent SQLite usage admission across independent connections and process restarts. Unresolved reservations retain their estimated exposure. This is conservative estimated accounting, not provider invoice reconciliation; pricing remains approximate.
- Signed updater manifests bind version, OS/architecture, asset name, size and SHA-256 to a trusted Ed25519 key and expiry. Unsigned downloads are refused. Downloads are bounded, use unique temporary files and clean up on failure/cancellation. `tools/make-release-manifest.py` produces the manifest/signature from an offline publisher key. This stages an authenticated asset; installer signing and automatic rollback remain separate release work.
- Workspace VM exports from `/workspace/exports` are validated, bounded and saved to a new outbox directory for review, without automatically changing host project files. ARM UEFI firmware can be supplied with a signed manifest digest. Workspace imports use directory handles and bounded reads.
- Persistent channel instructions (`ChannelInfo.instructions`, `create_channel_with_instructions`, `update_channel_instructions`, `gm channels set-instructions`) with supervisor prompt injection for channel bot tasks.
- Team templates and safe import/export (`gm team export`, `gm team preview`, `gm team import`) with strict credential exclusion and trigger safety (all imported routines forced to `enabled = false`).
- Durable routines quiet hours (`QuietHours`, `is_quiet`), overlap suppression recording, and persistent SQLite run history (`routine_runs_v1`, `gm routines runs <id>`).
- CLI discovery metadata exists, but host execution and version probes are disabled pending a signed, isolated CLI-agent VM profile. Official distribution inputs for Claude, Codex and Grok Build are pinned for provisioning; live CLI execution and unknown-cost accounting remain release blockers.
- SQLite ordinary opens now fail closed. Explicit `gm diagnostics recover-database` requires exclusive runtime/database leases and verified preservation of database/WAL/SHM files. Queue/channel/routine, usage and pinned memory migrations are versioned, transactional and backed up. Configuration and roster formats are versioned with legacy backup preservation. Audit/replay lifecycle policy remains outstanding.
- Local health diagnostics (`gm diagnostics check`) and sanitized support bundle export (`gm diagnostics export`) with `SecretScrubber` credential redaction and SHA-256 fingerprint verification.
- Release engineering documentation: `THIRD_PARTY_NOTICES.md`, `CONTRIBUTING.md`, `docs/SUPPORT_MATRIX.md`, `docs/TROUBLESHOOTING.md`, and `docs/PRIVACY.md`.

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

No signed guest image or firmware distribution is included and hardware boot has not been certified. ARM transport accepts `workspace_vm.firmware_path` only when the signed image manifest includes its `firmware_sha256`. Guest networking, persistent VM sessions, CLI installations/login flows, configurable resource controls and the signed image publishing/update pipeline remain release blockers. The transport does not currently enable Claude/Codex/Grok CLI accounts.

## Remaining implementation and verification

| Area | Remaining work |
| --- | --- |
| Desktop parity | Certify the implemented Windows and Linux backends in real desktop sessions, permission revocation, multi-monitor scaling and protected/elevated target behavior. Finish application-launch/menu parity and evaluate WGC/DXGI capture. |
| Native product experience | Certify onboarding, accessibility and IME on all three OSes; add native file picking/export, rich binary previews, bot duplication/templates and full native services integration. |
| Agent workflows | Complete research/citation verification, Git worktree/patch/test flows, richer tool schemas, real checkpoint/model compatibility and representative end-to-end evaluations. |
| Ecosystem | Add API/CLI provider drivers beyond the existing routes, reviewed templates/team imports, OAuth/MCP connector setup, remote TLS pairing and optional voice. |
| Data/operations | Finish versioned migration/retention policy for all stores, diagnostics export, richer recovery UX, configurable provider pricing and invoice reconciliation. |
| Releases | Provision the release trust key, implement automatic rollback and signed installers for each OS/architecture; validate install/update/uninstall on clean machines. Authenticated staging is implemented. |
| Quality targets | Run the real 30-task-per-OS/three-workflow release suite repeatedly, measure performance/resource budgets and publish results. Offline fixtures do not certify these targets. |

Do not advertise public desktop parity, VM isolation certification, model quality, signing/notarization or the README performance targets as completed based on the current unit tests.

## Verification in this workspace

The prior runtime foundation passed 188 Linux tests and workspace Clippy. Verification of this expanded implementation is being recorded below; those earlier results do not certify the new desktop or native-product features.

A native application was launched under a private Xvfb display using a software Vulkan driver. Native UI → daemon IPC → a local HTTP fixture provider → scoped file write → completed queue state passed. The fixture also emitted split UTF-8 SSE data successfully. The UI test exposed duplicate stream/reply delivery; correlated message IDs and regression tests now cover both delivery orders and unrelated clients. No paid model was used.

This virtual-display test does not certify real desktop automation or hardware rendering. The software-rendered UI used about 120 MiB RSS before adding daemon/helper memory, exceeding the existing whole-process-tree UI goal; native GPU measurements and optimization remain necessary. macOS/Windows native flows, signed VM boot, real provider quality, platform signing and public-release acceptance gates remain unverified.

## Signed update setup

Production builds pin the independently verified publisher key with build-time `GM_RELEASE_PUBLIC_KEY_HEX`. A development build can explicitly configure `GM_UPDATE_TRUSTED_PUBLIC_KEY_HEX` at runtime. A release must contain `grok-muse-release.json` and its hex Ed25519 signature `grok-muse-release.sig`, alongside the exact signed assets. No key is downloaded or trusted automatically from the release being verified.

Generate release metadata with `python3 tools/make-release-manifest.py --version VERSION --expires RFC3339_TIME --asset OS:ARCH:/path/to/asset --private-key /offline/ed25519.pem --output-dir /staging`. Repeat `--asset` for each target. Keep private signing keys outside this repository. The script prints only the public verification key.

Linux builds require PipeWire/SPA development headers and Clang in addition to the existing native dependencies (`libpipewire-0.3-dev libspa-0.2-dev clang` on Ubuntu). `tools/test-native-x11.sh` runs capture/focus/keyboard/pointer tests on a private Xvfb display. `python3 tools/workspace-guest/test_agent.py` checks guest artifact export boundaries without booting a VM.

## Expanded verification

The expanded Linux workspace passed 202 tests, including the native composer shortcut, conversation-context ordering, persistent usage admission, DST routines, attachment boundaries, signed manifests, VM artifact validation and installer pipeline. Full workspace Clippy passes with warnings denied. The final run includes helper ownership checks in both helper targets and the installer pipeline.

The Windows platform/helper cross-target Clippy check passes with warnings denied. The real X11 integration test passes on a private Xvfb server: actual pixel capture, focused-window metadata, shifted keyboard input, pointer delivery and cancelled-input rejection.

The native app was exercised through a private Xvfb display using software graphics and a local HTTP fixture provider. Ordered conversation context, split UTF-8 streaming, exactly-once display, interrupted restart restoration of history/drafts/tasks, the fixed Ctrl+Enter shortcut, per-bot model preferences and native task submission through a real scoped file write to durable completion all passed. Native file listing/text preview, channel task routing and a due routine dispatch also passed. No paid model or real user account was used.

The release-manifest generator was independently checked with OpenSSL. The guest export boundary tests pass without claiming a booted VM. Hardware performance checks now run separately through the manually invoked native-desktop workflow on an authorized self-hosted Mac. Missing native services, measurements or live provider credentials fail explicitly; simulations do not certify a production release.

A single post-task software-graphics sample measured approximately 116 MiB for the native UI and 159 MiB across UI/daemon/helper, above the 120 MiB whole-tree UI-open target. This is not a steady-state native-GPU measurement. The target remains unmet in this environment.

## Cross-platform release readiness follow-up

See [release readiness](release-readiness.md) for the current safety fixes, routine entry points, native import/save and bot duplication, platform service definitions, exact-target manual updates, signed candidate workflow, and evidence contract. Public release remains blocked; no signed VM/CLI-agent certification, complete research/coding workflow or hardware acceptance is claimed.
