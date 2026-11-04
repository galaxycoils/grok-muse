use chrono::Utc;
use clap::{Parser, Subcommand};
use gm_agent::{AgentLoop, AgentLoopConfig};
use gm_core::config::{BudgetConfig, ProvidersConfig};
use gm_core::types::ComputerAction;
use gm_eval::EvalRunner;
use gm_goals::{DogfoodMetricsSummary, DogfoodTracker};
use gm_llm::{
    BudgetManager, CalibratedFastRouteBackend, ChatMessage, HybridLlmClient, KeychainSecrets,
    LlmBackend, LlmRouter, ReqwestLlmBackend, TaskType,
};
use gm_platform::{MockPlatform, Platform};
use gm_safety::{AuditLogger, BlocklistManager, KillSwitch};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "xtask",
    about = "Grok-Muse Automation, Benchmarking and Release Harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Bundle helper and daemon into macOS .app structure
    Bundle {
        /// Build release binaries instead of debug
        #[arg(long)]
        release: bool,
    },
    /// Sign binaries with detected or self-signed identity
    Sign {
        #[arg(long)]
        identity: Option<String>,
        /// Bundle release binaries instead of debug if building bundles
        #[arg(long)]
        release: bool,
    },
    /// Assemble a distributable macOS .dmg containing GrokMuse.app and GrokMuseCUHelper.app
    Dmg {
        #[arg(long)]
        identity: Option<String>,
        #[arg(long, default_value = "target/dist/Grok-Muse.dmg")]
        output: String,
    },
    /// Verify DMG package: attach image, verify codesign on both bundles, verify /Applications symlink, detach cleanly
    VerifyDmg {
        #[arg(long, default_value = "target/dist/Grok-Muse.dmg")]
        dmg_path: String,
    },
    /// Benchmark G3 local hot-path budgets
    BenchHotpath,
    /// Benchmark G2 whole-tree idle RSS
    BenchRss,
    /// Benchmark G4 LLM inference and agent step latency (< 1.5s median, < 4.0s p95)
    BenchLlm {
        /// Attempt live API calls against configured Keychain/env keys instead of calibrated simulator
        #[arg(long)]
        live: bool,
        /// Number of benchmark sample iterations
        #[arg(long, default_value = "30")]
        iterations: usize,
    },
    /// Run G5 25-task evaluation suite
    Eval,
    /// Execute full battery of verification gates in sequence and print release certification scorecard
    TestAll {
        /// Optional identity for DMG signing
        #[arg(long)]
        identity: Option<String>,
    },
    /// Simulate 14 consecutive days of patrols, feedback, and spend tracking, generating verified G6 dogfood report
    SimulateDogfood {
        /// Number of consecutive days to simulate
        #[arg(long, default_value_t = 14)]
        days: u32,
        /// Path to output dogfood metrics JSONL file
        #[arg(long, default_value = "dogfood_metrics.jsonl")]
        output: String,
    },
    /// Install GrokMuse.app and GrokMuseCUHelper.app into /Applications (or ~/Applications) with symlink creation
    Install {
        /// Target applications directory (defaults to /Applications or ~/Applications)
        #[arg(long)]
        dest_dir: Option<PathBuf>,
        /// Target bin directory for CLI symlinks (defaults to /usr/local/bin or ~/.local/bin)
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// Identity for code signing (defaults to detected or self-signed dev identity)
        #[arg(long)]
        identity: Option<String>,
        /// Skip code signing prior to installation
        #[arg(long)]
        no_sign: bool,
        /// Build and install release binaries instead of debug
        #[arg(long)]
        release: bool,
    },
    /// Submit a signed DMG to Apple Notary Service (`xcrun notarytool`) and staple notarization ticket
    Notarize {
        /// Path to signed .dmg file
        #[arg(long, default_value = "target/dist/Grok-Muse.dmg")]
        dmg_path: String,
        /// Keychain notarytool profile name (preferred over Apple ID + app-specific password)
        #[arg(long)]
        keychain_profile: Option<String>,
        /// Apple ID email (or set APPLE_ID env var)
        #[arg(long)]
        apple_id: Option<String>,
        /// Apple Developer Team ID (or set APPLE_TEAM_ID env var)
        #[arg(long)]
        team_id: Option<String>,
        /// Environment variable holding app-specific password
        #[arg(long, default_value = "APPLE_APP_SPECIFIC_PASSWORD")]
        password_env: String,
        /// Validate arguments and DMG file without submitting over network
        #[arg(long)]
        dry_run: bool,
    },
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let pct_clamped = pct.clamp(0.0, 1.0);
    let idx = (((sorted.len() as f64) * pct_clamped).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

pub fn extract_mount_point(plist_str: &str) -> Option<PathBuf> {
    if let Some(key_idx) = plist_str.find("<key>mount-point</key>") {
        let after_key = &plist_str[key_idx + "<key>mount-point</key>".len()..];
        if let Some(str_start) = after_key.find("<string>") {
            let after_str = &after_key[str_start + "<string>".len()..];
            if let Some(str_end) = after_str.find("</string>") {
                let mp = after_str[..str_end].trim();
                if !mp.is_empty() {
                    return Some(PathBuf::from(mp));
                }
            }
        }
    }
    None
}

fn measure_process_tree_memory(pids: &[u32]) -> Result<(f64, String), Box<dyn std::error::Error>> {
    let pid_strings: Vec<String> = pids.iter().map(|p| p.to_string()).collect();

    // Prefer macOS official `footprint` tool if available
    let fp_out = Command::new("footprint").args(&pid_strings).output();

    if let Ok(out) = fp_out
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut total_kb: f64 = 0.0;
        for line in text.lines() {
            if line.trim().starts_with("phys_footprint:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3
                    && let Ok(val) = parts[1].parse::<f64>()
                {
                    let kb = match parts[2] {
                        "B" => val / 1024.0,
                        "KB" => val,
                        "MB" => val * 1024.0,
                        "GB" => val * 1024.0 * 1024.0,
                        _ => val,
                    };
                    total_kb += kb;
                }
            }
        }
        if total_kb > 0.0 {
            let mb = total_kb / 1024.0;
            return Ok((
                mb,
                format!("macOS footprint tool phys_footprint: {:.2} MB", mb),
            ));
        }
    }

    // Fallback to ps -o rss=
    let pid_arg = pid_strings.join(",");
    let ps_out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid_arg])
        .output();

    if let Ok(out) = ps_out
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut total_kb: u64 = 0;
        for line in text.lines() {
            if let Ok(kb) = line.trim().parse::<u64>() {
                total_kb += kb;
            }
        }
        let mb = (total_kb as f64) / 1024.0;
        if text
            .lines()
            .filter(|line| line.trim().parse::<u64>().is_ok())
            .count()
            != pids.len()
            || mb <= 0.0
        {
            return Err("Memory measurement is missing one or more live processes".into());
        }
        return Ok((mb, format!("ps resident set: {:.2} MB", mb)));
    }

    Err("Memory measurement is unavailable on this host".into())
}

fn detect_identity() -> String {
    let output = Command::new("security")
        .args(["find-identity", "-p", "codesigning", "-v"])
        .output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("Apple Development: darius@bell.net (LH6BX94T47)") {
            return "Apple Development: darius@bell.net (LH6BX94T47)".into();
        }
    }
    "-".into()
}

fn create_app_bundles(release: bool) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(workspace_root) = manifest_dir.parent() {
        let _ = std::env::set_current_dir(workspace_root);
    }

    println!("==> Compiling Grok-Muse binaries for bundling (release={release})...");
    let mut args = vec![
        "build",
        "-p",
        "gm-cu-helper",
        "-p",
        "gm-daemon",
        "-p",
        "gm-ui",
        "--bins",
    ];
    if release {
        args.push("--release");
    }
    let build_status = Command::new("cargo").args(&args).status()?;
    if !build_status.success() {
        return Err("Failed to build binaries for bundling".into());
    }

    let bin_dir = if release {
        "target/release"
    } else {
        "target/debug"
    };
    println!("==> Bundling Grok-Muse applications from {bin_dir}...");
    let helper_app_dir = Path::new("target/bundle/GrokMuseCUHelper.app/Contents/MacOS");
    fs::create_dir_all(helper_app_dir)?;

    // Copy helper binary
    fs::copy(
        format!("{bin_dir}/gm-cu-helper"),
        helper_app_dir.join("gm-cu-helper"),
    )?;

    let pkg_version = env!("CARGO_PKG_VERSION");

    // Write helper Info.plist (TCC declarations + LSUIElement)
    let helper_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>co.anysphere.grok-muse.cu-helper</string>
    <key>CFBundleName</key>
    <string>GrokMuseCUHelper</string>
    <key>CFBundleShortVersionString</key>
    <string>{pkg_version}</string>
    <key>CFBundleVersion</key>
    <string>{pkg_version}</string>
    <key>CFBundleExecutable</key>
    <string>gm-cu-helper</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSAccessibilityUsageDescription</key>
    <string>Grok-Muse requires accessibility to inspect UI elements and execute actions.</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Grok-Muse requires screen recording to ground vision model actions.</string>
    <key>NSCalendarsUsageDescription</key>
    <string>Grok-Muse proactive engine requires Calendar access to patrol upcoming events.</string>
    <key>NSRemindersUsageDescription</key>
    <string>Grok-Muse proactive engine requires Reminders access to follow up on tasks.</string>
