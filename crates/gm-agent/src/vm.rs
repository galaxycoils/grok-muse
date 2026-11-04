//! Offline workspace VM execution. No host shell, host mounts, or guest network device.
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use gm_core::{
    config::WorkspaceVmConfig,
    error::{GmError, Result},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::Command,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageManifest {
    version: u32,
    architecture: String,
    sha256: String,
    #[serde(default)]
    firmware_sha256: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct GuestFile {
    path: String,
    data: String,
}
#[derive(Serialize)]
struct GuestRequest {
    version: u32,
    command: String,
    timeout_ms: u64,
    files: Vec<GuestFile>,
}
#[derive(Deserialize)]
struct GuestReply {
    version: u32,
    exit_code: i32,
    stdout: String,
    stderr: String,
    #[serde(default)]
    artifacts: Vec<GuestFile>,
}
pub struct VmOutcome {
    pub exit_code: i32,
    pub output: String,
    pub artifacts: Vec<VmArtifact>,
}
pub struct VmArtifact {
    pub path: String,
    pub bytes: Vec<u8>,
}
fn decode_artifacts(files: Vec<GuestFile>) -> Result<Vec<VmArtifact>> {
    let mut artifacts = Vec::new();
    let mut total = 0usize;
    let mut names = std::collections::HashSet::new();
    if files.len() > 32 {
        return Err(config_error("Guest exported too many files"));
    }
    for file in files {
        if file.path.len() > 1024
            || file.path.contains('\\')
            || file.path.split('/').any(|c| {
                c.is_empty() || c == "." || c == ".." || c.contains(':') || c.starts_with('.')
            })
            || !names.insert(file.path.clone())
        {
            return Err(config_error(
                "Guest exported an unsafe or duplicate artifact path",
            ));
        }
        if file.data.len() > 1_400_000 {
            return Err(config_error("Guest artifact exceeds 1 MiB"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.data)
            .map_err(|_| config_error("Invalid guest artifact encoding"))?;
        total = total.saturating_add(bytes.len());
        if bytes.len() > 1024 * 1024 || total > 4 * 1024 * 1024 {
            return Err(config_error("Guest artifact export exceeds its byte limit"));
        }
        artifacts.push(VmArtifact {
            path: file.path,
            bytes,
        });
    }
    Ok(artifacts)
}
fn config_error(message: impl Into<String>) -> GmError {
    GmError::Config(message.into())
}

fn verified_image(config: &WorkspaceVmConfig) -> Result<()> {
    if !config.enabled {
        return Err(config_error(
            "Workspace VM is not configured. Host shell fallback is disabled.",
        ));
    }
    let raw = std::fs::read(&config.manifest_path)?;
    let signature = hex::decode(std::fs::read_to_string(&config.signature_path)?.trim())
        .map_err(|_| config_error("Invalid image signature"))?;
    let key = hex::decode(&config.trusted_public_key_hex)
        .map_err(|_| config_error("Invalid trusted image public key"))?;
    let key: [u8; 32] = key
        .try_into()
        .map_err(|_| config_error("Image public key must be 32 bytes"))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| config_error("Invalid Ed25519 image signature"))?;
    VerifyingKey::from_bytes(&key)
        .map_err(|_| config_error("Invalid image signing key"))?
        .verify_strict(&raw, &signature)
        .map_err(|_| config_error("Workspace image manifest signature is not trusted"))?;
    let manifest: ImageManifest = serde_json::from_slice(&raw)?;
    if manifest.version != 1 || manifest.architecture != std::env::consts::ARCH {
        return Err(config_error(
            "Workspace image version or architecture is incompatible",
        ));
    }
    let mut file = std::fs::File::open(&config.image_path)?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        hasher.update(&chunk[..count]);
    }
    if hex::encode(hasher.finalize()) != manifest.sha256 {
        return Err(config_error("Workspace image checksum mismatch"));
    }
    match (&config.firmware_path, &manifest.firmware_sha256) {
        (Some(path), Some(expected)) => {
            let mut file = std::fs::File::open(path)?;
            let mut hasher = Sha256::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                hasher.update(&chunk[..n]);
            }
            if hex::encode(hasher.finalize()) != *expected {
                return Err(config_error("VM firmware checksum mismatch"));
            }
        }
        (None, None) if !cfg!(target_arch = "aarch64") => {}
        _ => {
            return Err(config_error(
                "VM firmware must be provided and authenticated by the image manifest; ARM requires UEFI",
            ));
        }
    }
    Ok(())
}
fn qemu_arguments(config: &WorkspaceVmConfig) -> Vec<String> {
    let accelerator = if cfg!(target_os = "macos") {
        "hvf"
    } else if cfg!(target_os = "windows") {
        "whpx"
    } else {
        "kvm"
    };
    let mut args = vec![
        "-accel",
        accelerator,
        "-m",
        "2048",
        "-smp",
        "2",
        "-display",
        "none",
        "-monitor",
        "none",
        "-serial",
        "none",
        "-nic",
        "none",
        "-no-reboot",
        "-snapshot",
        "-device",
        "virtio-serial-pci",
        "-chardev",
        "stdio,id=gm-agent,signal=off",
        "-device",
        "virtserialport,chardev=gm-agent,name=org.grokmuse.agent",
        "-drive",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    // Commas in filenames need escaping in QEMU option dictionaries.
    args.push(format!(
        "file={},format=qcow2,if=virtio,snapshot=on",
        config.image_path.to_string_lossy().replace(',', ",,")
    ));
    if cfg!(target_arch = "aarch64") {
        args.extend([
            "-machine".into(),
            "virt".into(),
            "-cpu".into(),
            "host".into(),
        ]);
    }
    if let Some(path) = &config.firmware_path {
        args.extend(["-bios".into(), path.to_string_lossy().into_owned()]);
    }
    args
}
fn collect_files(
    directory: &cap_std::fs::Dir,
    relative: &Path,
    files: &mut Vec<GuestFile>,
    total: &mut usize,
    entries: &mut usize,
    depth: usize,
) -> Result<()> {
    if depth > 32 {
        return Err(config_error(
            "Workspace directory nesting exceeds 32 levels",
        ));
    }
    for entry in directory.entries()? {
        let entry = entry?;
        *entries += 1;
        if *entries > 10000 {
            return Err(config_error(
                "Workspace import exceeds directory entry limit",
            ));
        }
        let metadata = entry.file_type()?;
        if metadata.is_symlink() {
            return Err(GmError::SafetyViolation(
                "Workspace VM import rejects symbolic links".into(),
            ));
        }
        if entry.file_name() == ".git" {
            continue;
        }
        let path = relative.join(entry.file_name());
        if metadata.is_dir() {
            collect_files(&entry.open_dir()?, &path, files, total, entries, depth + 1)?;
        } else if metadata.is_file() {
            if files.len() >= 1000 {
                return Err(config_error("Workspace VM import exceeds 1000 files"));
            }
            let remaining = 8 * 1024 * 1024 - *total;
            let file = entry.open()?;
            if file.metadata()?.len() > remaining as u64 {
                return Err(config_error("Workspace VM import exceeds 8 MiB"));
            }
            let mut data = Vec::new();
            file.take(remaining as u64 + 1).read_to_end(&mut data)?;
            *total += data.len();
            if *total > 8 * 1024 * 1024 {
                return Err(config_error("Workspace VM import exceeds 8 MiB"));
            }
            let path = path
                .to_str()
                .ok_or_else(|| config_error("Workspace paths must be valid Unicode"))?
                .replace('\\', "/");
            files.push(GuestFile {
                path,
                data: base64::engine::general_purpose::STANDARD.encode(data),
            });
        }
    }
    Ok(())
}

async fn guest_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::other("Incomplete guest protocol frame"))
            };
        }
        let end = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(bytes.len());
        if line.len() + end > 12 * 1024 * 1024 {
            return Err(std::io::Error::other("Guest protocol frame exceeds limit"));
        }
        let finished = bytes[end - 1] == b'\n';
        line.extend_from_slice(&bytes[..end]);
        reader.consume(end);
        if finished {
            line.pop();
            return String::from_utf8(line)
                .map(Some)
                .map_err(|_| std::io::Error::other("Invalid guest protocol UTF-8"));
        }
    }
}

