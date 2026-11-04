# Contributing to Grok Muse

Thank you for contributing to Grok Muse! We welcome bug reports, design suggestions, and code contributions.

## Architectural Principles

1. **Local-First & Privacy-Conscious**: Grok Muse stores all state, transcripts, memories, and databases locally on the host. External network calls only happen to configured model providers, connectors, or explicitly enabled bridges.
2. **Explicit Permissions & Hard Isolation**: No arbitrary host-shell executions without bounded workspace isolation or explicit approval. Sensitive actions (file modifications outside workspace, keyboard/mouse automation, credential access) require approval cards and cannot be bypassed.
3. **Execution Reliability Over Hype**: Single runtime daemon ownership, transactional task claims, crash recovery with backup preservation, and bounded queue workers. We never silently discard or reset user data.

## Getting Started

### Prerequisites

- **Rust Toolchain**: Rust 1.85+ (Edition 2024).
- **macOS**: macOS 14+ Sonoma or Sequoia (Apple Silicon or Intel). Requires Xcode command line tools.
- **Linux**: Ubuntu 24.04+ (or equivalent modern distribution with systemd, Wayland/X11, and KVM).
- **Windows**: Windows 11 x64 with WHPX enabled.

### Building & Running

```bash
# Clone the repository
git clone https://github.com/galaxycoils/grok-muse.git
cd grok-muse

# Build the entire workspace
cargo build --workspace

# Run full test suite
cargo test --workspace

# Check for warnings and lints
cargo clippy --workspace -- -D warnings
```

## Pull Request Guidelines

1. **Focused Diffs**: Keep changes focused on a single issue or milestone.
2. **Never Weaken Tests**: Never delete, skip, or weaken existing tests to make changes pass.
3. **Trigger & Safety Invariants**:
   - Any template import must keep triggers initially disabled.
   - Any routine firing within quiet hours must record `skipped_quiet_hours` without executing.
   - Any corrupt database must be backed up to `.bak` before a fresh store is initialized.
   - Secret scrubbers must sanitize all credentials before logging or exporting diagnostics.
4. **Documentation**: When adding subcommands or modifying public contracts, update the relevant documentation in `docs/`.

## Code of Conduct

Be constructive, rigorous, and respectful in reviews and discussions.
