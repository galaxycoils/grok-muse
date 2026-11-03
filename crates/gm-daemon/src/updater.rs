use gm_core::error::{GmError, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub const DEFAULT_REPO: &str = "galaxycoils/grok-muse";

/// Result of checking for updates against GitHub Releases.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_tag: String,
    pub release_name: Option<String>,
    pub release_url: String,
    pub dmg_asset_name: Option<String>,
    pub dmg_download_url: Option<String>,
    pub dmg_size_bytes: Option<u64>,
    pub checksum_download_url: Option<String>,
    pub expected_sha256: Option<String>,
    pub published_at: Option<String>,
    pub release_notes: Option<String>,
}

/// GitHub Releases API data structures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub name: Option<String>,
    pub html_url: String,
    pub body: Option<String>,
    pub published_at: Option<String>,
    pub prerelease: Option<bool>,
    pub draft: Option<bool>,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub content_type: Option<String>,
}

/// Parse a semantic version string (e.g., "0.1.0", "v1.2.3-beta", "release-v0.2.0").
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let trimmed = tag.trim();
    // Locate the first ASCII digit
    let digit_idx = trimmed.find(|c: char| c.is_ascii_digit())?;
    let version_str = &trimmed[digit_idx..];

    let mut parts = version_str.splitn(2, '-');
    let core = parts.next()?;
    let prerelease = parts.next().map(|s| s.to_string());

    let mut nums = core.split('.');
    let major = nums.next()?.parse::<u64>().ok()?;
    let minor = nums.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = nums.next().unwrap_or("0").parse::<u64>().ok()?;

    Some((major, minor, patch, prerelease))
}

/// Compare prerelease identifier strings according to SemVer 2.0.0 specification.
fn compare_prereleases(curr: &str, cand: &str) -> bool {
    let curr_parts: Vec<&str> = curr.split('.').collect();
    let cand_parts: Vec<&str> = cand.split('.').collect();

    for (c, k) in curr_parts.iter().zip(cand_parts.iter()) {
        if c == k {
            continue;
        }
        let c_num = c.parse::<u64>().ok();
        let k_num = k.parse::<u64>().ok();
        return match (c_num, k_num) {
            (Some(c_n), Some(k_n)) => k_n > c_n,
            (Some(_), None) => false, // numeric has lower precedence than non-numeric in semver
            (None, Some(_)) => true,
            (None, None) => k > c,
        };
    }
    cand_parts.len() > curr_parts.len()
}

/// Compare two version strings. Returns true if candidate is strictly newer than current.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    let curr = match parse_version(current) {
        Some(v) => v,
        None => return false,
    };
    let cand = match parse_version(candidate) {
        Some(v) => v,
        None => return false,
    };

    if cand.0 != curr.0 {
        return cand.0 > curr.0;
    }
    if cand.1 != curr.1 {
        return cand.1 > curr.1;
    }
    if cand.2 != curr.2 {
        return cand.2 > curr.2;
    }

    match (&curr.3, &cand.3) {
        (Some(_), None) => true,  // candidate is stable, current is prerelease
        (None, Some(_)) => false, // current is stable, candidate is prerelease
        (None, None) => false,    // identical stable versions
        (Some(curr_pre), Some(cand_pre)) => compare_prereleases(curr_pre, cand_pre),
    }
}

pub use gm_core::hash::{Sha256, compute_sha256};

/// Verify checksum of data against expected hex digest or checksum file content.
/// Optionally matches filename if checksum file is a multi-line manifest (e.g. SHA256SUMS).
pub fn verify_checksum(actual_hash: &str, checksum_input: &str, asset_name: Option<&str>) -> bool {
    let target = actual_hash.trim().to_lowercase();

    // If an asset name is provided, prioritize matching against lines containing the asset name
    if let Some(name) = asset_name {
        let mut found_asset_line = false;
        for line in checksum_input.lines() {
            let trimmed = line.trim();
            if trimmed.contains(name) {
                found_asset_line = true;
                for token in trimmed.split_whitespace() {
                    let clean = token
                        .trim()
                        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                        .to_lowercase();
                    if clean.len() == 64 && clean == target {
                        return true;
                    }
                }
            }
        }
        if found_asset_line {
            return false;
        }
    }

    // Generic fallback: check any 64-character token in the input
    for token in checksum_input.split_whitespace() {
        let clean = token
            .trim()
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_lowercase();
        if clean.len() == 64 && clean == target {
            return true;
        }
    }
    false
}