pub async fn execute(
    config: WorkspaceVmConfig,
    workspace: &Path,
    command: &str,
    timeout_ms: u64,
) -> Result<VmOutcome> {
    let verification = config.clone();
    tokio::task::spawn_blocking(move || verified_image(&verification))
        .await
        .map_err(|e| config_error(e.to_string()))??;
    let workspace = workspace.to_path_buf();
    let files = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut total = 0;
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&workspace, cap_std::ambient_authority())?;
        collect_files(&directory, Path::new(""), &mut files, &mut total, &mut 0, 0)?;
        Ok::<_, GmError>(files)
    })
    .await
    .map_err(|e| config_error(e.to_string()))??;
    let request = GuestRequest {
        version: 1,
        command: command.into(),
        timeout_ms,
        files,
    };
    let mut child = Command::new(&config.qemu_path)
        .args(qemu_arguments(&config))
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| config_error("VM input unavailable"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| config_error("VM output unavailable"))?;
    let mut output = BufReader::new(output);
    let ready=tokio::time::timeout(std::time::Duration::from_secs(90),guest_line(&mut output)).await.map_err(|_|GmError::Timeout("Workspace VM did not boot; hardware virtualization and a compatible signed image are required".into()))??;
    if ready.as_deref() != Some("GROK_MUSE_GUEST_V1") {
        return Err(config_error("Workspace guest handshake failed"));
    }
    input.write_all(&serde_json::to_vec(&request)?).await?;
    input.write_all(b"\n").await?;
    input.flush().await?;
    let line = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms + 5000),
        guest_line(&mut output),
    )
    .await
    .map_err(|_| GmError::Timeout("Workspace command exceeded its deadline".into()))??
    .ok_or_else(|| config_error("Workspace VM disconnected"))?;
    if line.len() > 12 * 1024 * 1024 {
        return Err(config_error("Workspace command output exceeds its limit"));
    }
    let reply: GuestReply = serde_json::from_str(&line)?;
    if reply.version != 1 {
        return Err(config_error("Unsupported workspace reply protocol"));
    }
    child.kill().await?;
    let _ = child.wait().await;
    Ok(VmOutcome {
        exit_code: reply.exit_code,
        output: format!("{}{}", reply.stdout, reply.stderr),
        artifacts: decode_artifacts(reply.artifacts)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_has_no_host_mounts_network_or_software_fallback() {
        let config = WorkspaceVmConfig::default();
        let args = qemu_arguments(&config);
        assert!(args.windows(2).any(|a| a == ["-nic", "none"]));
        assert!(args.contains(&"-snapshot".to_string()));
        assert!(
            !args
                .iter()
                .any(|s| s.contains("virtfs") || s.contains("tcg"))
        );
    }
    #[test]
    fn unconfigured_vm_fails_closed() {
        assert!(verified_image(&WorkspaceVmConfig::default()).is_err());
    }
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    #[test]
    fn image_and_manifest_tampering_are_rejected() {
        let directory = std::env::temp_dir().join(format!("gm-image-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let image = b"test fixture, not a runnable VM";
        let firmware = cfg!(target_arch = "aarch64").then_some(b"UEFI test fixture".as_slice());
        let manifest = serde_json::to_vec(&ImageManifest {
            version: 1,
            architecture: std::env::consts::ARCH.into(),
            sha256: hex::encode(Sha256::digest(image)),
            firmware_sha256: firmware.map(|f| hex::encode(Sha256::digest(f))),
        })
        .unwrap();
        let config = WorkspaceVmConfig {
            enabled: true,
            image_path: directory.join("image.qcow2"),
            manifest_path: directory.join("manifest.json"),
            signature_path: directory.join("manifest.sig"),
            trusted_public_key_hex: hex::encode(key.verifying_key().as_bytes()),
            firmware_path: firmware.map(|_| directory.join("uefi.bin")),
            ..Default::default()
        };
        if let Some(firmware) = firmware {
            std::fs::write(config.firmware_path.as_ref().unwrap(), firmware).unwrap();
        }
        std::fs::write(&config.image_path, image).unwrap();
        std::fs::write(&config.manifest_path, &manifest).unwrap();
        std::fs::write(
            &config.signature_path,
            hex::encode(key.sign(&manifest).to_bytes()),
        )
        .unwrap();
        verified_image(&config).unwrap();
        std::fs::write(&config.image_path, b"modified image").unwrap();
        assert!(verified_image(&config).is_err());
        std::fs::write(&config.image_path, image).unwrap();
        std::fs::write(&config.manifest_path, b"modified manifest").unwrap();
        assert!(verified_image(&config).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod export_tests {
    use super::*;
    #[test]
    fn rejects_guest_path_escapes_duplicates_and_oversized_payloads() {
        let file = |path: &str, data: &[u8]| GuestFile {
            path: path.into(),
            data: base64::engine::general_purpose::STANDARD.encode(data),
        };
        let decoded = decode_artifacts(vec![file("patches/change.patch", b"patch")]).unwrap();
        assert_eq!(decoded[0].bytes, b"patch");
        for path in [
            "../escape",
            "/absolute",
            "C:/windows",
            "nested/../file",
            ".ssh/key",
            "a\\b",
            "a//b",
            "a/./b",
        ] {
            assert!(
                decode_artifacts(vec![file(path, b"test")]).is_err(),
                "{path}"
            );
        }
        assert!(decode_artifacts(vec![file("same", b"a"), file("same", b"b")]).is_err());
        assert!(decode_artifacts(vec![file("huge", &vec![0; 1024 * 1024 + 1])]).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn imports_reject_symlink_escapes_and_bounded_reading_rejects_large_files() {
        let root = std::env::temp_dir().join(format!("gm-vm-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape")).unwrap();
        let dir = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        assert!(collect_files(&dir, Path::new(""), &mut vec![], &mut 0, &mut 0, 0).is_err());
        std::fs::remove_file(root.join("escape")).unwrap();
        std::fs::File::create(root.join("large"))
            .unwrap()
            .set_len(9 * 1024 * 1024)
            .unwrap();
        assert!(collect_files(&dir, Path::new(""), &mut vec![], &mut 0, &mut 0, 0).is_err());
        drop(dir);
        std::fs::remove_dir_all(root).unwrap();
    }
}
