use gm_core::error::{GmError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub const DEFAULT_REPO: &str = "grok-muse/grok-muse";

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

/// Parse a semantic version string (e.g., "0.1.0", "v1.2.3-beta").
pub fn parse_version(tag: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let clean = tag.trim().trim_start_matches('v').trim_start_matches('V');
    let mut parts = clean.splitn(2, '-');
    let core = parts.next()?;
    let prerelease = parts.next().map(|s| s.to_string());

    let mut nums = core.split('.');
    let major = nums.next()?.parse::<u64>().ok()?;
    let minor = nums.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = nums.next().unwrap_or("0").parse::<u64>().ok()?;

    Some((major, minor, patch, prerelease))
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

    // If numbers match, non-prerelease is newer than prerelease
    match (&curr.3, &cand.3) {
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => false,
    }
}

/// Compute SHA-256 digest of byte slice as lowercase hexadecimal string.
/// Self-contained standard FIPS 180-4 implementation for zero-dependency cross-platform reliability.
pub fn compute_sha256(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, w_val) in w.iter_mut().take(16).enumerate() {
            let idx = i * 4;
            *w_val =
                u32::from_be_bytes([chunk[idx], chunk[idx + 1], chunk[idx + 2], chunk[idx + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut h_val = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h_val
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h_val = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(h_val);
    }

    format!(
        "{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]
    )
}

/// Verify checksum of data against expected hex digest or checksum file content.
pub fn verify_checksum(data: &[u8], checksum_input: &str) -> bool {
    let actual_hash = compute_sha256(data);
    let target = actual_hash.to_lowercase();

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

    // Locate DMG asset
    let dmg_asset = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".dmg") || a.name.contains("Grok-Muse"));

    // Locate SHA-256 checksum asset
    let checksum_asset = release.assets.iter().find(|a| {
        a.name.ends_with(".sha256")
            || a.name.ends_with(".sha256sum")
            || a.name.eq_ignore_ascii_case("SHA256SUMS")
    });

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

    let resp = client
        .get(&url)
        .header("User-Agent", format!("grok-muse-updater/{current_version}"))
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .map_err(|e| GmError::Platform(format!("Network error querying GitHub Releases: {e}")))?;

    if !resp.status().is_success() {
        return Err(GmError::Platform(format!(
            "GitHub Releases API returned status {}: {}",
            resp.status(),
            resp.status().canonical_reason().unwrap_or("Unknown")
        )));
    }

    let json_text = resp
        .text()
        .await
        .map_err(|e| GmError::Platform(format!("Failed reading response body: {e}")))?;

    parse_github_release_json(&json_text, current_version)
}

/// Download DMG release asset and verify its SHA-256 checksum if available.
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

    let default_client = reqwest::Client::new();
    let client = client_opt.unwrap_or(&default_client);

    info!(
        "Downloading release DMG from {} to {:?}",
        dmg_url, target_path
    );

    let resp = client
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

    let dmg_bytes = resp
        .bytes()
        .await
        .map_err(|e| GmError::Platform(format!("Failed reading DMG bytes: {e}")))?;

    std::fs::write(&target_path, &dmg_bytes)?;
    info!(
        "Successfully wrote {} bytes to {:?}",
        dmg_bytes.len(),
        target_path
    );

    // Verify SHA-256 checksum if checksum URL is provided
    let mut verified = false;
    if let Some(ref checksum_url) = check_result.checksum_download_url {
        info!("Fetching release checksum from {}", checksum_url);
        if let Ok(c_resp) = client.get(checksum_url).send().await
            && c_resp.status().is_success()
            && let Ok(c_text) = c_resp.text().await
        {
            if verify_checksum(&dmg_bytes, &c_text) {
                info!("SHA-256 checksum verified successfully!");
                verified = true;
            } else {
                warn!("Warning: SHA-256 checksum verification failed!");
            }
        }
    } else if let Some(ref expected_hash) = check_result.expected_sha256 {
        if verify_checksum(&dmg_bytes, expected_hash) {
            info!("SHA-256 checksum verified against expected hash!");
            verified = true;
        } else {
            warn!("Warning: SHA-256 checksum did not match expected hash!");
        }
    }

    Ok((target_path, verified))
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
            "UNVERIFIED (checksum not found or matched)"
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
        assert_eq!(
            parse_version("v2.0.0-rc1"),
            Some((2, 0, 0, Some("rc1".into())))
        );
        assert_eq!(parse_version("invalid"), None);
    }

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.0-alpha", "0.1.0"));
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
    }

    #[test]
    fn test_verify_checksum() {
        let data = b"abc";
        let valid_single = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_checksum(data, valid_single));

        let valid_sha256sums = format!(
            "{valid_single}  Grok-Muse.dmg\n1111222233334444555566667777888899990000111122223333444455556666  other.dmg"
        );
        assert!(verify_checksum(data, &valid_sha256sums));

        let invalid = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_checksum(data, invalid));
    }

    #[test]
    fn test_parse_github_release_json() {
        let json = r###"{
            "tag_name": "v0.2.0",
            "name": "Grok-Muse v0.2.0 Release",
            "html_url": "https://github.com/grok-muse/grok-muse/releases/tag/v0.2.0",
            "body": "## What's Changed\n* Added auto-update client (A11)\n* Added Linux & Windows platform foundations (A13)",
            "published_at": "2026-11-03T21:00:00Z",
            "assets": [
                {
                    "name": "Grok-Muse.dmg",
                    "browser_download_url": "https://github.com/grok-muse/grok-muse/releases/download/v0.2.0/Grok-Muse.dmg",
                    "size": 52428800,
                    "content_type": "application/x-apple-diskimage"
                },
                {
                    "name": "Grok-Muse.dmg.sha256",
                    "browser_download_url": "https://github.com/grok-muse/grok-muse/releases/download/v0.2.0/Grok-Muse.dmg.sha256",
                    "size": 64,
                    "content_type": "text/plain"
                }
            ]
        }"###;

        let result = parse_github_release_json(json, "0.1.0").expect("Failed to parse release");
        assert_eq!(result.latest_version, "0.2.0");
        assert!(result.update_available);
        assert_eq!(result.dmg_asset_name.as_deref(), Some("Grok-Muse.dmg"));
        assert!(result.dmg_download_url.is_some());
        assert!(result.checksum_download_url.is_some());

        // Test when already up to date
        let result_up_to_date = parse_github_release_json(json, "0.2.0").unwrap();
        assert!(!result_up_to_date.update_available);
    }
}
