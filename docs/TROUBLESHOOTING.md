# Grok Muse Troubleshooting Guide

This guide covers common issues, diagnostic procedures, and recovery paths for Grok Muse.

---

## 1. Running System Diagnostics

When encountering issues, first run the automated health check:

```bash
# Check all subsystem statuses
gm diagnostics check

# Or output structured JSON
gm diagnostics check --json
```

If reporting an issue to support, generate a redacted diagnostic bundle:

```bash
gm diagnostics export --output /tmp/grok-muse-support.json
```
The export scrubs registered environment secrets and the local companion token across the whole bundle, removes URL credentials/query strings, and verifies known-secret removal. Unknown secrets and personal information still require review before sharing.

---

## 2. IPC Sockets and Daemon Connectivity

### Issue: "path must be shorter than SUN_LEN" on macOS
- **Cause**: On macOS Darwin, Unix domain sockets (`sockaddr_un`) have a strict 104-byte length limit (`SUN_LEN`).
- **Fix**: Sockets are bound in `/tmp/` with short truncated hashes (e.g. `/tmp/gm-ses-*.sock`). Ensure `/tmp` is accessible and writable.

### Issue: Daemon not responding or socket disconnected
- Verify the daemon service is running:
  ```bash
  gm status
  gm service status
  ```
- If the LaunchAgent or service needs reinstallation:
  ```bash
  gm service install
  ```

---

## 3. Database Recovery and Integrity

### Issue: Database corruption or unexpected power loss
- **Explicit Recovery**: Ordinary database opens fail without resetting data. Stop the UI, companion, daemon and all CLI clients, then run `gm diagnostics recover-database`. Only confirmed corruption qualifies. Recovery requires exclusive runtime/database leases, verifies copies of every database/WAL/SHM component, and preserves originals in a unique private `corrupt-*.bak` directory. A later startup creates a new store. Lock, permission and I/O failures must be resolved without recovery.
- **Restoring from JSONL Backup**:
  ```bash
  # Check for existing backups
  ls -l "$HOME/Library/Application Support/GrokMuse/"*.bak

  # Restore queue from a JSONL export
  gm queue restore /path/to/backup.jsonl
  ```

---

## 4. macOS Privacy & Security Permissions

### Issue: "Screen Recording not granted" or "Accessibility permission missing"
- Grok Muse requires macOS permissions for computer-use automation:
  - **Screen Recording**: For capturing display state during computer-use tasks.
  - **Accessibility**: For inspecting UI elements via AXUIElement.
- **Remediation**:
  ```bash
  # Open all missing permission settings panes automatically
  gm permissions --open-missing

  # Or open specific settings pane directly
  gm permissions --open screen
  gm permissions --open accessibility
  ```

---

## 5. Model Providers & CLI Drivers

### Issue: Claude / Codex / Grok CLI execution disabled
Host execution is disabled pending the signed CLI-agent VM and egress/credential certification. Installing a binary or setting an API key does not enable it. Use Meta/xAI API routes meanwhile. Historical installation examples below do not imply supported runtime execution:

- CLI discovery checks presence only:
  - **Claude**: Install via `npm install -g @anthropic-ai/claude-code`. Authenticate via `claude login` or `export ANTHROPIC_API_KEY="..."`.
  - **Codex**: Install via `npm install -g @openai/codex`. Authenticate via `codex login` or `export OPENAI_API_KEY="..."`.
  - **Grok**: Official distribution is `@xai-official/grok`. Authenticate via `grok login --device-auth` or `export XAI_API_KEY="..."`.

### Issue: Local Candle VLM not found
- Download open-weight vision-language models for offline operation:
  ```bash
  gm models pull qwen2-vl-2b
  ```

---

## 6. Workspace VM Isolation

### Issue: VM fails to launch
- **Linux**: Verify `/dev/kvm` exists and your user belongs to the `kvm` group:
  ```bash
  ls -la /dev/kvm
  sudo usermod -aG kvm $USER
  ```
- **macOS**: Ensure Hypervisor.framework entitlement is active.
- **Windows**: Ensure Windows Hypervisor Platform (WHPX) is enabled under Windows Features.