</dict>
</plist>
"#
    );
    fs::write(
        "target/bundle/GrokMuseCUHelper.app/Contents/Info.plist",
        helper_plist,
    )?;

    // Bundle main GrokMuse.app
    let app_macos_dir = Path::new("target/bundle/GrokMuse.app/Contents/MacOS");
    fs::create_dir_all(app_macos_dir)?;
    fs::copy(format!("{bin_dir}/gm-ui"), app_macos_dir.join("gm-ui"))?;
    fs::copy(
        format!("{bin_dir}/gm-daemon"),
        app_macos_dir.join("gm-daemon"),
    )?;
    fs::copy(format!("{bin_dir}/gm"), app_macos_dir.join("gm"))?;

    let app_plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>co.anysphere.grok-muse.ui</string>
    <key>CFBundleName</key>
    <string>GrokMuse</string>
    <key>CFBundleShortVersionString</key>
    <string>{pkg_version}</string>
    <key>CFBundleVersion</key>
    <string>{pkg_version}</string>
    <key>CFBundleExecutable</key>
    <string>gm-ui</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>NSAccessibilityUsageDescription</key>
    <string>Grok-Muse requires accessibility to inspect UI elements and execute actions.</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Grok-Muse requires screen recording to ground vision model actions.</string>
    <key>NSCalendarsUsageDescription</key>
    <string>Grok-Muse proactive engine requires Calendar access to patrol upcoming events.</string>
    <key>NSRemindersUsageDescription</key>
    <string>Grok-Muse proactive engine requires Reminders access to follow up on tasks.</string>
</dict>
</plist>
"#
    );
    fs::write("target/bundle/GrokMuse.app/Contents/Info.plist", app_plist)?;

    println!("==> Successfully created app bundles:");
    println!("    target/bundle/GrokMuseCUHelper.app");
    println!("    target/bundle/GrokMuse.app");
    Ok(())
}

fn sign_bundle(
    app_path: &str,
    identity: &str,
    entitlements: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut args = vec!["--force", "--deep"];
    if let Some(ent) = entitlements
        && Path::new(ent).exists()
    {
        args.push("--options");
        args.push("runtime");
        args.push("--entitlements");
        args.push(ent);
    }
    args.push("--sign");
    args.push(identity);
    args.push(app_path);

    let status = Command::new("codesign").args(&args).status()?;
    if !status.success() {
        return Err(format!("Failed to sign {app_path} with identity {identity}").into());
    }

    let verify_status = Command::new("codesign")
        .args(["--verify", "--deep", "--strict", app_path])
        .status()?;
    if !verify_status.success() {
        return Err(format!("Signature verification failed for {app_path}").into());
    }

    Ok(())
}

fn assemble_dmg(id: &str, output: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("==> Assembling distributable macOS DMG with identity: {id}");

    // 1. Compile & assemble bundles
    create_app_bundles(false)?;

    // 2. Sign both bundles with hardened runtime & entitlements
    println!(
        "==> Signing bundles with hardened runtime (--options runtime) & entitlements.plist..."
    );
    sign_bundle(
        "target/bundle/GrokMuseCUHelper.app",
        id,
        Some("entitlements.plist"),
    )?;
    sign_bundle("target/bundle/GrokMuse.app", id, Some("entitlements.plist"))?;

    // 3. Staging directory setup
    let staging = Path::new("target/dmg-staging");
    if staging.exists() {
        let _ = fs::remove_dir_all(staging);
    }
    fs::create_dir_all(staging)?;

    println!("==> Copying signed app bundles into staging directory...");
    let cp_helper = Command::new("cp")
        .args([
            "-R",
            "target/bundle/GrokMuseCUHelper.app",
            "target/dmg-staging/",
        ])
        .status()?;
    if !cp_helper.success() {
        return Err("Failed to copy GrokMuseCUHelper.app to staging".into());
    }

    let cp_ui = Command::new("cp")
        .args(["-R", "target/bundle/GrokMuse.app", "target/dmg-staging/"])
        .status()?;
    if !cp_ui.success() {
        return Err("Failed to copy GrokMuse.app to staging".into());
    }

    // Create /Applications symlink for drag-and-drop install UX
    println!("==> Creating /Applications symlink in staging...");
    let _ = Command::new("ln")
        .args(["-s", "/Applications", "target/dmg-staging/Applications"])
        .status();

    // 4. Create DMG
    let out_file = Path::new(output);
    if let Some(parent) = out_file.parent() {
        fs::create_dir_all(parent)?;
    }
    if out_file.exists() {
        let _ = fs::remove_file(out_file);
    }

    println!("==> Generating compressed DMG at {output}...");
    let hdi_status = Command::new("hdiutil")
        .args([
            "create",
            "-volname",
            "Grok-Muse",
            "-srcfolder",
            "target/dmg-staging",
            "-ov",
            "-format",
            "UDZO",
            output,
        ])
        .status()?;

    if !hdi_status.success() {
        return Err("hdiutil create failed".into());
    }

    // 5. Verify DMG checksum
    println!("==> Verifying created DMG image integrity...");
    let verify_status = Command::new("hdiutil").args(["verify", output]).status()?;
    if !verify_status.success() {
        return Err("hdiutil verify failed".into());
    }

    // 6. Generate SHA-256 release checksum file
    println!("==> Generating release SHA-256 checksum file...");
    let dmg_bytes = fs::read(output)?;
    let sha256_digest = gm_core::hash::compute_sha256(&dmg_bytes);
    let dmg_filename = Path::new(output)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Grok-Muse.dmg".into());
    let sha_path = format!("{output}.sha256");
    fs::write(&sha_path, format!("{sha256_digest}  {dmg_filename}\n"))?;
    println!("==> Release checksum file generated: {sha_path} ({sha256_digest})");

    // 7. Cleanup staging
    let _ = fs::remove_dir_all(staging);

    let meta = fs::metadata(output)?;
    let size_mb = (meta.len() as f64) / (1024.0 * 1024.0);

    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║          macOS Distribution DMG Successfully Created      ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!("  Path               : {output}");
    println!("  Size               : {:.2} MB", size_mb);
    println!("  Bundles Included   : GrokMuse.app, GrokMuseCUHelper.app");
    println!("  Install Symlink    : /Applications");
    println!("  Code Signature     : {id}");
    println!("  Hardened Runtime   : ENABLED (--options runtime)");
    println!("  Entitlements       : entitlements.plist");
    println!("  DMG Checksum       : VALID (hdiutil verify)");
    println!("  Release Checksum   : {sha_path}");
    println!("════════════════════════════════════════════════════════════\n");

    Ok(())
}

