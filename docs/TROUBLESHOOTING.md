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
The export automatically scrubs all API keys, bearer tokens, passwords, and sensitive system settings via `SecretScrubber`.

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
- **Automatic Recovery**: Grok Muse monitors database health using `PRAGMA quick_check(1)`. If corruption is detected, the corrupted database (and any WAL/SHM files) is automatically preserved as `<name>.corrupt.<timestamp>.bak` before a fresh store is initialized. User data is never deleted silently.
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

### Issue: Claude / Codex / Grok CLI not detected
- Verify that the CLI binary is installed and present in your system `PATH`:
  - **Claude**: Install via `npm install -g @anthropic-ai/claude-code`. Authenticate via `claude login` or `export ANTHROPIC_API_KEY="..."`.
  - **Codex**: Install via `npm install -g @openai/codex`. Authenticate via `codex login` or `export OPENAI_API_KEY="..."`.
  - **Grok**: Authenticate via `grok auth` or `export XAI_API_KEY="..."`.

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