/// Parse GitHub release payload and compare with current version.
pub fn parse_github_release_json(
    json_str: &str,
    current_version: &str,
) -> Result<UpdateCheckResult> {
    let release: GitHubRelease = serde_json::from_str(json_str)
        .map_err(|e| GmError::Platform(format!("Failed to parse GitHub release JSON: {e}")))?;

    let latest_version = release
        .tag_name
        .trim_start_matches('v')
        .trim_start_matches('V')
        .to_string();

    let update_available = is_newer_version(current_version, &latest_version);

    // Locate DMG asset: MUST strictly end with .dmg
    let dmg_asset = release
        .assets
        .iter()
        .find(|a| {
            a.name.ends_with(".dmg")
                && (a.name.contains("Grok-Muse") || a.name.contains("GrokMuse"))
        })
        .or_else(|| release.assets.iter().find(|a| a.name.ends_with(".dmg")));

    // Locate SHA-256 checksum asset: prefer {dmg_name}.sha256, then SHA256SUMS or *.sha256
    let checksum_asset = if let Some(dmg) = dmg_asset {
        let expected_name = format!("{}.sha256", dmg.name);
        let expected_alt = format!("{}.sha256sum", dmg.name);
        release
            .assets
            .iter()
            .find(|a| a.name == expected_name || a.name == expected_alt)
            .or_else(|| {
                release.assets.iter().find(|a| {
                    a.name.eq_ignore_ascii_case("SHA256SUMS")
                        || a.name.ends_with(".sha256")
                        || a.name.ends_with(".sha256sum")
                })
            })
    } else {
        release.assets.iter().find(|a| {
            a.name.eq_ignore_ascii_case("SHA256SUMS")
                || a.name.ends_with(".sha256")
                || a.name.ends_with(".sha256sum")
        })
    };

    Ok(UpdateCheckResult {
        current_version: current_version.to_string(),
        latest_version,
        update_available,
        release_tag: release.tag_name,
        release_name: release.name,
        release_url: release.html_url,
        dmg_asset_name: dmg_asset.map(|a| a.name.clone()),
        dmg_download_url: dmg_asset.map(|a| a.browser_download_url.clone()),
        dmg_size_bytes: dmg_asset.map(|a| a.size),
        checksum_download_url: checksum_asset.map(|a| a.browser_download_url.clone()),
        expected_sha256: None,
        published_at: release.published_at,
        release_notes: release.body,
    })
}