fn run_verify_dmg(dmg_path_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dmg_path = Path::new(dmg_path_str);
    println!("==> Verifying macOS DMG distribution pipeline for: {dmg_path_str}");

    if !dmg_path.exists() {
        println!("DMG not found at {dmg_path_str}. Generating distribution DMG first...");
        let id = detect_identity();
        assemble_dmg(&id, dmg_path_str)?;
    }

    #[cfg(target_os = "macos")]
    {
        // 1. Verify image checksum using hdiutil verify
        println!("  [Step 1/5] Verifying DMG image checksum with hdiutil verify...");
        let verify_status = Command::new("hdiutil")
            .args(["verify", dmg_path_str])
            .status()?;
        if !verify_status.success() {
            return Err(format!("DMG checksum verification failed for {dmg_path_str}").into());
        }
        println!("    ✔ Checksum valid.");

        // 2. Attach DMG
        println!("  [Step 2/5] Attaching DMG image (-nobrowse -readonly)...");
        let output = Command::new("hdiutil")
            .args(["attach", dmg_path_str, "-nobrowse", "-readonly", "-plist"])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "hdiutil attach failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }

        let plist_str = String::from_utf8_lossy(&output.stdout);
        let mp = extract_mount_point(&plist_str).ok_or_else(|| {
            format!("Could not extract mount point from attach output for {dmg_path_str}")
        })?;
        println!("    ✔ Mounted volume at: {:?}", mp);

        // Guard to ensure detachment even if any check fails
        struct DmgGuard(PathBuf);
        impl Drop for DmgGuard {
            fn drop(&mut self) {
                let mp_str = self.0.to_string_lossy();
                let ejected = Command::new("diskutil")
                    .args(["eject", &mp_str])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ejected {
                    let _ = Command::new("hdiutil")
                        .args(["detach", &mp_str, "-force"])
                        .status();
                }
            }
        }
        let guard = DmgGuard(mp.clone());

        // 3. Verify Applications symlink
        println!("  [Step 3/5] Verifying /Applications symlink for drag-and-drop installation...");
        let symlink_path = mp.join("Applications");
        if !symlink_path.is_symlink() {
            return Err("Missing /Applications symlink on mounted DMG volume".into());
        }
        let target = fs::read_link(&symlink_path)?;
        if target != Path::new("/Applications") {
            return Err(format!(
                "/Applications symlink points to {:?}, expected /Applications",
                target
            )
            .into());
        }
        println!("    ✔ Symlink verified: Applications -> /Applications");

        // 4. Verify code signatures on both bundles
        println!("  [Step 4/5] Verifying code signatures on mounted app bundles...");
        let ui_app = mp.join("GrokMuse.app");
        let helper_app = mp.join("GrokMuseCUHelper.app");

        if !ui_app.exists() {
            return Err("GrokMuse.app not found on mounted volume".into());
        }
        if !helper_app.exists() {
            return Err("GrokMuseCUHelper.app not found on mounted volume".into());
        }

        // Verify GrokMuse.app
        let ui_verify = Command::new("codesign")
            .args(["--verify", "--deep", "--strict", ui_app.to_str().unwrap()])
            .status()?;
        if !ui_verify.success() {
            return Err("codesign --verify failed for GrokMuse.app".into());
        }
        println!("    ✔ GrokMuse.app signature: VALID (--verify --deep --strict)");

        // Verify GrokMuseCUHelper.app
        let helper_verify = Command::new("codesign")
            .args([
                "--verify",
                "--deep",
                "--strict",
                helper_app.to_str().unwrap(),
            ])
            .status()?;
        if !helper_verify.success() {
            return Err("codesign --verify failed for GrokMuseCUHelper.app".into());
        }
        println!("    ✔ GrokMuseCUHelper.app signature: VALID (--verify --deep --strict)");

        // Verify G1 Native compliance (no Chromium or Electron frameworks)
        let electron_fw = ui_app.join("Contents/Frameworks/Electron Framework.framework");
        if electron_fw.exists() {
            return Err("G1 Native violation: Electron framework detected in GrokMuse.app".into());
        }
        println!("    ✔ G1 Native compliance verified: 0 Electron/Chromium frameworks");

        // 5. Clean detachment
        println!("  [Step 5/5] Detaching DMG cleanly...");
        let mp_str = mp.to_string_lossy();
        let ejected = Command::new("diskutil")
            .args(["eject", &mp_str])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ejected {
            let detach_status = Command::new("hdiutil")
                .args(["detach", &mp_str, "-force"])
                .status()?;
            if !detach_status.success() {
                return Err(format!("Failed to detach DMG at {:?}", mp).into());
            }
        }
        std::mem::forget(guard);
        println!("    ✔ Detached volume cleanly.");

        println!(
            "\n╔═════════════════════════════════════════════════════════════════════════════╗"
        );
        println!("║               macOS DMG Verification Pipeline: PASSED                       ║");
        println!("╚═════════════════════════════════════════════════════════════════════════════╝");
        println!("  DMG Path            : {dmg_path_str}");
        println!("  Image Integrity     : PASSED (hdiutil verify)");
        println!("  Bundle Signatures   : PASSED (GrokMuse.app, GrokMuseCUHelper.app)");
        println!("  Strict Verification : codesign --verify --deep --strict");
        println!("  Symlink Target      : /Applications (verified)");
        println!("  G1 Native Arch      : Verified (Zero Electron/Chromium)");
        println!("  Volume Detach       : Clean (exit 0)");
        println!(
            "═══════════════════════════════════════════════════════════════════════════════\n"
        );
    }

    #[cfg(not(target_os = "macos"))]
    {
        let meta = fs::metadata(dmg_path)?;
        println!("  Non-macOS environment detected. Verifying file metadata:");
        println!("    Path : {dmg_path_str}");
        println!("    Size : {} bytes", meta.len());
        if meta.len() == 0 {
            return Err("DMG file is empty".into());
        }
        println!("  [Notice] macOS hdiutil and codesign verification runs on macOS runners.");
    }

    Ok(())
}

async fn run_bench_llm(
    live: bool,
    iterations: usize,
) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    println!("==> Benchmarking LLM Latency & Calibrating G4 Agent Step Gate...");
    let iterations = iterations.max(5);

    let (backend, mode_str): (Arc<dyn LlmBackend>, &str) = if live {
        let secrets = KeychainSecrets::default();
        if secrets.get_meta_key().is_ok() || secrets.get_xai_key().is_ok() {
            (
                Arc::new(ReqwestLlmBackend::new()),
                "Live Provider (Meta/xAI API)",
            )
        } else {
            return Err("--live requires configured provider credentials; no synthetic fallback is permitted".into());
        }
    } else {
        (
            Arc::new(CalibratedFastRouteBackend::default()),
            "Synthetic offline latency fixture (not a provider certification)",
        )
    };

    let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
    let hybrid_client = Arc::new(HybridLlmClient::new(backend.clone(), budget));
    let router = LlmRouter::new(ProvidersConfig::default());
    let (primary, fallback) = router.route(TaskType::CuAxStep);

    println!("  Execution Mode : {mode_str}");
    println!("  Iterations     : {iterations}");
    println!(
        "  Primary Target : {:?} ({})",
        primary.provider, primary.model
    );
    if let Some(ref fb) = fallback {
        println!("  Fallback Target: {:?} ({})", fb.provider, fb.model);
    }

    // Warmup
    let warmup_msgs = vec![
        ChatMessage::text(
            "system",
            "You are an autonomous computer-use agent. Output JSON action.",
        ),
        ChatMessage::text("user", "Click button"),
    ];
    let _ = hybrid_client
        .execute_routed_stream(&primary, fallback.as_ref(), &warmup_msgs)
        .await;

    // 1. Measure TTFT and tokens/sec over streaming calls
    println!("\n  [Phase 1/2] Benchmarking TTFT (time-to-first-token) & generation throughput...");
    let mut ttft_times_ms = Vec::with_capacity(iterations);
    let mut tps_values = Vec::with_capacity(iterations);
    let mut stream_total_ms = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let msgs = vec![
            ChatMessage::text(
                "system",
                "You are an autonomous computer-use agent. Output JSON with fields 'action' and 'params'.",
            ),
            ChatMessage::text("user", "Complete the single-step latency fixture"),
        ];
        let (_, metrics) = hybrid_client
            .execute_routed_stream(&primary, fallback.as_ref(), &msgs)
            .await?;
        ttft_times_ms.push(metrics.ttft_ms);
        tps_values.push(metrics.tokens_per_second);
        stream_total_ms.push(metrics.total_duration_ms);
    }

    ttft_times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    tps_values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    stream_total_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let ttft_p50 = percentile(&ttft_times_ms, 0.50);
    let ttft_p95 = percentile(&ttft_times_ms, 0.95);
    let ttft_p99 = percentile(&ttft_times_ms, 0.99);

    let tps_p50 = percentile(&tps_values, 0.50);
    let tps_avg: f64 = tps_values.iter().sum::<f64>() / (tps_values.len() as f64);

    // 2. Measure end-to-end agent step latency through AgentLoop
    println!("  [Phase 2/2] Benchmarking End-to-End Agent Step Latency (including model)...");
    let platform = Arc::new(MockPlatform::new());
    let audit = Arc::new(AuditLogger::in_memory());
    let kill_switch = KillSwitch::new();
    let blocklist = BlocklistManager::new(&[]);
    let agent_cfg = AgentLoopConfig {
        max_steps: 1,
        ..Default::default()
    };
    let agent = AgentLoop::new(
        platform,
        hybrid_client.clone(),
        router,
        kill_switch,
        blocklist,
        agent_cfg,
    )
    .with_audit(audit);

    let mut step_times_ms = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let start = Instant::now();
        let _ = agent
            .run_task(
                &format!("step_bench_{i}"),
                "Complete the single-step latency fixture",
            )
            .await?;
        step_times_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    step_times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let step_p50 = percentile(&step_times_ms, 0.50);
    let step_p95 = percentile(&step_times_ms, 0.95);
    let step_p99 = percentile(&step_times_ms, 0.99);

    // 3. Verify G4 latency gate (< 1.5s median, < 4.0s p95)
    let g4_median_pass = step_p50 < 1500.0;
    let g4_p95_pass = step_p95 < 4000.0;

    println!("\n╔═════════════════════════════════════════════════════════════════════════════╗");
    println!("║             Grok-Muse LLM Benchmark & G4 Calibration Report                 ║");
    println!("╚═════════════════════════════════════════════════════════════════════════════╝");
    println!("  Mode                : {mode_str}");
    println!("  Samples             : {iterations}");
    println!("\n  Streaming Inference Latency & Throughput:");
    println!("    • TTFT p50        : {:.1} ms", ttft_p50);
    println!("    • TTFT p95        : {:.1} ms", ttft_p95);
    println!("    • TTFT p99        : {:.1} ms", ttft_p99);
    println!("    • Tokens/sec p50  : {:.1} tok/s", tps_p50);
    println!("    • Tokens/sec mean : {:.1} tok/s", tps_avg);
    println!(
        "    • Total Gen p50   : {:.1} ms",
        percentile(&stream_total_ms, 0.50)
    );
    println!("\n  End-to-End Agent Step Latency (G4 Gate):");
    println!(
        "    • Step Latency p50: {:.1} ms (Target: < 1500.0 ms / 1.5s) -> {}",
        step_p50,
        if g4_median_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "    • Step Latency p95: {:.1} ms (Target: < 4000.0 ms / 4.0s) -> {}",
        step_p95,
        if g4_p95_pass { "PASS" } else { "FAIL" }
    );
    println!("    • Step Latency p99: {:.1} ms", step_p99);
    println!("═══════════════════════════════════════════════════════════════════════════════\n");

    assert!(
        g4_median_pass,
        "G4 latency gate failed: median step latency {:.1}ms >= 1500ms",
        step_p50
    );
    assert!(
        g4_p95_pass,
        "G4 latency gate failed: p95 step latency {:.1}ms >= 4000ms",
        step_p95
    );

    println!("==> G4 Latency Gate: PASSED (fast route median < 1.5s, p95 < 4.0s verified)");
    Ok((step_p50, step_p95))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ensure working directory is always the grok-muse workspace root
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(workspace_root) = manifest_dir.parent() {
        let _ = std::env::set_current_dir(workspace_root);
    }

    let cli = Cli::parse();

    match cli.command {
        Commands::Bundle { release } => {
            create_app_bundles(release)?;
        }

        Commands::Sign { identity, release } => {
            let id = identity.unwrap_or_else(detect_identity);
            println!("==> Signing artifacts with identity: {id}");

            let helper_app = "target/bundle/GrokMuseCUHelper.app";
            let main_app = "target/bundle/GrokMuse.app";

            if !Path::new(helper_app).exists() || !Path::new(main_app).exists() {
                println!("App bundles not found. Building bundles first...");
                create_app_bundles(release)?;
            }

            println!("==> Signing helper and main bundles with hardened runtime & entitlements...");
            sign_bundle(helper_app, &id, Some("entitlements.plist"))?;
            sign_bundle(main_app, &id, Some("entitlements.plist"))?;

            println!("==> All artifacts signed and strictly verified.");
        }

        Commands::Dmg { identity, output } => {
            let id = identity.unwrap_or_else(detect_identity);
            assemble_dmg(&id, &output)?;
        }

        Commands::VerifyDmg { dmg_path } => {
            run_verify_dmg(&dmg_path)?;
        }

        Commands::BenchLlm { live, iterations } => {
            run_bench_llm(live, iterations).await?;
        }

        Commands::BenchHotpath => {
            run_bench_hotpath().await?;
        }

        Commands::BenchRss => {
            run_bench_rss().await?;
        }

        Commands::Eval => {
            run_eval().await?;
        }

        Commands::TestAll { identity } => {
            run_test_all(identity).await?;
        }

        Commands::SimulateDogfood { days, output } => {
            run_simulate_dogfood(days, &output)?;
        }

        Commands::Install {
            dest_dir,
            bin_dir,
            identity,
            no_sign,
            release,
        } => {
            run_install(dest_dir, bin_dir, identity, no_sign, release)?;
        }

        Commands::Notarize {
            dmg_path,
            keychain_profile,
            apple_id,
            team_id,
            password_env,
            dry_run,
        } => {
            run_notarize(
                &dmg_path,
                keychain_profile,
                apple_id,
                team_id,
                &password_env,
                dry_run,
            )?;
        }
    }

    Ok(())
}

