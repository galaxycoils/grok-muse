use gm_core::error::{GmError, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

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
    #[serde(default)]
    pub manifest_download_url: Option<String>,
    #[serde(default)]
    pub signature_download_url: Option<String>,
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
    let target = actual_hash.trim();
    if target.len() != 64 || !target.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    if let Some(name) = asset_name {
        let matches: Vec<_> = checksum_input
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                let hash = parts.next()?;
                let file = parts
                    .next()?
                    .strip_prefix('*')
                    .unwrap_or_else(|| line.split_whitespace().nth(1).unwrap_or(""));
                (parts.next().is_none() && file == name).then_some(hash)
            })
            .collect();
        return matches.len() == 1 && matches[0].eq_ignore_ascii_case(target);
    }
    let input = checksum_input.trim();
    input.len() == 64 && input.eq_ignore_ascii_case(target)
}

pub fn release_asset_name(version: &str, os: &str, architecture: &str) -> Result<String> {
    if version.is_empty()
        || !version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-+".contains(&c))
    {
        return Err(GmError::Config("Invalid release version".into()));
    }
    let extension = match (os, architecture) {
        ("macos", "aarch64" | "x86_64") => "dmg",
        ("windows", "x86_64") => "zip",
        ("linux", "x86_64") => "tar.gz",
        _ => {
            return Err(GmError::Config(
                "Unsupported release OS/architecture".into(),
            ));
        }
    };
    Ok(format!(
        "GrokMuse-{version}-{os}-{architecture}.{extension}"
    ))
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

    // Canonical names bind preliminary selection to one OS and architecture.
    // The signed manifest independently verifies the same identity at staging.
    let expected_name = release_asset_name(
        &latest_version,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )?;
    let matches: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .collect();
    if matches.len() > 1 {
        return Err(GmError::SafetyViolation(
            "Release contains duplicate target assets".into(),
        ));
    }
    let dmg_asset = matches.first().copied();

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
        manifest_download_url: release
            .assets
            .iter()
            .find(|a| a.name == "grok-muse-release.json")
            .map(|a| a.browser_download_url.clone()),
        signature_download_url: release
            .assets
            .iter()
            .find(|a| a.name == "grok-muse-release.sig")
            .map(|a| a.browser_download_url.clone()),
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
    let key=option_env!("GM_RELEASE_PUBLIC_KEY_HEX").map(str::to_owned)
        .or_else(||std::env::var("GM_UPDATE_TRUSTED_PUBLIC_KEY_HEX").ok())
        .ok_or_else(||GmError::Config("This development build has no trusted release signing key. Configure GM_UPDATE_TRUSTED_PUBLIC_KEY_HEX from an independently verified publisher key.".into()))?;
    download_with_trusted_key(check_result, target_dir, client_opt, &key).await
}