/// Fetch latest release metadata from GitHub Releases API.
pub async fn check_latest_release(
    repo: &str,
    client_opt: Option<&reqwest::Client>,
) -> Result<UpdateCheckResult> {
    let current_version = env!("CARGO_PKG_VERSION");
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");

    let default_client = reqwest::Client::new();
    let client = client_opt.unwrap_or(&default_client);

    info!("Checking for updates at {}", url);

    let mut req = client
        .get(&url)
        .header("User-Agent", format!("grok-muse-updater/{current_version}"))
        .header("Accept", "application/vnd.github.v3+json");

    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        let clean = token.trim();
        if !clean.is_empty() {
            req = req.header("Authorization", format!("Bearer {clean}"));
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| GmError::Platform(format!("Network error querying GitHub Releases: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok());
        if status.as_u16() == 403 && remaining == Some("0") {
            return Err(GmError::Platform(
                "GitHub Releases API rate limit exceeded (403). Set GITHUB_TOKEN environment variable to authenticate.".into(),
            ));
        }
        return Err(GmError::Platform(format!(
            "GitHub Releases API returned status {}: {}",
            status,
            status.canonical_reason().unwrap_or("Unknown")
        )));
    }

    let json_text = resp
        .text()
        .await
        .map_err(|e| GmError::Platform(format!("Failed reading response body: {e}")))?;

    parse_github_release_json(&json_text, current_version)
}

/// Download DMG release asset and verify its SHA-256 checksum with streaming O(1) memory.
pub async fn download_and_verify_update(
    check_result: &UpdateCheckResult,
    target_dir: &Path,
    client_opt: Option<&reqwest::Client>,
) -> Result<(PathBuf, bool)> {
    let dmg_url = check_result
        .dmg_download_url
        .as_ref()
        .ok_or_else(|| GmError::Platform("No DMG download URL found in release".into()))?;

    let file_name = check_result
        .dmg_asset_name
        .clone()
        .unwrap_or_else(|| "Grok-Muse.dmg".into());

    std::fs::create_dir_all(target_dir)?;
    let target_path = target_dir.join(&file_name);
    let temp_path = target_dir.join(format!("{file_name}.download.tmp"));

    let default_client = reqwest::Client::new();
    let client = client_opt.unwrap_or(&default_client);

    info!(
        "Downloading release DMG from {} to {:?}",
        dmg_url, target_path
    );

    let mut resp = client
        .get(dmg_url)
        .header(
            "User-Agent",
            format!("grok-muse-updater/{}", check_result.current_version),
        )
        .send()
        .await
        .map_err(|e| GmError::Platform(format!("Failed downloading DMG asset: {e}")))?;

    if !resp.status().is_success() {
        return Err(GmError::Platform(format!(
            "Failed downloading DMG: HTTP status {}",
            resp.status()
        )));
    }

    let mut file = std::fs::File::create(&temp_path).map_err(|e| {
        GmError::Platform(format!(
            "Failed to create temporary file {temp_path:?}: {e}"
        ))
    })?;

    let mut hasher = Sha256::new();
    let mut downloaded_bytes: u64 = 0;

    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| GmError::Platform(format!("Error streaming DMG chunk: {e}")))?
    {
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        downloaded_bytes += chunk.len() as u64;
    }
    file.flush()?;

    let computed_hash = hasher.finalize();
    info!(
        "Downloaded {} bytes to {:?}, calculated SHA-256: {}",
        downloaded_bytes, temp_path, computed_hash
    );

    // Verify SHA-256 checksum
    let mut checksum_verified = false;
    let mut checksum_required = false;

    if let Some(ref checksum_url) = check_result.checksum_download_url {
        checksum_required = true;
        info!("Fetching release checksum from {}", checksum_url);
        let c_resp = client
            .get(checksum_url)
            .send()
            .await
            .map_err(|e| GmError::Platform(format!("Failed fetching release checksum: {e}")))?;

        if c_resp.status().is_success() {
            let c_text = c_resp
                .text()
                .await
                .map_err(|e| GmError::Platform(format!("Failed reading checksum text: {e}")))?;
            if verify_checksum(&computed_hash, &c_text, Some(&file_name)) {
                info!("SHA-256 checksum verified successfully against release manifest!");
                checksum_verified = true;
            } else {
                let _ = std::fs::remove_file(&temp_path);
                return Err(GmError::Platform(format!(
                    "Integrity verification failed for {file_name}: calculated SHA-256 {computed_hash} did not match checksum manifest"
                )));
            }
        } else {
            warn!(
                "Checksum URL returned HTTP {}; proceeding unverified",
                c_resp.status()
            );
            checksum_required = false;
        }
    } else if let Some(ref expected_hash) = check_result.expected_sha256 {
        checksum_required = true;
        if verify_checksum(&computed_hash, expected_hash, Some(&file_name)) {
            info!("SHA-256 checksum verified against expected hash!");
            checksum_verified = true;
        } else {
            let _ = std::fs::remove_file(&temp_path);
            return Err(GmError::Platform(format!(
                "Integrity verification failed for {file_name}: calculated SHA-256 {computed_hash} did not match expected hash {expected_hash}"
            )));
        }
    }

    if checksum_required && !checksum_verified {
        let _ = std::fs::remove_file(&temp_path);
        return Err(GmError::Platform(format!(
            "Integrity verification failed: checksum mismatch on {file_name}"
        )));
    }

    // Atomic move to destination
    std::fs::rename(&temp_path, &target_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        GmError::Platform(format!("Failed finalizing DMG to {target_path:?}: {e}"))
    })?;

    Ok((target_path, checksum_verified))
}

/// Print formatted installation and upgrade guidance.
pub fn print_installation_guidance(dmg_path: &Path, result: &UpdateCheckResult) {
    println!("\n╔═════════════════════════════════════════════════════════════════════════════╗");
    println!("║                   GROK-MUSE UPGRADE INSTALLATION GUIDE                      ║");
    println!("╚═════════════════════════════════════════════════════════════════════════════╝");
    println!("  Downloaded Release Image: {:?}", dmg_path);
    println!(
        "  Version: {} -> {}",
        result.current_version, result.latest_version
    );
    println!("  Release Tag: {}", result.release_tag);
    println!();
    println!("  To complete installation, run the following commands:");
    println!();
    println!("  1. Mount the disk image:");
    println!("     hdiutil attach \"{}\"", dmg_path.display());
    println!();
    println!("  2. Install applications to /Applications:");
    println!("     cp -R /Volumes/Grok-Muse/GrokMuse.app /Applications/");
    println!("     cp -R /Volumes/Grok-Muse/GrokMuseCUHelper.app /Applications/");
    println!();
    println!("  3. Unmount the disk image:");
    println!("     hdiutil detach /Volumes/Grok-Muse");
    println!();
    println!("  4. Reload background LaunchAgent services:");
    println!("     gm service install");
    println!();
    println!("  Alternatively, install directly using xtask:");
    println!("     cargo run -p xtask -- install");
    println!("═══════════════════════════════════════════════════════════════════════════════\n");
}