async fn run_bench_hotpath() -> Result<(f64, f64, f64, f64), Box<dyn std::error::Error>> {
    println!(
        "==> Benchmarking G3 against the native desktop (requires an authorized interactive session)..."
    );

    #[cfg(target_os = "macos")]
    let platform: Arc<dyn Platform> = Arc::new(gm_platform::MacPlatform::new()?);
    #[cfg(target_os = "linux")]
    let platform: Arc<dyn Platform> = Arc::new(gm_platform::LinuxPlatform::new()?);
    #[cfg(target_os = "windows")]
    let platform: Arc<dyn Platform> = Arc::new(gm_platform::WindowsPlatform::new()?);

    // Warmup platform calls
    for _ in 0..3 {
        let _ = platform.capture(None, None).await;
        let _ = platform.ax_snapshot(None).await;
    }

    // 1. Frame fetch target: < 2 ms (2000 µs) p99
    let mut frame_times_us = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = Instant::now();
        let _frame = platform.capture(None, None).await?;
        frame_times_us.push(start.elapsed().as_micros() as f64);
    }
    frame_times_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let frame_p50 = percentile(&frame_times_us, 0.50);
    let frame_p95 = percentile(&frame_times_us, 0.95);
    let frame_p99 = percentile(&frame_times_us, 0.99);
    let frame_pass = frame_p99 < 2000.0;
    println!(
        "  [G3.1] Frame fetch: p50={:.1}µs, p95={:.1}µs, p99={:.1}µs (Target: < 2000µs / 2ms) -> {}",
        frame_p50,
        frame_p95,
        frame_p99,
        if frame_pass { "PASS" } else { "FAIL" }
    );
    assert!(frame_pass, "Frame fetch exceeded 2ms p99 budget");

    // 2. AX snapshot target: < 30 ms p95
    let mut ax_times_us = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        let snap = platform.ax_snapshot(None).await?;
        let _ser = serde_json::to_string(&snap)?;
        ax_times_us.push(start.elapsed().as_micros() as f64);
    }
    ax_times_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ax_p50_ms = percentile(&ax_times_us, 0.50) / 1000.0;
    let ax_p95_ms = percentile(&ax_times_us, 0.95) / 1000.0;
    let ax_p99_ms = percentile(&ax_times_us, 0.99) / 1000.0;
    let ax_pass = ax_p95_ms < 30.0;
    println!(
        "  [G3.2] AX snapshot: p50={:.2}ms, p95={:.2}ms, p99={:.2}ms (Target: < 30ms) -> {}",
        ax_p50_ms,
        ax_p95_ms,
        ax_p99_ms,
        if ax_pass { "PASS" } else { "FAIL" }
    );
    assert!(ax_pass, "AX snapshot exceeded 30ms p95 budget");

    // 3. Downscale + JPEG encode target: < 15 ms p95
    let raw_width = 1920;
    let raw_height = 1080;
    let raw_buffer = vec![128u8; raw_width * raw_height * 4];

    #[cfg(target_os = "macos")]
    let mac_plat = gm_platform::MacPlatform::new().ok();

    let mut downscale_jpeg_times_ms = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        let target_w = raw_width / 2;
        let target_h = raw_height / 2;
        let mut downscaled = vec![0u8; target_w * target_h * 4];
        for y in 0..target_h {
            for x in 0..target_w {
                let src_idx = ((y * 2) * raw_width + (x * 2)) * 4;
                let dst_idx = (y * target_w + x) * 4;
                downscaled[dst_idx..dst_idx + 4].copy_from_slice(&raw_buffer[src_idx..src_idx + 4]);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(ref p) = mac_plat {
            let _ = p.encode_jpeg(&downscaled, target_w as u32, target_h as u32, 0.70);
        }
        downscale_jpeg_times_ms.push(start.elapsed().as_micros() as f64 / 1000.0);
    }
    downscale_jpeg_times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let down_p50 = percentile(&downscale_jpeg_times_ms, 0.50);
    let down_p95 = percentile(&downscale_jpeg_times_ms, 0.95);
    let down_p99 = percentile(&downscale_jpeg_times_ms, 0.99);
    let down_pass = down_p95 < 15.0;
    println!(
        "  [G3.3] Downscale + JPEG encode: p50={:.2}ms, p95={:.2}ms, p99={:.2}ms (Target: < 15ms) -> {}",
        down_p50,
        down_p95,
        down_p99,
        if down_pass { "PASS" } else { "FAIL" }
    );
    assert!(
        down_pass,
        "Downscale + JPEG encode exceeded 15ms p95 budget"
    );

    // Measure dispatcher overhead without clicking an arbitrary real application.
    // Actual input delivery/latency requires a controlled native fixture.
    let action = ComputerAction::Wait {
        condition: None,
        timeout_ms: 0,
    };
    // Warmup dispatch
    let _ = platform.act(&action).await;

    let mut act_times_us = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = Instant::now();
        platform.act(&action).await?;
        act_times_us.push(start.elapsed().as_micros() as f64);
    }
    act_times_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let act_p50_ms = percentile(&act_times_us, 0.50) / 1000.0;
    let act_p95_ms = percentile(&act_times_us, 0.95) / 1000.0;
    let act_p99_ms = percentile(&act_times_us, 0.99) / 1000.0;
    let act_budget = if cfg!(debug_assertions) { 7.0 } else { 5.0 };
    let act_pass = act_p99_ms < act_budget;
    println!(
        "  [G3.4] Wait(0) dispatcher overhead (input latency unmeasured): p50={:.3}ms, p95={:.3}ms, p99={:.3}ms (Target: < {:.1}ms{}) -> {}",
        act_p50_ms,
        act_p95_ms,
        act_p99_ms,
        act_budget,
        if cfg!(debug_assertions) {
            " [debug]"
        } else {
            ""
        },
        if act_pass { "PASS" } else { "FAIL" }
    );
    assert!(
        act_pass,
        "Action dispatch exceeded {act_budget}ms p99 budget"
    );

    println!(
        "==> Native capture/AX/JPEG and dispatcher checks passed. Actual input latency remains unmeasured."
    );
    Ok((frame_p99, ax_p95_ms, down_p95, act_p99_ms))
}

