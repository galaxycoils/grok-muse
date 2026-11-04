# Cross-platform release readiness

Status: **blocked; development changes are not public-release certification**.

Release targets: macOS ARM64 and Intel, Windows 11 x64, Ubuntu 24.04 x64 with separate X11 and Wayland evidence. The selected release bar includes the advertised CLI-agent, VM and coding workflows. Updates remain manually installed after authenticated staging.

## Implemented in this change

- Ordinary OpenBot/usage SQLite opens fail without resetting corrupt data. Shared connection leases prevent explicit recovery while clients remain open. `gm diagnostics recover-database` also requires exclusive runtime ownership, distinguishes corruption from ordinary failures, verifies every copied database/WAL/SHM component and retains originals in a unique private quarantine directory. A later startup creates a new store.
- Queue/channel/routine, usage and pinned memory schemas have transactional version records. Existing stores are backed up with a consistent SQLite snapshot before upgrade. Newer component versions are rejected. Interrupted migrations leave the original schema/version intact. The wrapper establishes the pinned Darius schema transactionally before its idempotent initializer. Persistent-memory failures propagate rather than silently selecting an empty in-memory store. Configuration and roster also carry explicit schema versions, reject newer versions, and preserve verified legacy backups before format upgrades.
- `gm diagnostics prune-backups --keep N` explicitly removes older verified migration snapshots, retaining at least one. It never deletes corruption evidence, task records, replay/deduplication receipts, audit logs or memory. Default retention is preservation. Native conversations retain their existing bounded 500-message/2 MiB policy and clear-history control; memory upkeep remains explicit/configured.
- Host CLI invocation is disabled, including version probes. Discovery does not infer successful login from an environment variable. Screenshot routing to CLI providers is disabled. This is a safety gate, not completion of isolated CLI execution.
- `POST /api/routines/{id}/trigger`, `gm routines trigger <id> --delivery-id <id>` and `DaemonRequest::TriggerRoutine` all call the same admission logic. HTTP requires the existing companion bearer token; IPC requires the existing local peer identity. Payload is `{ "delivery_id": "1-128 visible ASCII characters" }`. Outcomes preserve `status` and nullable `task_id`: queued, duplicate, disabled, overlap_suppressed, skipped_quiet_hours. Unknown routines return HTTP 404; malformed/gate-rejected requests return 400. No schedule advancement occurs. Imported disabled routines remain inert.
- Diagnostics inspect the actual shared OpenBot database and perform bounded matching Ping/Pong probes for daemon and helper. Support exports remove URL credentials/query strings and scrub registered secrets across the entire bundle. The local companion token is included in registered secrets. `redaction_verified` means registered-secret removal only; unknown secrets/personal data still require review. Fingerprints hash compact serialized bundle JSON with `sha256_fingerprint` empty.
- Linux service management installs user systemd units tied to the graphical session. Windows uses per-user logon tasks with `InteractiveToken`, least privilege and bounded restart policy. Install/start failures propagate. Missing status reports are shown explicitly rather than reported as running. Native lifecycle certification remains required.
- Native UI has file import/save dialogs. Binary export reads only capability-scoped inbox/outbox files, at most 8 MiB; the daemon does not accept arbitrary host export destinations. UI saves only after the user selects a destination. Bot duplication copies configuration but no grants, history, files or credentials. Setup and profile fields have explicit accessibility label relationships. IME and screen-reader certification remain required.
- `gm patch <patch> --repo <worktree>` previews a bounded patch and its digest. Repeating with `--approve-sha256 <digest>` applies those exact in-memory bytes after `git apply --check`. Conflicts, path escapes, nested/non-Git roots and changed content fail. No automated agent tool can invoke host patch application. Reviewed VM patch generation/worktree execution is still required before the full coding workflow is certified.
- Updates select canonical exact-target names and independently validate signed version/OS/architecture/name/size/hash. Missing target does not select a package for another platform. Installation remains manual.

## Distribution and signing

`tools/package-release.py --version VERSION --output-dir target/release-candidate` packages four native binaries, documentation, license/notices and source-commit metadata. A clean committed checkout is mandatory. macOS requires a preconfigured Developer ID identity and notary profile, signs binaries/bundles/DMG and validates the stapled ticket. Windows requires an installed signing certificate and validates every executable signature. Linux archives include a runtime dependency inventory; system runtime libraries are prerequisites, **not yet a self-contained dependency bundle**.

`tools/sign-release.py --directory release-candidate` requires protected `GM_RELEASE_SIGNING_KEY_PEM` and independently pinned `GM_RELEASE_PUBLIC_KEY_HEX`. It checks all four build receipts against this commit/version/target, signs the canonical manifest and archives, and never creates or falls back to a development publisher key. Production builds embed the same public key. Names are:

- `GrokMuse-VERSION-macos-aarch64.dmg`
- `GrokMuse-VERSION-macos-x86_64.dmg`
- `GrokMuse-VERSION-windows-x86_64.zip`
- `GrokMuse-VERSION-linux-x86_64.tar.gz`

`.github/workflows/release.yml` is manually dispatched, produces reviewable candidate artifacts, and does not publish a GitHub release. Its protected `release` environment must provide publisher variables/secrets, signing identities and dedicated authorized `grok-muse-release` runners for each target. All checkouts use the same source commit and pinned Darius revision. Native evidence must pass before packaging starts.

The manual update procedure is: stop UI/services; close all writers; retain the old package and a complete closed-app data backup; install the verified matching package; restart services/UI; run diagnostics. If recovery is needed, stop all writers again and restore the old package **with its compatible data backup**. Never run an older binary against a newer schema. Clean-machine install/update/recovery/uninstall evidence remains outstanding.

## Guest images and CLI identities

`tools/sign-workspace-image.py` authenticates an already provisioned offline v1 qcow2 image and optional firmware using an independently pinned publisher key. It rejects backing files, encrypted images, symlink inputs and missing ARM firmware. It does not provision an image, certify a boot, implement a CLI-agent network profile, or establish firmware redistribution rights.

Official CLI package inputs are pinned with npm integrity records in `tools/workspace-guest/package-lock.json`. These are provisioning inputs only; no package was installed or executed on the host:

- Claude Code: `@anthropic-ai/claude-code@2.1.318`.
- Codex: `@openai/codex@0.166.0`.
- Grok Build: `@xai-official/grok@1.0.70`, from [xAI's official distribution](https://github.com/xai-org/grok-build/tree/main/crates/codegen/xai-grok-pager/npm/grok). [Headless documentation](https://docs.x.ai/build/cli/headless-scripting) specifies single-prompt mode and update suppression; [enterprise documentation](https://docs.x.ai/build/enterprise) identifies proxy behavior/hosts. This replaces the previously unspecified `grok` package assumption.

**Still required:** provision/test signed x64 and ARM guest images; implement the separate CLI-agent protocol and provider-only egress broker with isolated credentials and process-tree cancellation; integrate/test all three pinned CLI protocols against real accounts. The offline workspace profile must retain no network adapter and no host mounts. A provider proxy or network option must not be enabled merely because a CLI binary is present.

## Native evidence contract

`tools/verify-release-evidence.py --directory evidence --commit SHA --public-key-hex HEX` verifies the exact bytes of `evidence.json` against hex Ed25519 `evidence.sig`. Receipts expire after 30 days and must describe this exact committed source with `source: "native"`. Referenced logs/results are bounded, contained files with SHA-256 checksums. The publisher signature is an authorized reviewer attestation to the underlying real observations; the validator cannot prove that an operator collected those observations honestly.

Top-level fields: `schema: 1`, `commit`, `source`, timezone-bearing `recorded_at`, and `targets`. Every target has `os`, `architecture`, `session`, `checks`, `runs`, `measurements`, `artifacts`.

All five target/session combinations must appear. Each requires three consecutive runs of the 30 reviewed tasks in `docs/release/acceptance-tasks.json`, ten per workflow. Each task records `id`, `workflow`, boolean `success`, boolean `unapproved_irreversible`. Every run needs at least 21 successes and zero unapproved irreversible actions. All named capability/lifecycle checks in the verifier must pass. Measurements require at least 30 samples and the configured idle/UI memory, capture/AX/JPEG/dispatch/kill and step-latency targets. Artifacts are `{ "path": "relative/log", "sha256": "..." }`.

Fixtures, simulated latency and guest-protocol unit tests do not qualify. Authorized native runners or operators must collect these results, sign the reviewed receipt using the independently trusted publisher, and upload a `native-release-evidence` artifact. The candidate workflow accepts that run ID and refuses absent, stale, mismatched, incomplete, unsafe or fixture evidence.

## Remaining release blockers

1. CLI-agent VM profile, egress/credential broker, signed image provisioning and hardware boot certification.
2. End-to-end research/citation verification and isolated Git/worktree/test/patch production; host patch review/apply is implemented, complete agent workflow is not.
3. Versioned migration/retention coverage for audit/replay data; richer binary previews and remaining native integration.
4. Complete runtime dependency distribution, platform signing identities, actual signed packages and clean-machine lifecycle evidence.
5. Native desktop/accessibility/IME/multi-monitor tests, real providers/checkpoints, three-run task acceptance and performance targets on all targets.

Local validation: all-feature workspace Clippy, 225 Rust tests excluding the OS-dependent `gm-platform` test target, the additional signed HTTP staging regression, Windows foundation cross-target checking, workflow lint, three release-tool regressions and two guest boundary tests. These checks do not certify native desktop behavior or paid model quality. The full native workspace test run currently fails its unchanged macOS Accessibility permission assertion for the newly built test executable. Do not label this workspace fully tested or certified until that OS gate and the remaining target evidence pass.
