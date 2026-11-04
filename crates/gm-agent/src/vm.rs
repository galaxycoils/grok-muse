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
}
#[derive(Serialize)]
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
    args
}
fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<GuestFile>,
    total: &mut usize,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if metadata.is_symlink() {
            return Err(GmError::SafetyViolation(
                "Workspace VM import rejects symbolic links".into(),
            ));
        }
        if entry.file_name() == ".git" {
            continue;
        }
        if metadata.is_dir() {
            collect_files(root, &entry.path(), files, total)?;
        } else if metadata.is_file() {
            if files.len() >= 1000 {
                return Err(config_error("Workspace VM import exceeds 1000 files"));
            }
            let data = std::fs::read(entry.path())?;
            *total = total.saturating_add(data.len());
            if *total > 8 * 1024 * 1024 {
                return Err(config_error("Workspace VM import exceeds 8 MiB"));
            }
            let path = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| config_error("Invalid workspace import path"))?
                .to_string_lossy()
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
        if line.len() + end > 2 * 1024 * 1024 {
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
) -> Result<(i32, String)> {
    let verification = config.clone();
    tokio::task::spawn_blocking(move || verified_image(&verification))
        .await
        .map_err(|e| config_error(e.to_string()))??;
    let workspace = workspace.to_path_buf();
    let files = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut total = 0;
        collect_files(&workspace, &workspace, &mut files, &mut total)?;
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
    if line.len() > 2 * 1024 * 1024 {
        return Err(config_error("Workspace command output exceeds its limit"));
    }
    let reply: GuestReply = serde_json::from_str(&line)?;
    if reply.version != 1 {
        return Err(config_error("Unsupported workspace reply protocol"));
    }
    child.kill().await?;
    let _ = child.wait().await;
    Ok((reply.exit_code, format!("{}{}", reply.stdout, reply.stderr)))
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
        let manifest = serde_json::to_vec(&ImageManifest {
            version: 1,
            architecture: std::env::consts::ARCH.into(),
            sha256: hex::encode(Sha256::digest(image)),
        })
        .unwrap();
        let config = WorkspaceVmConfig {
            enabled: true,
            image_path: directory.join("image.qcow2"),
            manifest_path: directory.join("manifest.json"),
            signature_path: directory.join("manifest.sig"),
            trusted_public_key_hex: hex::encode(key.verifying_key().as_bytes()),
            ..Default::default()
        };
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