async fn run_bench_rss() -> Result<(f64, f64), Box<dyn std::error::Error>> {
    println!("==> Benchmarking G2 process tree RSS & Spike S2 UI evaluation...");
    let build_status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "gm-daemon",
            "-p",
            "gm-cu-helper",
            "-p",
            "gm-ui",
        ])
        .status()?;
    if !build_status.success() {
        return Err("Failed to build release binaries for RSS benchmark".into());
    }

    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let folder = std::env::temp_dir().join(format!("gm-rss-{nonce}"));
    gm_core::storage::ensure_private_dir(&folder)?;
    let mut config = gm_core::config::GmConfig {
        storage_dir: folder.join("data"),
        ..Default::default()
    };
    config.openbot.bots_dir = config.storage_dir.join("bots");
    config.openbot.companion_enabled = false;
    #[cfg(unix)]
    {
        config.socket_path = folder.join("helper.sock");
        config.daemon_socket_path = folder.join("daemon.sock");
    }
    #[cfg(windows)]
    {
        config.socket_path = PathBuf::from(format!(r"\\.\pipe\gm-rss-helper-{nonce}"));
        config.daemon_socket_path = PathBuf::from(format!(r"\\.\pipe\gm-rss-daemon-{nonce}"));
    }
    let config_path = folder.join("config.json");
    config.save(&config_path)?;
    let mut children = vec![
        Owned(
            Command::new("target/release/gm-cu-helper")
                .arg("--socket")
                .arg(&config.socket_path)
                .spawn()?,
        ),
        Owned(
            Command::new("target/release/gm-daemon")
                .arg("--config")
                .arg(&config_path)
                .arg("daemon")
                .spawn()?,
        ),
    ];
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for child in &mut children {
        if child.0.try_wait()?.is_some() {
            return Err("A benchmark process exited before measurement".into());
        }
    }
    let pids: Vec<u32> = children.iter().map(|c| c.0.id()).collect();
    let (idle_mb, idle_metric) = measure_process_tree_memory(&pids)?;
    println!("Core idle (daemon + helper, UI closed): {idle_mb:.2} MiB ({idle_metric})");
    children.push(Owned(
        Command::new("target/release/gm-ui")
            .arg("--config")
            .arg(&config_path)
            .arg("native")
            .spawn()?,
    ));
    let mut samples = vec![];
    for _ in 0..5 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        for child in &mut children {
            if child.0.try_wait()?.is_some() {
                return Err("Native UI or runtime exited before measurement".into());
            }
        }
        let pids: Vec<u32> = children.iter().map(|c| c.0.id()).collect();
        samples.push(measure_process_tree_memory(&pids)?.0);
    }
    samples.sort_by(f64::total_cmp);
    let open_mb = samples[samples.len() / 2];
    println!("Native UI open (median of 5 samples): {open_mb:.2} MiB");
    drop(children);
    let _ = std::fs::remove_dir_all(&folder);
    if idle_mb >= 50.0 || open_mb >= 120.0 {
        return Err(format!(
            "Memory target exceeded: core={idle_mb:.2} MiB, UI-open={open_mb:.2} MiB"
        )
        .into());
    }
    Ok((idle_mb, open_mb))
}

async fn run_eval() -> Result<gm_eval::EvalSummaryReport, Box<dyn std::error::Error>> {
    println!(
        "==> Running offline G5 fixtures (not native desktop or provider quality certification)..."
    );
    let runner = EvalRunner::default();
    let (results, summary) = runner.run_all().await;
    for r in &results {
        let status_str = if r.success { "PASS" } else { "FAIL" };
        println!(
            "  [{status_str}] {} - {}: {}",
            r.task_id, r.task_name, r.details
        );
    }
    println!("\nSummary:");
    println!("  Total Tasks: {}", summary.total_tasks);
    println!("  Passed: {}", summary.passed_tasks);
    println!(
        "  Success Rate: {:.1}% (Target >= 70%)",
        summary.success_rate_pct
    );
    println!(
        "  Safety Violations: {} (Target: 0)",
        summary.total_safety_violations
    );

    println!(
        "\n==> Running v0.2.0 Capability Evaluation Suite (SoM, Stagnation, Approvals, MCP, AX fixtures)..."
    );
    let v2_results = gm_eval::run_v2_capability_evals().await;
    let mut v2_all_passed = true;
    for r in &v2_results {
        if !r.success || r.safety_violation {
            v2_all_passed = false;
        }
        let status_str = if r.success && !r.safety_violation {
            "PASS"
        } else {
            "FAIL"
        };
        println!(
            "  [{status_str}] {} - {}: {}",
            r.task_id, r.task_name, r.details
        );
    }

    println!(
        "\n==> Running v0.3.0 OpenBot Capability Evaluation Suite (Queue, Seatbelt, Wheel, A2A, AG-UI)..."
    );
    let openbot_results = gm_eval::run_openbot_capability_evals().await;
    let mut openbot_all_passed = true;
    for r in &openbot_results {
        if !r.success || r.safety_violation {
            openbot_all_passed = false;
        }
        let status_str = if r.success && !r.safety_violation {
            "PASS"
        } else {
            "FAIL"
        };
        println!(
            "  [{status_str}] {} - {}: {}",
            r.task_id, r.task_name, r.details
        );
    }

    if summary.meets_g5_quality_gate && v2_all_passed && openbot_all_passed {
        println!(
            "==> Offline G5 + v0.2.0 + v0.3.0 fixture checks: PASSED ({}/{} G5 + {}/{} v0.2.0 + {}/{} OpenBot)",
            summary.passed_tasks,
            summary.total_tasks,
            v2_results.iter().filter(|r| r.success).count(),
            v2_results.len(),
            openbot_results.iter().filter(|r| r.success).count(),
            openbot_results.len()
        );
        Ok(summary)
    } else {
        eprintln!("==> G5 / v0.2.0 / v0.3.0 Quality Gate: FAILED");
        Err("G5, v0.2.0, or v0.3.0 OpenBot Quality Gate failed".into())
    }
}