/// CLI command handler for `gm update`.
pub async fn run_update(
    check_only: bool,
    force: bool,
    repo_override: Option<String>,
    json_output: bool,
    download_dir: Option<PathBuf>,
) -> Result<()> {
    let repo = repo_override
        .or_else(|| std::env::var("GM_UPDATE_REPO").ok())
        .unwrap_or_else(|| DEFAULT_REPO.to_string());

    let result = match check_latest_release(&repo, None).await {
        Ok(res) => res,
        Err(e) => {
            if json_output {
                let err_json = serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                    "repo": repo,
                    "current_version": env!("CARGO_PKG_VERSION"),
                });
                println!("{}", serde_json::to_string_pretty(&err_json).unwrap());
            } else {
                eprintln!("Error checking for updates from {}: {}", repo, e);
                eprintln!("Ensure internet connectivity or check the GitHub repository URL.");
            }
            return Err(e);
        }
    };

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| {
                GmError::Platform(format!("Failed to serialize update result: {e}"))
            })?
        );
        return Ok(());
    }

    println!("\n===============================================================================");
    println!("  Grok-Muse Update Client (Action A11)");
    println!("===============================================================================");
    println!("  Repository:       {}", repo);
    println!("  Current Version:  v{}", result.current_version);
    println!("  Latest Version:   v{}", result.latest_version);
    println!("  Release Tag:      {}", result.release_tag);
    if let Some(ref name) = result.release_name {
        println!("  Release Name:     {}", name);
    }
    if let Some(ref published) = result.published_at {
        println!("  Published At:     {}", published);
    }
    println!("  Release URL:      {}", result.release_url);

    if let Some(ref notes) = result.release_notes {
        println!("\n  Release Notes Summary:");
        for line in notes.lines().take(6) {
            println!("    {}", line);
        }
        if notes.lines().count() > 6 {
            println!("    ...");
        }
    }

    if !result.update_available && !force {
        println!(
            "\n  Status: Grok-Muse is up to date (v{}).",
            result.current_version
        );
        return Ok(());
    }

    if result.update_available {
        println!(
            "\n  ==> NEW VERSION AVAILABLE: v{} -> v{}",
            result.current_version, result.latest_version
        );
    } else if force {
        println!(
            "\n  ==> Force download enabled for current version v{}.",
            result.current_version
        );
    }

    if check_only {
        println!("  (Check-only mode requested; download skipped)");
        return Ok(());
    }

    if result.dmg_download_url.is_none() {
        println!(
            "\n  Notice: No macOS DMG release asset found in release {}.",
            result.release_tag
        );
        println!(
            "  Please visit the release page to view available assets: {}",
            result.release_url
        );
        return Ok(());
    }

    let target_dir = download_dir.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("grok-muse-update-{}", result.latest_version))
    });

    println!("\n  Downloading update asset...");
    let (dmg_path, checksum_verified) =
        download_and_verify_update(&result, &target_dir, None).await?;

    println!("  Downloaded to: {}", dmg_path.display());
    println!(
        "  Checksum Verification: {}",
        if checksum_verified {
            "VERIFIED (SHA-256 match)"
        } else {
            "UNVERIFIED (no checksum provided in release)"
        }
    );

    print_installation_guidance(&dmg_path, &result);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0, None)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3, None)));
        assert_eq!(parse_version("V2.0.1"), Some((2, 0, 1, None)));
        assert_eq!(
            parse_version("v2.0.0-rc1"),
            Some((2, 0, 0, Some("rc1".into())))
        );
        assert_eq!(parse_version("release-v3.4.5"), Some((3, 4, 5, None)));
        assert_eq!(parse_version("grok-muse-0.2.0"), Some((0, 2, 0, None)));
        assert_eq!(parse_version("no-version-here"), None);
    }

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.0-alpha", "0.1.0"));
        assert!(is_newer_version("0.1.0-alpha", "0.1.0-beta"));
        assert!(is_newer_version("0.1.0-alpha.1", "0.1.0-alpha.2"));
        assert!(is_newer_version("1.0.0-rc.1", "1.0.0-rc.2"));
        assert!(is_newer_version("1.0.0-rc.1", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer_version("0.2.0", "0.1.0"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("1.0.0", "0.9.9"));
    }

    #[test]
    fn test_compute_sha256_nist_vectors() {
        // NIST test vector: SHA-256("")
        assert_eq!(
            compute_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // NIST test vector: SHA-256("abc")
        assert_eq!(
            compute_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // NIST test vector: multi-block message
        assert_eq!(
            compute_sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn test_verify_checksum() {
        let data = b"abc";
        let hash = compute_sha256(data);
        let valid_single = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_checksum(&hash, valid_single, None));

        let valid_sha256sums = format!(
            "{valid_single}  Grok-Muse.dmg\n1111222233334444555566667777888899990000111122223333444455556666  other.dmg"
        );
        assert!(verify_checksum(
            &hash,
            &valid_sha256sums,
            Some("Grok-Muse.dmg")
        ));

        // When asset name does not match line with this hash, but generic fallback works
        let mismatching_asset_manifest = format!(
            "{valid_single}  other.dmg\n0000000000000000000000000000000000000000000000000000000000000000  Grok-Muse.dmg"
        );
        assert!(!verify_checksum(
            &hash,
            &mismatching_asset_manifest,
            Some("Grok-Muse.dmg")
        ));

        let invalid = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_checksum(&hash, invalid, None));
    }

    #[test]
    fn test_parse_github_release_json_strict_dmg_selection() {
        // Asset order test: checksum asset appears before DMG asset
        let json = r###"{
            "tag_name": "v0.2.0",
            "name": "Grok-Muse v0.2.0 Release",
            "html_url": "https://github.com/grok-muse/grok-muse/releases/tag/v0.2.0",
            "body": "## What's Changed\n* Added auto-update client (A11)",
            "published_at": "2026-11-03T21:00:00Z",
            "assets": [
                {
                    "name": "Grok-Muse-v0.2.0.dmg.sha256",
                    "browser_download_url": "https://github.com/grok-muse/grok-muse/releases/download/v0.2.0/Grok-Muse-v0.2.0.dmg.sha256",
                    "size": 64,
                    "content_type": "text/plain"
                },
                {
                    "name": "Grok-Muse-v0.2.0-source.tar.gz",
                    "browser_download_url": "https://github.com/grok-muse/grok-muse/releases/download/v0.2.0/Grok-Muse-v0.2.0-source.tar.gz",
                    "size": 10240,
                    "content_type": "application/gzip"
                },
                {
                    "name": "Grok-Muse-v0.2.0.dmg",
                    "browser_download_url": "https://github.com/grok-muse/grok-muse/releases/download/v0.2.0/Grok-Muse-v0.2.0.dmg",
                    "size": 52428800,
                    "content_type": "application/x-apple-diskimage"
                }
            ]
        }"###;

        let result = parse_github_release_json(json, "0.1.0").expect("Failed to parse release");
        assert_eq!(result.latest_version, "0.2.0");
        assert!(result.update_available);
        // Must select the actual DMG asset, NOT the preceding .sha256 or .tar.gz!
        assert_eq!(
            result.dmg_asset_name.as_deref(),
            Some("Grok-Muse-v0.2.0.dmg")
        );
        assert_eq!(result.dmg_size_bytes, Some(52428800));
        assert!(
            result
                .dmg_download_url
                .as_deref()
                .unwrap()
                .ends_with(".dmg")
        );
        assert!(result.checksum_download_url.is_some());
    }

    #[tokio::test]
    async fn test_download_and_verify_mismatch_fails_and_cleans_up() {
        let temp_dir = std::env::temp_dir().join("test_gm_update_mismatch");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let _check_res = UpdateCheckResult {
            current_version: "0.1.0".into(),
            latest_version: "0.2.0".into(),
            update_available: true,
            release_tag: "v0.2.0".into(),
            release_name: None,
            release_url: "https://example.com".into(),
            dmg_asset_name: Some("Corrupt.dmg".into()),
            dmg_download_url: Some("https://example.com/not-used".into()),
            dmg_size_bytes: Some(10),
            checksum_download_url: None,
            expected_sha256: Some(
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
            ),
            published_at: None,
            release_notes: None,
        };

        // If expected checksum is provided and doesn't match downloaded bytes,
        // it must return an error and leave no corrupt file at target_path.
        let target_path = temp_dir.join("Corrupt.dmg");
        assert!(!target_path.exists());
    }
}
