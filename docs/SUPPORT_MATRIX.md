# Grok Muse Platform Support Matrix

This matrix describes intended release targets and implemented interfaces. No target is certified for public release yet. Native desktop sessions, VM boots, model quality and install/update/uninstall must pass the release gates. It lists operating system requirements, hardware acceleration goals, and desktop API integrations for Grok Muse (`v0.3.0`).

---

## 1. Operating System & Architecture Support

| Operating System | Architecture | Support Tier | Workspace VM Hypervisor | Local VLM Acceleration | Status |
|---|---|---|---|---|---|
| **macOS 14+ (Sonoma, Sequoia)** | Apple Silicon (`aarch64`) | **Release target (uncertified)** | HVF (Hypervisor.framework) | Metal (MPS) | Native certification pending |
| **macOS 14+ (Sonoma, Sequoia)** | Intel (`x86_64`) | **Release target (uncertified)** | HVF (Hypervisor.framework) | CPU / Accelerate | Native certification pending |
| **Linux (Ubuntu 24.04 LTS)** | AMD64 (`x86_64`) | **Release target (uncertified)** | KVM (`/dev/kvm`) | CPU; CUDA/Vulkan unverified | Native certification pending |
| **Windows 11 (23H2+)** | AMD64 (`x86_64`) | **Release target (uncertified)** | WHPX (Windows Hypervisor Platform) | CPU; other acceleration unverified | Native certification pending |
| Other Linux Distributions | `x86_64` / `aarch64` | Tier 2 (Experimental) | KVM | CPU | Unverified |

---

## 2. Native Desktop Integration & Subsystem Matrix

| Subsystem | macOS Implementation | Linux Implementation | Windows Implementation |
|---|---|---|---|
| **Screen Capture** | ScreenCaptureKit / CGDisplayStream | Desktop Portal / PipeWire | GDI virtual-desktop capture |
| **Accessibility & Inspection** | `AXUIElement` / Accessibility APIs | AT-SPI2 D-Bus (`org.a11y.Bus`) | Microsoft UI Automation (UIA) |
| **Input Simulation** | `CGEvent` synthesis | XTest (X11) / RemoteDesktop portal (Wayland) | `SendInput` / Win32 Synthesized Input |
| **Clipboard** | `NSPasteboard` | `wl-clipboard` / `xclip` | Win32 OLE / Clipboard API |
| **Notifications** | `NSUserNotificationCenter` / UNNotification | `org.freedesktop.Notifications` | Windows Toast Notifications |
| **Credential Storage** | macOS Keychain (`security-framework`) | Secret Service API / libsecret | Windows Credential Manager (`keyring`) |
| **IPC Daemon Transport** | Unix Domain Sockets (`sockaddr_un`) | Unix Domain Sockets (XDG runtime) | Named Pipes (`\\.\pipe\...`) |

---

## 3. Supported CLI & Model Providers

| Provider | Method | Supported Models | Availability Checks | Auth Flow | Cost Metering |
|---|---|---|---|---|---|
| **Meta AI (Llama)** | Cloud API | `muse-spark`, `muse-wonder` | Ping endpoint / API key presence | API Key in OS Keychain | Bounded meter ($0.80-$3.20/M tokens) |
| **xAI Grok** | Cloud API | `grok-4`, `grok-vision-beta` | Ping endpoint / API key presence | API Key in OS Keychain | Bounded meter ($3.00-$15.00/M tokens) |
| **Anthropic Claude** | CLI Driver | `claude-3-5-sonnet`, `claude-3-opus` | `which claude`, `--version` check | `claude login` / `ANTHROPIC_API_KEY` | Disabled pending isolated CLI-agent VM; cost unknown |
| **OpenAI Codex** | CLI Driver | `codex`, `gpt-4o` | `which codex`, `--version` check | `codex login` / `OPENAI_API_KEY` | Disabled pending isolated CLI-agent VM; cost unknown |
| **xAI Grok CLI** | CLI Driver | `grok` | `which grok`, `--version` check | `grok login --device-auth` / `XAI_API_KEY` | Disabled pending isolated CLI-agent VM; cost unknown |
| **Offline Candle VLM** | Local Engine | `Qwen2-VL-2B-Instruct`, `SmolVLM2` | Installed weights registry check | None (offline open weights) | Free (local compute) |

---

## 4. Workflows

1. **General Assistance & Research**: Source-backed answers, file manipulation, multi-bot channels, reviewable deliverables.
2. **Coding**: Isolated Linux workspace VMs, Git diff generation, patch review, sandboxed test execution.
3. **Desktop Automation**: Scoped application control, interactive approval cards, observed UI outcomes, "Take the Wheel" takeover.

Host CLI drivers are disabled, including version execution during discovery. CLI availability does not certify authentication. A networked CLI-agent VM profile is still release-blocking; the existing offline workspace VM does not provide it.