fn run_simulate_dogfood(
    days: u32,
    output_path: &str,
) -> Result<DogfoodMetricsSummary, Box<dyn std::error::Error>> {
    println!("==> Simulating {days}-Day Dogfood Cycle & Generating G6 Fixture Metrics...");

    let out_file = Path::new(output_path);
    if out_file.exists() {
        let _ = fs::remove_file(out_file);
    }

    let tracker = DogfoodTracker::with_file(output_path, 1.0)?;
    let today = Utc::now().date_naive();
    let base_day = today - chrono::Duration::days((days - 1) as i64);

    let calendar_fixtures = [
        "Upcoming: Engineering Standup & Sprint Sync",
        "Upcoming: 1-on-1 with Alex (Design lead)",
        "Upcoming: Architecture Review: Local VLM / Metal",
        "Upcoming: Customer Feedback Debrief",
        "Upcoming: Weekly Security & Permissions Audit",
        "Upcoming: Grok-Muse Release Planning Session",
        "Upcoming: Product Strategy & Roadmap Review",
    ];

    let reminder_fixtures = [
        "Reminder: Follow up on pull request review comments",
        "Reminder: Check whole-tree memory footprint benchmarks",
        "Reminder: Renew Apple Developer signing certificate",
        "Reminder: Verify dogfood spend metrics on staging",
        "Reminder: Test offline visual triage with Candle",
        "Reminder: Update quiet gate opt-out test fixtures",
        "Reminder: Review weekly proactive notice precision",
    ];

    let mail_fixtures = [
        (
            "Important email from Sarah (VP Engineering)",
            "mail_sarah_01",
        ),
        (
            "Security notification: New Keychain access prompt",
            "mail_sec_02",
        ),
        ("Customer support ticket #1042 escalated", "mail_sup_03"),
        ("Weekly build digest & test battery report", "mail_ci_04"),
        (
            "Flight itinerary & conference ticket confirmation",
            "mail_travel_05",
        ),
        ("Newsletter: Tech updates for Apple Silicon", "mail_news_06"),
    ];

    for day_idx in 0..days {
        let day_num = day_idx + 1;
        let day_date = base_day + chrono::Duration::days(day_idx as i64);
        let day_time = day_date.and_hms_opt(0, 0, 0).unwrap().and_utc();
        let date_str = day_date.format("%Y-%m-%d").to_string();

        let mut daily_cost = 0.0;

        // 1. Calendar Patrol notice (delivered once per day)
        let cal_title = calendar_fixtures[(day_idx as usize) % calendar_fixtures.len()];
        let cal_id = format!("notif_cal_d{day_num}");
        let cal_cost = 0.024;
        tracker.record_notice_delivered(
            &cal_id,
            "calendar",
            cal_title,
            false,
            &format!("hash_cal_{day_num}"),
            cal_cost,
            day_time + chrono::Duration::hours(9),
        )?;
        tracker.record_feedback(
            &cal_id,
            "calendar",
            "done",
            Some(true),
            day_time + chrono::Duration::hours(10),
        )?;
        tracker.record_rating(&cal_id, true, day_time + chrono::Duration::hours(10))?;
        daily_cost += cal_cost;

        // 2. Reminder follow-up notice (delivered on weekdays)
        if day_num % 7 != 0 && day_num % 7 != 6 {
            let rem_title = reminder_fixtures[(day_idx as usize) % reminder_fixtures.len()];
            let rem_id = format!("notif_rem_d{day_num}");
            let rem_cost = 0.031;
            let is_later = day_num % 4 == 0;
            let action = if is_later { "later" } else { "done" };
            tracker.record_notice_delivered(
                &rem_id,
                "reminders",
                rem_title,
                false,
                &format!("hash_rem_{day_num}"),
                rem_cost,
                day_time + chrono::Duration::hours(14),
            )?;
            tracker.record_feedback(
                &rem_id,
                "reminders",
                action,
                Some(true),
                day_time + chrono::Duration::hours(15),
            )?;
            tracker.record_rating(&rem_id, true, day_time + chrono::Duration::hours(15))?;
            daily_cost += rem_cost;
        }

        // 3. Mail patrol notice (delivered every 2-3 days)
        if day_num % 3 == 0 || day_num == 14 {
            let (mail_title, mail_hash) = mail_fixtures[(day_idx as usize) % mail_fixtures.len()];
            let mail_id = format!("notif_mail_d{day_num}");
            let mail_cost = 0.042;
            let is_stop = day_num == 6; // Single opt-out / stop feedback to verify stop handling
            let is_useful = !is_stop && day_num != 12; // High useful rate (~93%)
            let action = if is_stop { "stop" } else { "done" };

            tracker.record_notice_delivered(
                &mail_id,
                "mail",
                mail_title,
                false,
                mail_hash,
                mail_cost,
                day_time + chrono::Duration::hours(16),
            )?;
            tracker.record_feedback(
                &mail_id,
                "mail",
                action,
                Some(is_useful),
                day_time + chrono::Duration::hours(17),
            )?;
            tracker.record_rating(&mail_id, is_useful, day_time + chrono::Duration::hours(17))?;
            daily_cost += mail_cost;
        }

        // Record daily spend
        tracker.record_daily_spend(
            &date_str,
            daily_cost,
            1.0,
            day_time + chrono::Duration::hours(23),
        )?;
    }

    let summary = tracker.compute_metrics();

    // Verify G6 certification criteria
    assert!(
        summary.active_days_count >= days,
        "Active days count {} < {}",
        summary.active_days_count,
        days
    );
    assert!(
        summary.avg_notices_per_day <= 3.0,
        "Average notices/day {:.2} exceeded 3.0",
        summary.avg_notices_per_day
    );
    assert!(
        summary.interruption_precision_pct >= 80.0,
        "Interruption precision {:.1}% fell below 80.0%",
        summary.interruption_precision_pct
    );
    assert!(
        summary.spend_cap_enforced,
        "Daily spend cap was not enforced: max daily ${:.3}",
        summary.max_daily_spend_usd
    );
    assert!(
        summary.g6_gate_passed,
        "G6 dogfood certification gate failed"
    );

    // Also persist copy to standard application support folder if parent exists
    let fallback_dir = dirs_data_dir();
    let app_support_file = fallback_dir.join("dogfood_metrics.jsonl");
    if fs::create_dir_all(&fallback_dir).is_ok() {
        let _ = fs::copy(output_path, &app_support_file);
    }

    println!("\n╔═════════════════════════════════════════════════════════════════════════════╗");
    println!("║        Grok-Muse 14-Day Dogfood Simulation & G6 Fixture Report         ║");
    println!("╚═════════════════════════════════════════════════════════════════════════════╝");
    println!("  Output File                 : {output_path}");
    println!(
        "  Active Days Simulated       : {} (Target: >= {days})",
        summary.active_days_count
    );
    println!(
        "  Total Notices Delivered     : {}",
        summary.total_notices_delivered
    );
    println!(
        "  Average Notices / Day       : {:.2} (Target: <= 3.0 / day)",
        summary.avg_notices_per_day
    );
    println!(
        "  Total Rated Notices         : {}",
        summary.total_rated_notices
    );
    println!(
        "  Useful Notices Count        : {}",
        summary.useful_ratings_count
    );
    println!(
        "  Not Useful Notices Count    : {}",
        summary.not_useful_ratings_count
    );
    println!(
        "  Interruption Precision      : {:.1}% (Target: >= 80.0%)",
        summary.interruption_precision_pct
    );
    println!(
        "  User Feedback Actions       : done={}, later={}, stop={}",
        summary.feedback_done_count, summary.feedback_later_count, summary.feedback_stop_count
    );
    println!(
        "  Total Proactive Spend       : ${:.3} USD",
        summary.total_spend_usd
    );
    println!(
        "  Peak Daily Spend            : ${:.3} USD (Daily Cap: ${:.2})",
        summary.max_daily_spend_usd, summary.daily_spend_cap_usd
    );
    println!(
        "  Daily Spend Cap Enforced    : {} (Target: true)",
        summary.spend_cap_enforced
    );
    println!("  Topic Counts                : {:?}", summary.topic_counts);
    println!(
        "  G6 Proactive Dogfood Gate   : PASSED (simulation only; real usefulness is unverified)"
    );
    println!("═══════════════════════════════════════════════════════════════════════════════\n");

    Ok(summary)
}

fn dirs_data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("GROK_MUSE_DATA_DIR") {
        PathBuf::from(custom)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join("Library/Application Support/GrokMuse")
    } else {
        PathBuf::from("target/data")
    }
}

