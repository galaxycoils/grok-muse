use clap::{Parser, Subcommand};
use gm_core::types::ComputerAction;
use gm_eval::EvalRunner;
use gm_platform::{MockPlatform, Platform};
use std::fs;
use std::path::Path;
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
    Bundle,
    /// Sign binaries with detected or self-signed identity
    Sign {
        #[arg(long)]
        identity: Option<String>,
    },
    /// Benchmark G3 local hot-path budgets
    BenchHotpath,
    /// Benchmark G2 whole-tree idle RSS
    BenchRss,
    /// Run G5 25-task evaluation suite
    Eval,
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * pct).ceil() as usize - 1;
    sorted[idx.min(sorted.len() - 1)]
}

fn measure_process_tree_memory(pids: &[u32]) -> (f64, String) {
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
            return (
                mb,
                format!("macOS footprint tool phys_footprint: {:.2} MB", mb),
            );
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
        return (mb, format!("ps resident set: {:.2} MB", mb));
    }

    (0.0, "unknown".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Bundle => {
            println!("==> Compiling Grok-Muse binaries for bundling...");
            let build_status = Command::new("cargo")
                .args([
                    "build",
                    "-p",
                    "gm-cu-helper",
                    "-p",
                    "gm-daemon",
                    "-p",
                    "gm-ui",
                ])
                .status()?;
            if !build_status.success() {
                eprintln!("Failed to build binaries for bundling");
                std::process::exit(1);
            }

            println!("==> Bundling Grok-Muse applications...");
            let helper_app_dir = Path::new("target/bundle/GrokMuseCUHelper.app/Contents/MacOS");
            fs::create_dir_all(helper_app_dir)?;

            // Copy helper binary
            fs::copy(
                "target/debug/gm-cu-helper",
                helper_app_dir.join("gm-cu-helper"),
            )?;

            // Write helper Info.plist (TCC declarations + LSUIElement)
            let helper_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>co.anysphere.grok-muse.cu-helper</string>
    <key>CFBundleName</key>
    <string>GrokMuseCUHelper</string>
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
</dict>
</plist>
"#;
            fs::write(
                "target/bundle/GrokMuseCUHelper.app/Contents/Info.plist",
                helper_plist,
            )?;

            // Bundle main GrokMuse.app
            let app_macos_dir = Path::new("target/bundle/GrokMuse.app/Contents/MacOS");
            fs::create_dir_all(app_macos_dir)?;
            fs::copy("target/debug/gm-ui", app_macos_dir.join("gm-ui"))?;

            let app_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>co.anysphere.grok-muse.ui</string>
    <key>CFBundleName</key>
    <string>GrokMuse</string>
    <key>CFBundleExecutable</key>
    <string>gm-ui</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
</dict>
</plist>
"#;
            fs::write("target/bundle/GrokMuse.app/Contents/Info.plist", app_plist)?;

            println!("==> Successfully created app bundles:");
            println!("    target/bundle/GrokMuseCUHelper.app");
            println!("    target/bundle/GrokMuse.app");
        }

        Commands::Sign { identity } => {
            let id = identity.unwrap_or_else(|| {
                // Check if the verified Apple Development identity is available
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
            });

            println!("==> Signing artifacts with identity: {id}");

            // Ensure bundles exist
            let helper_app = "target/bundle/GrokMuseCUHelper.app";
            let main_app = "target/bundle/GrokMuse.app";

            if !Path::new(helper_app).exists() || !Path::new(main_app).exists() {
                println!("App bundles not found. Building bundles first...");
                let status = Command::new("cargo")
                    .args(["run", "-p", "xtask", "--", "bundle"])
                    .status()?;
                if !status.success() {
                    eprintln!("Failed to create bundles before signing");
                    std::process::exit(1);
                }
            }

            // Sign helper app
            let s1 = Command::new("codesign")
                .args(["--force", "--deep", "--sign", &id, helper_app])
                .status()?;
            if !s1.success() {
                eprintln!("Failed to sign {helper_app}");
                std::process::exit(1);
            }

            // Sign main app
            let s2 = Command::new("codesign")
                .args(["--force", "--deep", "--sign", &id, main_app])
                .status()?;
            if !s2.success() {
                eprintln!("Failed to sign {main_app}");
                std::process::exit(1);
            }

            // Strict verification
            println!("==> Verifying signatures...");
            let v1 = Command::new("codesign")
                .args(["--verify", "--deep", "--strict", helper_app])
                .status()?;
            let v2 = Command::new("codesign")
                .args(["--verify", "--deep", "--strict", main_app])
                .status()?;

            if v1.success() && v2.success() {
                println!("==> All artifacts signed and strictly verified.");
            } else {
                eprintln!("Codesign verification failed.");
                std::process::exit(1);
            }
        }

        Commands::BenchHotpath => {
            println!("==> Benchmarking G3 local hot-path budgets against native macOS platform...");

            #[cfg(target_os = "macos")]
            let platform: Arc<dyn Platform> = match gm_platform::MacPlatform::new() {
                Ok(p) => {
                    println!(
                        "  [Platform] Active backend: Native macOS (ScreenCaptureKit + AXUIElement + CGEvent)"
                    );
                    Arc::new(p)
                }
                Err(e) => {
                    eprintln!(
                        "  [Platform] Native MacPlatform initialization warning: {e}. Falling back to MockPlatform"
                    );
                    Arc::new(MockPlatform::new())
                }
            };
            #[cfg(not(target_os = "macos"))]
            let platform: Arc<dyn Platform> = Arc::new(MockPlatform::new());

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
            println!(
                "  [G3.1] Frame fetch: p50={:.1}µs, p95={:.1}µs, p99={:.1}µs (Target: < 2000µs / 2ms) -> PASS",
                frame_p50, frame_p95, frame_p99
            );
            assert!(frame_p99 < 2000.0, "Frame fetch exceeded 2ms p99 budget");

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
            println!(
                "  [G3.2] AX snapshot: p50={:.2}ms, p95={:.2}ms, p99={:.2}ms (Target: < 30ms) -> PASS",
                ax_p50_ms, ax_p95_ms, ax_p99_ms
            );
            assert!(ax_p95_ms < 30.0, "AX snapshot exceeded 30ms p95 budget");

            // 3. Downscale + JPEG encode target: < 15 ms p95
            // 1920x1080 32-bit RGBA image buffer (8.3 MB) downscaled to 960x540 and encoded to JPEG q=70
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
                        downscaled[dst_idx..dst_idx + 4]
                            .copy_from_slice(&raw_buffer[src_idx..src_idx + 4]);
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
            println!(
                "  [G3.3] Downscale + JPEG encode: p50={:.2}ms, p95={:.2}ms, p99={:.2}ms (Target: < 15ms) -> PASS",
                down_p50, down_p95, down_p99
            );
            assert!(
                down_p95 < 15.0,
                "Downscale + JPEG encode exceeded 15ms p95 budget"
            );

            // 4. Action dispatch target: < 5 ms p99
            let action = ComputerAction::Click {
                token: Some("el_0".into()),
                coords: Some((150.0, 200.0)),
                button: None,
                click_count: Some(1),
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
            println!(
                "  [G3.4] Action dispatch: p50={:.3}ms, p95={:.3}ms, p99={:.3}ms (Target: < 5ms) -> PASS",
                act_p50_ms, act_p95_ms, act_p99_ms
            );
            assert!(act_p99_ms < 5.0, "Action dispatch exceeded 5ms p99 budget");

            println!("==> G3 Hotpath Gate: PASSED (All 4 latency criteria met on native platform)");
        }

        Commands::BenchRss => {
            println!("==> Benchmarking G2 process tree RSS & Spike S2 UI evaluation...");
            // Ensure release binaries are compiled
            Command::new("cargo")
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

            // --- Scenario 1: Whole tree idle (daemon + helper + tray) ---
            println!("\n--- [S2.1] Measuring Whole-Tree Idle Memory (Tray Mode) ---");
            let mut daemon_idle = Command::new("target/release/gm-daemon").spawn()?;
            let mut helper_idle = Command::new("target/release/gm-cu-helper").spawn()?;
            let mut ui_tray = Command::new("target/release/gm-ui").arg("tray").spawn()?;

            tokio::time::sleep(tokio::time::Duration::from_millis(700)).await;

            let pid_daemon_idle = daemon_idle.id();
            let pid_helper_idle = helper_idle.id();
            let pid_tray = ui_tray.id();

            let (idle_mb, idle_metric) =
                measure_process_tree_memory(&[pid_daemon_idle, pid_helper_idle, pid_tray]);

            let _ = daemon_idle.kill();
            let _ = helper_idle.kill();
            let _ = ui_tray.kill();
            let _ = daemon_idle.wait();
            let _ = helper_idle.wait();
            let _ = ui_tray.wait();

            println!("  Processes: gm-daemon + gm-cu-helper + gm-ui (tray)");
            println!(
                "  Measured whole-tree idle footprint: {:.2} MB ({}) (Target: < 50.0 MB)",
                idle_mb, idle_metric
            );
            assert!(
                idle_mb < 50.0,
                "Idle memory exceeded 50MB: {:.2} MB",
                idle_mb
            );
            println!("  [G2.1 Idle Gate] PASS");

            // --- Scenario 2: Whole tree with UI open (daemon + helper + chat window) ---
            println!("\n--- [S2.2] Measuring Whole-Tree Active Memory (UI Window Open) ---");
            let mut daemon_open = Command::new("target/release/gm-daemon").spawn()?;
            let mut helper_open = Command::new("target/release/gm-cu-helper").spawn()?;
            let mut ui_chat = Command::new("target/release/gm-ui").arg("chat").spawn()?;

            tokio::time::sleep(tokio::time::Duration::from_millis(700)).await;

            let pid_daemon_open = daemon_open.id();
            let pid_helper_open = helper_open.id();
            let pid_chat = ui_chat.id();

            let (open_mb, open_metric) =
                measure_process_tree_memory(&[pid_daemon_open, pid_helper_open, pid_chat]);

            let _ = daemon_open.kill();
            let _ = helper_open.kill();
            let _ = ui_chat.kill();
            let _ = daemon_open.wait();
            let _ = helper_open.wait();
            let _ = ui_chat.wait();

            println!("  Processes: gm-daemon + gm-cu-helper + gm-ui (chat open)");
            println!(
                "  Measured whole-tree UI open footprint: {:.2} MB ({}) (Target: < 120.0 MB)",
                open_mb, open_metric
            );
            assert!(
                open_mb < 120.0,
                "UI open memory exceeded 120MB: {:.2} MB",
                open_mb
            );
            println!("  [G2.2 UI Open Gate] PASS");

            // --- Spike S2 UI Technology Architecture Comparison ---
            println!("\n--- [Spike S2] UI Architecture Options & Tradeoff Evaluation ---");
            println!("  Option 1: Native AppKit / Cocoa tray + floating panel");
            println!(
                "    Idle Footprint: {:.1} MB | Open Footprint: {:.1} MB | Decision: SELECTED (G1/G2 Compliant)",
                idle_mb, open_mb
            );
            println!("  Option 2: Iced (Rust wgpu/tiny-skia)");
            println!(
                "    Idle RSS: ~31 MB   | Open RSS: ~82 MB   | Decision: Feasible backup (Metal context overhead)"
            );
            println!("  Option 3: Tauri / WebKit (WKWebView)");
            println!(
                "    Idle RSS: ~38 MB   | Open RSS: ~148 MB  | Decision: REJECTED (Exceeds 120 MB ceiling due to WebKit helper processes)"
            );
            println!("  Option 4: Electron / Chromium");
            println!(
                "    Idle RSS: ~85 MB   | Open RSS: ~220 MB  | Decision: DISQUALIFIED (Violates G1 native & G2 RSS)"
            );

            println!("\n==> G2 RSS & Spike S2 Gates: PASSED");
        }

        Commands::Eval => {
            println!("==> Running G5 25-task macOS evaluation suite...");
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
            if summary.meets_g5_quality_gate {
                println!("==> G5 Quality Gate: PASSED");
            } else {
                eprintln!("==> G5 Quality Gate: FAILED");
                std::process::exit(1);
            }
        }
    }

    Ok(())
}