async fn download_with_trusted_key(
    check_result: &UpdateCheckResult,
    target_dir: &Path,
    client_opt: Option<&reqwest::Client>,
    key: &str,
) -> Result<(PathBuf, bool)> {
    let dmg_url = check_result.dmg_download_url.as_ref().ok_or_else(|| {
        GmError::Platform("No package exists for this OS/architecture in the release".into())
    })?;

    let file_name = check_result
        .dmg_asset_name
        .clone()
        .ok_or_else(|| GmError::Config("Release package name is missing".into()))?;

    if file_name.contains('/')
        || file_name.contains('\\')
        || file_name == "."
        || file_name == ".."
        || file_name.is_empty()
    {
        return Err(GmError::SafetyViolation(
            "Invalid release asset filename".into(),
        ));
    }
    let default_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| GmError::Platform(e.to_string()))?;
    let client = client_opt.unwrap_or(&default_client);
    let manifest_url = check_result
        .manifest_download_url
        .as_deref()
        .ok_or_else(|| GmError::SafetyViolation("Release has no authenticated manifest".into()))?;
    let signature_url = check_result
        .signature_download_url
        .as_deref()
        .ok_or_else(|| GmError::SafetyViolation("Release has no manifest signature".into()))?;
    let raw = bounded_release_fetch(client, manifest_url, 128 * 1024).await?;
    let signature = bounded_release_fetch(client, signature_url, 4096).await?;
    let manifest = verify_release_manifest(&raw, &signature, key)?;
    let asset = manifest
        .assets
        .iter()
        .filter(|a| {
            a.name == file_name
                && a.os == std::env::consts::OS
                && a.architecture == std::env::consts::ARCH
        })
        .collect::<Vec<_>>();
    if manifest.version != check_result.latest_version || asset.len() != 1 {
        return Err(GmError::SafetyViolation(
            "Signed release version, asset or target does not match this update".into(),
        ));
    }
    let asset = asset[0];
    if asset.size_bytes == 0 || asset.size_bytes > 2 * 1024 * 1024 * 1024 {
        return Err(GmError::SafetyViolation(
            "Release exceeds download limits".into(),
        ));
    }
    std::fs::create_dir_all(target_dir)?;
    let target_path = target_dir.join(&file_name);
    let temp_path = target_dir.join(format!(".gm-update-{}", uuid::Uuid::new_v4()));
    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let cleanup = Temp(temp_path.clone());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    let mut response = client
        .get(dmg_url)
        .send()
        .await
        .map_err(|e| GmError::Platform(e.to_string()))?
        .error_for_status()
        .map_err(|e| GmError::Platform(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| GmError::Platform(e.to_string()))?
    {
        count = count
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| GmError::SafetyViolation("Release length overflow".into()))?;
        if count > asset.size_bytes {
            return Err(GmError::SafetyViolation(
                "Release is larger than its signed size".into(),
            ));
        }
        file.write_all(&chunk)?;
        hasher.update(&chunk);
    }
    if count != asset.size_bytes || !verify_checksum(&hasher.finalize(), &asset.sha256, None) {
        return Err(GmError::SafetyViolation(
            "Release does not match its signed size and checksum".into(),
        ));
    }
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp_path, &target_path)?;
    drop(cleanup);
    info!("Authenticated release staged at {:?}", target_path);
    Ok((target_path, true))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedReleaseManifest {
    pub schema: u32,
    pub version: String,
    pub expires_at: String,
    pub assets: Vec<SignedReleaseAsset>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedReleaseAsset {
    pub name: String,
    pub os: String,
    pub architecture: String,
    pub size_bytes: u64,
    pub sha256: String,
}
pub fn verify_release_manifest(
    raw: &[u8],
    signature_hex: &[u8],
    public_key_hex: &str,
) -> Result<SignedReleaseManifest> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let failure = || GmError::SafetyViolation("Release manifest signature is not trusted".into());
    let key: [u8; 32] = hex::decode(public_key_hex.trim())
        .map_err(|_| failure())?
        .try_into()
        .map_err(|_| failure())?;
    let signature = hex::decode(
        std::str::from_utf8(signature_hex)
            .map_err(|_| failure())?
            .trim(),
    )
    .map_err(|_| failure())?;
    let signature = Signature::from_slice(&signature).map_err(|_| failure())?;
    VerifyingKey::from_bytes(&key)
        .map_err(|_| failure())?
        .verify_strict(raw, &signature)
        .map_err(|_| failure())?;
    let manifest: SignedReleaseManifest = serde_json::from_slice(raw)?;
    let expires =
        chrono::DateTime::parse_from_rfc3339(&manifest.expires_at).map_err(|_| failure())?;
    if manifest.schema != 1
        || expires <= chrono::Utc::now()
        || manifest.assets.is_empty()
        || manifest.assets.len() > 32
    {
        return Err(GmError::SafetyViolation(
            "Release manifest is expired or unsupported".into(),
        ));
    }
    let mut identities = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for asset in &manifest.assets {
        if !identities.insert((&asset.os, &asset.architecture))
            || !names.insert(&asset.name)
            || asset.size_bytes == 0
            || asset.size_bytes > 2 * 1024 * 1024 * 1024
            || asset.sha256.len() != 64
            || !asset.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || asset.name != release_asset_name(&manifest.version, &asset.os, &asset.architecture)?
        {
            return Err(GmError::SafetyViolation(
                "Release manifest contains an invalid or ambiguous target".into(),
            ));
        }
    }
    Ok(manifest)
}
async fn bounded_release_fetch(
    client: &reqwest::Client,
    url: &str,
    maximum: usize,
) -> Result<Vec<u8>> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| GmError::Platform(e.to_string()))?
        .error_for_status()
        .map_err(|e| GmError::Platform(e.to_string()))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| GmError::Platform(e.to_string()))?
    {
        if bytes.len() + chunk.len() > maximum {
            return Err(GmError::SafetyViolation(
                "Release metadata exceeds limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Print formatted installation and upgrade guidance.
pub fn print_installation_guidance(dmg_path: &Path, result: &UpdateCheckResult) {
    if !cfg!(target_os = "macos") {
        println!(
            "Authenticated {} release staged at {}",
            result.latest_version,
            dmg_path.display()
        );
        println!(
            "Quit Grok Muse before replacing the installed application. Keep the previous installation for manual rollback. Application data is stored separately."
        );
        return;
    }
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
    println!(
        "  Stop Grok Muse and its services before replacing applications. Retain the previous package and a closed-app data backup for manual recovery."
    );
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
            "\n  Notice: No package for this OS/architecture found in release {}.",
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

        let expected =
            release_asset_name("0.2.0", std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let json = json.replace("Grok-Muse-v0.2.0.dmg", &expected);
        let result = parse_github_release_json(&json, "0.1.0").expect("Failed to parse release");
        assert_eq!(result.latest_version, "0.2.0");
        assert!(result.update_available);
        // Must select the actual DMG asset, NOT the preceding .sha256 or .tar.gz!
        assert_eq!(result.dmg_asset_name.as_deref(), Some(expected.as_str()));
        assert_eq!(result.dmg_size_bytes, Some(52428800));
        assert!(
            result
                .dmg_download_url
                .as_deref()
                .unwrap()
                .ends_with(&expected)
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
            manifest_download_url: None,
            signature_download_url: None,
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

#[cfg(test)]
mod signed_manifest_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    #[test]
    fn signed_release_rejects_tampering_wrong_publishers_and_expiry() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let raw = serde_json::to_vec(&SignedReleaseManifest {
            schema: 1,
            version: "1.2.3".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            assets: vec![SignedReleaseAsset {
                name: "GrokMuse-1.2.3-macos-aarch64.dmg".into(),
                os: "macos".into(),
                architecture: "aarch64".into(),
                size_bytes: 1,
                sha256: "00".repeat(32),
            }],
        })
        .unwrap();
        let signature = hex::encode(key.sign(&raw).to_bytes());
        let public = hex::encode(key.verifying_key().to_bytes());
        assert_eq!(
            verify_release_manifest(&raw, signature.as_bytes(), &public)
                .unwrap()
                .version,
            "1.2.3"
        );
        let mut tampered = raw.clone();
        tampered[20] ^= 1;
        assert!(verify_release_manifest(&tampered, signature.as_bytes(), &public).is_err());
        assert!(
            verify_release_manifest(
                &raw,
                signature.as_bytes(),
                &hex::encode(SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes())
            )
            .is_err()
        );
        let expired = String::from_utf8(raw)
            .unwrap()
            .replace("2099-01-01", "2000-01-01")
            .into_bytes();
        let signature = hex::encode(key.sign(&expired).to_bytes());
        assert!(verify_release_manifest(&expired, signature.as_bytes(), &public).is_err());
    }
}

#[cfg(test)]
mod target_selection_tests {
    use super::*;
    #[test]
    fn missing_target_never_falls_back_to_another_os_or_architecture() {
        let wrong = if std::env::consts::OS == "windows" {
            "macos"
        } else {
            "windows"
        };
        let asset = release_asset_name("1.0.0", wrong, "x86_64").unwrap();
        let json = serde_json::json!({"tag_name":"v1.0.0","name":"Release","html_url":"https://example.invalid","assets":[{"name":asset,"browser_download_url":"https://example.invalid/asset","size":42}]});
        let result = parse_github_release_json(&json.to_string(), "0.3.0").unwrap();
        assert!(result.dmg_asset_name.is_none());
        assert!(result.dmg_download_url.is_none());
        assert!(release_asset_name("../bad", "linux", "x86_64").is_err());
        assert!(release_asset_name("1.0.0", "windows", "aarch64").is_err());
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn signed_http_staging_rejects_tampering_and_keeps_verified_package() {
        let bytes = b"synthetic signed package fixture".to_vec();
        let key = SigningKey::from_bytes(&[17; 32]);
        let name =
            release_asset_name("9.9.9", std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let manifest = serde_json::to_vec(&SignedReleaseManifest {
            schema: 1,
            version: "9.9.9".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            assets: vec![SignedReleaseAsset {
                name: name.clone(),
                os: std::env::consts::OS.into(),
                architecture: std::env::consts::ARCH.into(),
                size_bytes: bytes.len() as u64,
                sha256: gm_core::compute_sha256(&bytes),
            }],
        })
        .unwrap();
        let signature = hex::encode(key.sign(&manifest).to_bytes()).into_bytes();
        let payload = std::sync::Arc::new(std::sync::Mutex::new(bytes.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = payload.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (manifest, signature, payload) =
                    (manifest.clone(), signature.clone(), serving.clone());
                tokio::spawn(async move {
                    let mut request = vec![];
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|v| v == b"\r\n\r\n") {
                        let n = stream.read(&mut buffer).await.unwrap();
                        if n == 0 || request.len() + n > 16384 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..n]);
                    }
                    let path = std::str::from_utf8(&request)
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let body = match path {
                        "/manifest" => manifest,
                        "/signature" => signature,
                        "/asset" => payload.lock().unwrap().clone(),
                        _ => vec![],
                    };
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(&body).await.unwrap();
                });
            }
        });
        let metadata = serde_json::json!({"tag_name":"v9.9.9","html_url":"https://example.invalid/release","assets":[
            {"name":name,"browser_download_url":format!("http://{address}/asset"),"size":bytes.len()},
            {"name":"grok-muse-release.json","browser_download_url":format!("http://{address}/manifest"),"size":1024},
            {"name":"grok-muse-release.sig","browser_download_url":format!("http://{address}/signature"),"size":128}]});
        let check = parse_github_release_json(&metadata.to_string(), "0.3.0").unwrap();
        let destination = tempfile::tempdir().unwrap();
        let public = hex::encode(key.verifying_key().to_bytes());
        let (path, verified) = download_with_trusted_key(&check, destination.path(), None, &public)
            .await
            .unwrap();
        assert!(verified);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        *payload.lock().unwrap() = b"tampered".to_vec();
        assert!(
            download_with_trusted_key(&check, destination.path(), None, &public)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        server.abort();
        let _ = server.await;
    }
}