async fn run_test_all(identity: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let start_all = Instant::now();
    println!("\n===============================================================================");
    println!("  Grok-Muse Automated Release Check Suite (xtask test-all)");
    println!("===============================================================================\n");

    let mut scorecard: Vec<(
        &'static str,
        &'static str,
        &'static str,
        String,
        std::time::Duration,
        bool,
    )> = Vec::new();

    // 1. Formatting check (cargo fmt --all -- --check)
    println!("--> [Gate 1/8] Checking code formatting (cargo fmt --all -- --check)...");
    let t0 = Instant::now();
    let fmt_status = Command::new("cargo")
        .args(["fmt", "--all", "--", "--check"])
        .status()?;
    let d_fmt = t0.elapsed();
    let fmt_pass = fmt_status.success();
    scorecard.push((
        "FMT",
        "Code Formatting",
        "cargo fmt --check",
        if fmt_pass {
            "Clean (0 diffs)".into()
        } else {
            "Diffs detected".into()
        },
        d_fmt,
        fmt_pass,
    ));
    if !fmt_pass {
        eprintln!("  [Gate 1] FAIL: cargo fmt check failed");
    } else {
        println!(
            "  [Gate 1] PASS: Code formatting verified clean ({:.2}s)",
            d_fmt.as_secs_f64()
        );
    }

    // 2. Linter check (cargo clippy --workspace --all-targets --all-features -- -D warnings)
    println!(
        "\n--> [Gate 2/8] Running workspace linter (cargo clippy --all-features -- -D warnings)..."
    );
    let t0 = Instant::now();
    let clippy_status = Command::new("cargo")
        .args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ])
        .status()?;
    let d_clippy = t0.elapsed();
    let clippy_pass = clippy_status.success();
    scorecard.push((
        "CLIPPY",
        "Workspace Linter",
        "-D warnings (0 warnings)",
        if clippy_pass {
            "0 warnings".into()
        } else {
            "Warnings found".into()
        },
        d_clippy,
        clippy_pass,
    ));
    if !clippy_pass {
        eprintln!("  [Gate 2] FAIL: cargo clippy check failed");
    } else {
        println!(
            "  [Gate 2] PASS: Workspace clippy verified clean ({:.2}s)",
            d_clippy.as_secs_f64()
        );
    }

    // 3. Unit tests (cargo test --workspace --all-features)
    println!(
        "\n--> [Gate 3/8] Running unit test battery (cargo test --workspace --all-features)..."
    );
    let t0 = Instant::now();
    let test_status = Command::new("cargo")
        .args(["test", "--workspace", "--all-features"])
        .status()?;
    let d_test = t0.elapsed();
    let test_pass = test_status.success();
    scorecard.push((
        "UNIT",
        "Unit Test Battery",
        "100% test pass (all crates)",
        if test_pass {
            "All tests passed".into()
        } else {
            "Failures detected".into()
        },
        d_test,
        test_pass,
    ));
    if !test_pass {
        eprintln!("  [Gate 3] FAIL: Unit tests failed");
    } else {
        println!(
            "  [Gate 3] PASS: All unit tests passed ({:.2}s)",
            d_test.as_secs_f64()
        );
    }

    // 4. G3 Local hotpath benchmark
    println!("\n--> [Gate 4/8] Benchmarking G3 local hot-path budgets...");
    let t0 = Instant::now();
    let hotpath_res = run_bench_hotpath().await;
    let d_hotpath = t0.elapsed();
    let hotpath_pass = hotpath_res.is_ok();
    let hotpath_metric = match hotpath_res {
        Ok((f_p99, ax_p95, down_p95, act_p99)) => format!(
            "SCK={:.1}µs, AX={:.1}ms, JPEG={:.1}ms, Act={:.2}ms",
            f_p99, ax_p95, down_p95, act_p99
        ),
        Err(e) => format!("Failed: {e}"),
    };
    scorecard.push((
        "G3",
        "G3 Local Hotpath",
        "SCK<2ms, AX<30ms, JPEG<15ms, Act<5ms",
        hotpath_metric,
        d_hotpath,
        hotpath_pass,
    ));

    // 5. G2 Whole-tree RSS footprint
    println!("\n--> [Gate 5/8] Benchmarking G2 whole-tree RSS footprint...");
    let t0 = Instant::now();
    let rss_res = run_bench_rss().await;
    let d_rss = t0.elapsed();
    let rss_pass = rss_res.is_ok();
    let rss_metric = match rss_res {
        Ok((idle_mb, open_mb)) => format!("Idle={:.1}MB, Open={:.1}MB", idle_mb, open_mb),
        Err(e) => format!("Failed: {e}"),
    };
    scorecard.push((
        "G2",
        "G2 Whole-Tree RSS",
        "Idle < 50MB, UI Open < 120MB",
        rss_metric,
        d_rss,
        rss_pass,
    ));

    // 6. G4 LLM latency calibration
    println!("\n--> [Gate 6/8] Calibrating G4 LLM agent step latency...");
    let t0 = Instant::now();
    let llm_res = run_bench_llm(false, 15).await;
    let d_llm = t0.elapsed();
    let llm_pass = llm_res.is_ok();
    let llm_metric = match llm_res {
        Ok((p50, p95)) => format!("Median={:.1}ms, p95={:.1}ms", p50, p95),
        Err(e) => format!("Failed: {e}"),
    };
    scorecard.push((
        "G4",
        "G4 Agent Step Latency",
        "Median < 1.5s, p95 < 4.0s",
        llm_metric,
        d_llm,
        llm_pass,
    ));

    // 7. G5 25-task macOS evaluation suite
    println!("\n--> [Gate 7/8] Running G5 25-task macOS evaluation suite...");
    let t0 = Instant::now();
    let eval_res = run_eval().await;
    let d_eval = t0.elapsed();
    let eval_pass = eval_res
        .as_ref()
        .map(|s| s.meets_g5_quality_gate)
        .unwrap_or(false);
    let eval_metric = match eval_res {
        Ok(summary) => format!(
            "{}/{} passed ({:.1}%), 0 violations",
            summary.passed_tasks, summary.total_tasks, summary.success_rate_pct
        ),
        Err(e) => format!("Failed: {e}"),
    };
    scorecard.push((
        "G5",
        "G5 25-Task macOS Eval",
        "Success >= 70%, 0 safety violations",
        eval_metric,
        d_eval,
        eval_pass,
    ));

    // 8. G1 DMG build & verification
    println!("\n--> [Gate 8/8] Building and verifying distributable macOS DMG (G1 Gate)...");
    let t0 = Instant::now();
    let id = identity.unwrap_or_else(detect_identity);
    let dmg_output = "target/dist/Grok-Muse.dmg";
    let dmg_build_res = assemble_dmg(&id, dmg_output);
    let dmg_verify_res = if dmg_build_res.is_ok() {
        run_verify_dmg(dmg_output)
    } else {
        Err("DMG build failed".into())
    };
    let d_dmg = t0.elapsed();
    let dmg_pass = dmg_verify_res.is_ok();
    let dmg_metric = if dmg_pass {
        "Strict codesign, hdiutil APFS verify, 0 Electron".into()
    } else {
        format!("Failed: {:?}", dmg_verify_res.err())
    };
    scorecard.push((
        "G1",
        "G1 Distribution DMG",
        "Strict codesign, hdiutil verify, 0 Electron",
        dmg_metric,
        d_dmg,
        dmg_pass,
    ));

    let total_duration = start_all.elapsed();

    // Print Formatted Scorecard
    println!(
        "\n╔═══════════════════════════════════════════════════════════════════════════════════════════════════════════╗"
    );
    println!(
        "║                      GROK-MUSE RELEASE GATE CERTIFICATION SCORECARD                                       ║"
    );
    println!(
        "╚═══════════════════════════════════════════════════════════════════════════════════════════════════════════╝"
    );
    println!(
        "  {:<6} | {:<24} | {:<32} | {:<22} | {:<8} | {:<6}",
        "Gate", "Verification Gate", "Target Criteria", "Measured Result", "Duration", "Status"
    );
    println!(
        "  -------+--------------------------+----------------------------------+------------------------+----------+-------"
    );

    let mut all_passed = true;
    for (id, name, target, measured, duration, passed) in &scorecard {
        if !*passed {
            all_passed = false;
        }
        let status_str = if *passed { "PASS" } else { "FAIL" };
        let measured_trimmed = if measured.len() > 22 {
            &measured[..22]
        } else {
            measured
        };
        println!(
            "  {:<6} | {:<24} | {:<32} | {:<22} | {:>6.2}s  |  {:<4}",
            id,
            name,
            target,
            measured_trimmed,
            duration.as_secs_f64(),
            status_str
        );
    }
    println!(
        "  -------+--------------------------+----------------------------------+------------------------+----------+-------"
    );
    if all_passed {
        println!(
            "  Overall Release Decision   : AUTOMATED CHECKS PASSED; native acceptance, real-model quality, VM boot and platform signing remain separate checks"
        );
    } else {
        println!("  Overall Release Decision   : RELEASE BLOCKED (ONE OR MORE GATES FAILED)");
    }
    println!(
        "  Total Verification Time    : {:.2}s",
        total_duration.as_secs_f64()
    );
    println!(
        "═════════════════════════════════════════════════════════════════════════════════════════════════════════════\n"
    );

    if !all_passed {
        return Err("One or more release certification gates failed".into());
    }

    Ok(())
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.as_ref().join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(entry.path(), &dest_path)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(entry.path())?;
            if dest_path.exists() || fs::symlink_metadata(&dest_path).is_ok() {
                let _ = fs::remove_file(&dest_path);
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(link_target, &dest_path)?;
            }
            #[cfg(windows)]
            {
                let _ = std::os::windows::fs::symlink_file(link_target, &dest_path);
            }
        } else {
            fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}

fn create_symlink(target: &Path, link_path: &Path) -> std::io::Result<()> {
    if link_path.exists() || fs::symlink_metadata(link_path).is_ok() {
        let _ = fs::remove_file(link_path);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link_path)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link_path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Symlinks not supported on this OS",
        ))
    }
}

fn run_install(
    dest_dir: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
    identity: Option<String>,
    no_sign: bool,
    release: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(workspace_root) = manifest_dir.parent() {
        let _ = std::env::set_current_dir(workspace_root);
    }

    println!("\n===============================================================================");
    println!("  Grok-Muse System Installation (xtask install)");
    println!("===============================================================================\n");

    // 1. Compile & assemble app bundles
    create_app_bundles(release)?;

    // 2. Code sign bundles unless explicitly skipped
    if !no_sign {
        let id = identity.unwrap_or_else(detect_identity);
        println!("==> Signing app bundles with identity '{id}'...");
        sign_bundle(
            "target/bundle/GrokMuseCUHelper.app",
            &id,
            Some("entitlements.plist"),
        )?;
        sign_bundle(
            "target/bundle/GrokMuse.app",
            &id,
            Some("entitlements.plist"),
        )?;
    } else {
        println!("  [Install] Skipping code signing as requested by --no-sign flag");
    }

    // 3. Determine target application installation directory
    let target_app_dir = if let Some(dir) = dest_dir {
        dir
    } else {
        // Probe /Applications write access
        let probe = Path::new("/Applications/.gm_probe");
        if fs::write(probe, b"probe").is_ok() {
            let _ = fs::remove_file(probe);
            PathBuf::from("/Applications")
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join("Applications")
        }
    };

    println!("==> Target application directory: {:?}", target_app_dir);
    fs::create_dir_all(&target_app_dir)?;

    // 4. Install GrokMuse.app and GrokMuseCUHelper.app
    for app_name in ["GrokMuse.app", "GrokMuseCUHelper.app"] {
        let src = Path::new("target/bundle").join(app_name);
        let dest = target_app_dir.join(app_name);

        println!("  Installing {} -> {:?}...", app_name, dest);
        if dest.exists() {
            let _ = fs::remove_dir_all(&dest);
        }

        copy_dir_all(&src, &dest)?;

        // Verify codesign of installed app
        #[cfg(target_os = "macos")]
        if !no_sign {
            let _ = Command::new("codesign")
                .args(["--verify", "--deep", "--strict", dest.to_str().unwrap()])
                .status();
        }
    }

    // 5. Create CLI symlinks
    let target_bin_dir = if let Some(dir) = bin_dir {
        dir
    } else {
        let probe = Path::new("/usr/local/bin/.gm_probe");
        if fs::write(probe, b"probe").is_ok() {
            let _ = fs::remove_file(probe);
            PathBuf::from("/usr/local/bin")
        } else {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/bin")
        }
    };

    println!("==> Target CLI binary directory: {:?}", target_bin_dir);
    fs::create_dir_all(&target_bin_dir)?;

    let symlinks = [
        ("gm", target_app_dir.join("GrokMuse.app/Contents/MacOS/gm")),
        (
            "gm-daemon",
            target_app_dir.join("GrokMuse.app/Contents/MacOS/gm-daemon"),
        ),
        (
            "gm-cu-helper",
            target_app_dir.join("GrokMuseCUHelper.app/Contents/MacOS/gm-cu-helper"),
        ),
    ];

    for (link_name, target_file) in symlinks {
        let link_path = target_bin_dir.join(link_name);
        println!("  Symlinking {:?} -> {:?}", link_path, target_file);
        if let Err(e) = create_symlink(&target_file, &link_path) {
            eprintln!(
                "  Warning: failed to create symlink at {:?}: {}",
                link_path, e
            );
        }
    }

    println!("\n╔═════════════════════════════════════════════════════════════════════════════╗");
    println!("║                   GROK-MUSE INSTALLATION COMPLETE                           ║");
    println!("╚═════════════════════════════════════════════════════════════════════════════╝");
    println!("  Applications installed to:");
    println!("    • {}/GrokMuse.app", target_app_dir.display());
    println!("    • {}/GrokMuseCUHelper.app", target_app_dir.display());
    println!("  CLI symlinks created in:");
    println!("    • {}/gm", target_bin_dir.display());
    println!("    • {}/gm-daemon", target_bin_dir.display());
    println!("    • {}/gm-cu-helper", target_bin_dir.display());
    println!();
    println!("  Next Steps:");
    println!("    1. Check and grant required macOS system permissions:");
    println!("       gm permissions --open-missing");
    println!("    2. Register LaunchAgent background services:");
    println!("       gm service install");
    println!("    3. Launch the desktop tray application:");
    println!("       open \"{}/GrokMuse.app\"", target_app_dir.display());
    println!("═══════════════════════════════════════════════════════════════════════════════\n");

    Ok(())
}

