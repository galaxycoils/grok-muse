//! Explicit human review/apply boundary for guest-produced Git patches.
use gm_core::{GmError, Result};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

fn patch_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata = path.symlink_metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        return Err(GmError::Config(
            "Patch must be a regular file under 1 MiB".into(),
        ));
    }
    let mut bytes = vec![];
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > 1024 * 1024 {
        return Err(GmError::Config("Patch is empty or exceeds 1 MiB".into()));
    }
    Ok(bytes)
}

fn git(repository: &Path) -> Command {
    let mut command = Command::new("git");
    // Disable repository-provided hooks, filesystem monitors, pagers and config
    // environment injection. This operation never invokes a shell or network.
    command.env_clear();
    for name in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "TMP",
        "TEMP",
        "HOME",
        "USERPROFILE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("GIT_CONFIG_NOSYSTEM", "1").env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    command.arg("-C").arg(repository).args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=",
        "--no-pager",
    ]);
    command
}

/// Without the exact reviewed digest, return a preview only. Apply consumes
/// those same in-memory bytes through stdin, so replacing the patch cannot
/// substitute unreviewed content between digest verification and application.
pub fn review_patch(
    repository: &Path,
    patch: &Path,
    approved_sha256: Option<&str>,
) -> Result<String> {
    let repository = repository.canonicalize()?;
    let top = git(&repository)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !top.status.success() {
        return Err(GmError::Config("Target is not a Git worktree".into()));
    }
    let top =
        std::path::PathBuf::from(String::from_utf8_lossy(&top.stdout).trim()).canonicalize()?;
    if top != repository {
        return Err(GmError::Config("Select the exact Git worktree root".into()));
    }
    let bytes = patch_bytes(patch)?;
    let digest = gm_core::compute_sha256(&bytes);
    if approved_sha256.is_none() {
        return Ok(format!(
            "Patch SHA-256: {digest}\nNo changes applied. Review this patch, then repeat with --approve-sha256 {digest}.\n{}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    if approved_sha256 != Some(digest.as_str()) {
        return Err(GmError::SafetyViolation(
            "Patch digest differs from the approved content; nothing applied".into(),
        ));
    }
    for check in [true, false] {
        let mut command = git(&repository);
        command.arg("apply");
        if check {
            command.arg("--check");
        }
        command
            .arg("--whitespace=error")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| GmError::Platform("Git stdin unavailable".into()))?
            .write_all(&bytes)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(GmError::Platform(format!(
                "Patch {} failed: {}",
                if check { "validation" } else { "application" },
                String::from_utf8_lossy(&output.stderr)
            )));
        }
    }
    Ok(format!(
        "Applied reviewed patch {digest}. Review Git diff and run project checks before committing."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_exact_approval_and_rejects_conflicts_and_escapes() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg(&repository)
                .output()
                .unwrap()
                .status
                .success()
        );
        std::fs::write(repository.join("file.txt"), "old\n").unwrap();
        let patch = root.path().join("change.patch");
        let bytes=b"diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
        std::fs::write(&patch, bytes).unwrap();
        assert!(
            review_patch(&repository, &patch, None)
                .unwrap()
                .contains("No changes applied")
        );
        assert!(review_patch(&repository, &patch, Some("wrong")).is_err());
        assert_eq!(
            std::fs::read_to_string(repository.join("file.txt")).unwrap(),
            "old\n"
        );
        let digest = gm_core::compute_sha256(bytes);
        assert!(
            review_patch(&repository, &patch, Some(&digest))
                .unwrap()
                .starts_with("Applied")
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("file.txt")).unwrap(),
            "new\n"
        );
        assert!(review_patch(&repository, &patch, Some(&digest)).is_err());
        let escape=b"diff --git a/../outside b/../outside\n--- /dev/null\n+++ b/../outside\n@@ -0,0 +1 @@\n+bad\n";
        std::fs::write(&patch, escape).unwrap();
        assert!(review_patch(&repository, &patch, Some(&gm_core::compute_sha256(escape))).is_err());
        assert!(!root.path().join("outside").exists());
    }
}