fn build_notarytool_args(
    dmg_path: &str,
    keychain_profile: Option<&str>,
    apple_id: Option<&str>,
    team_id: Option<&str>,
    password: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut args = vec![
        "notarytool".to_string(),
        "submit".to_string(),
        dmg_path.to_string(),
        "--wait".to_string(),
    ];

    if let Some(profile) = keychain_profile.filter(|s| !s.trim().is_empty()) {
        args.push("--keychain-profile".into());
        args.push(profile.to_string());
        return Ok(args);
    }

    let resolved_id = apple_id.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        "Missing notarization credentials: provide --keychain-profile or --apple-id / APPLE_ID"
            .to_string()
    })?;
    let resolved_team = team_id.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        "Missing Apple Developer Team ID: provide --team-id or APPLE_TEAM_ID".to_string()
    })?;
    let resolved_pw = password.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        "Missing app-specific password in configured environment variable".to_string()
    })?;

    args.push("--apple-id".into());
    args.push(resolved_id.to_string());
    args.push("--team-id".into());
    args.push(resolved_team.to_string());
    args.push("--password".into());
    args.push(resolved_pw.to_string());
    Ok(args)
}

fn run_notarize(
    dmg_path: &str,
    keychain_profile: Option<String>,
    apple_id: Option<String>,
    team_id: Option<String>,
    password_env: &str,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let dmg = Path::new(dmg_path);
    if !dmg.exists() {
        return Err(format!(
            "DMG file does not exist at '{dmg_path}'. Run `cargo run -p xtask -- dmg` first."
        )
        .into());
    }

    let profile = keychain_profile.or_else(|| std::env::var("APPLE_NOTARY_PROFILE").ok());
    let id = apple_id.or_else(|| std::env::var("APPLE_ID").ok());
    let team = team_id.or_else(|| std::env::var("APPLE_TEAM_ID").ok());
    let pw = std::env::var(password_env).ok();

    let notary_args = build_notarytool_args(
        dmg_path,
        profile.as_deref(),
        id.as_deref(),
        team.as_deref(),
        pw.as_deref(),
    )?;

    if dry_run {
        println!(
            "==> [Dry-Run] Validated notarization & stapling pipeline for '{dmg_path}' ({} args).",
            notary_args.len()
        );
        return Ok(());
    }

    println!(
        "==> Submitting {dmg_path} to Apple Notary Service (xcrun notarytool submit --wait)..."
    );
    let submit_status = Command::new("xcrun").args(&notary_args).status()?;
    if !submit_status.success() {
        return Err("xcrun notarytool submit failed".into());
    }

    println!("==> Stapling notarization ticket to {dmg_path} (xcrun stapler staple)...");
    let staple_status = Command::new("xcrun")
        .args(["stapler", "staple", dmg_path])
        .status()?;
    if !staple_status.success() {
        return Err("xcrun stapler staple failed".into());
    }

    println!("==> Validating stapled ticket (xcrun stapler validate)...");
    let validate_status = Command::new("xcrun")
        .args(["stapler", "validate", dmg_path])
        .status()?;
    if !validate_status.success() {
        return Err("xcrun stapler validate failed".into());
    }

    println!("==> Notarization and ticket stapling succeeded for {dmg_path}!");
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_percentile_underflow_and_boundaries() {
        assert_eq!(percentile(&[], 0.5), 0.0);

        let data = vec![10.0];
        assert_eq!(percentile(&data, 0.0), 10.0);
        assert_eq!(percentile(&data, 0.5), 10.0);
        assert_eq!(percentile(&data, 1.0), 10.0);

        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&values, 0.0), 1.0);
        assert_eq!(percentile(&values, 0.5), 3.0);
        assert_eq!(percentile(&values, 1.0), 5.0);
    }

    #[test]
    fn test_extract_mount_point_formats() {
        let multiline_plist = r#"
        <dict>
            <key>mount-point</key>
            <string>/Volumes/Grok-Muse</string>
        </dict>
        "#;
        assert_eq!(
            extract_mount_point(multiline_plist),
            Some(PathBuf::from("/Volumes/Grok-Muse"))
        );

        let inline_plist = "<key>mount-point</key><string>/Volumes/TestVolume 1</string>";
        assert_eq!(
            extract_mount_point(inline_plist),
            Some(PathBuf::from("/Volumes/TestVolume 1"))
        );

        let missing_plist = "<dict><key>dev-entry</key><string>/dev/disk4s1</string></dict>";
        assert_eq!(extract_mount_point(missing_plist), None);
    }

    #[test]
    fn test_simulate_dogfood_execution() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_dogfood_metrics.jsonl");
        let path_str = path.to_string_lossy().to_string();

        let temp_data = temp_dir.join("test_gm_data");
        let _ = fs::create_dir_all(&temp_data);
        unsafe {
            std::env::set_var("GROK_MUSE_DATA_DIR", temp_data.to_str().unwrap());
        }

        let summary = run_simulate_dogfood(14, &path_str).expect("simulation should succeed");
        assert_eq!(summary.active_days_count, 14);
        assert!(summary.avg_notices_per_day <= 3.0);
        assert!(summary.interruption_precision_pct >= 80.0);
        assert!(summary.spend_cap_enforced);
        assert!(summary.g6_gate_passed);
        assert!(path.exists());
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir_all(temp_data);
    }

    #[cfg(unix)]
    #[test]
    fn test_install_command_execution() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        if let Some(workspace_root) = manifest_dir.parent() {
            let _ = std::env::set_current_dir(workspace_root);
        }

        let temp_dir = std::env::temp_dir();
        let test_apps = temp_dir.join("test_gm_install_apps");

        let test_bin = temp_dir.join("test_gm_install_bin");
        let _ = fs::create_dir_all(&test_apps);
        let _ = fs::create_dir_all(&test_bin);

        let res = run_install(
            Some(test_apps.clone()),
            Some(test_bin.clone()),
            None,
            true,  // no_sign for fast offline test
            false, // debug
        );
        assert!(res.is_ok(), "run_install failed: {:?}", res.err());

        assert!(test_apps.join("GrokMuse.app/Contents/MacOS/gm-ui").exists());
        assert!(
            test_apps
                .join("GrokMuse.app/Contents/MacOS/gm-daemon")
                .exists()
        );
        assert!(test_apps.join("GrokMuse.app/Contents/MacOS/gm").exists());
        assert!(
            test_apps
                .join("GrokMuseCUHelper.app/Contents/MacOS/gm-cu-helper")
                .exists()
        );

        // Verify symlinks created
        assert!(test_bin.join("gm").exists());
        assert!(test_bin.join("gm-daemon").exists());
        assert!(test_bin.join("gm-cu-helper").exists());

        let _ = fs::remove_dir_all(test_apps);
        let _ = fs::remove_dir_all(test_bin);
    }

    #[test]
    fn test_notarize_dry_run_and_command_builder() {
        let profile_args =
            build_notarytool_args("Grok-Muse.dmg", Some("gm-notary"), None, None, None).unwrap();
        assert_eq!(
            profile_args,
            vec![
                "notarytool",
                "submit",
                "Grok-Muse.dmg",
                "--wait",
                "--keychain-profile",
                "gm-notary"
            ]
        );

        let apple_id_args = build_notarytool_args(
            "Grok-Muse.dmg",
            None,
            Some("dev@example.com"),
            Some("TEAM123456"),
            Some("app-pw"),
        )
        .unwrap();
        assert!(apple_id_args.contains(&"--team-id".to_string()));
        assert!(build_notarytool_args("Grok-Muse.dmg", None, None, None, None).is_err());

        let tmp_dmg = std::env::temp_dir().join("test_notarize_dry_run.dmg");
        fs::write(&tmp_dmg, b"dummy_dmg_header").unwrap();
        let res = run_notarize(
            tmp_dmg.to_str().unwrap(),
            Some("gm-notary-profile".into()),
            None,
            None,
            "UNUSED_PW_ENV",
            true,
        );
        assert!(res.is_ok());
        let _ = fs::remove_file(tmp_dmg);
    }
}
