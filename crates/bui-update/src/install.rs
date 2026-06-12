//! Download, verify, stage, and apply an update.
//!
//! Staging copies the new binary to `<exe_dir>/.copper.update` so the
//! final swap is two same-filesystem renames (atomic on Unix; the running
//! process keeps its inode, so replacing the path is safe while we run).

use std::path::PathBuf;
use std::process::Command;

use bui_net::Client;
use bui_url::Url;

use crate::github::ReleaseInfo;
use crate::version::Version;
use crate::UpdateError;

/// Name of the staged binary next to the installed one.
const STAGED_NAME: &str = ".copper.update";

#[derive(Debug, Clone)]
pub struct StagedUpdate {
    pub version: Version,
    pub staged_path: PathBuf,
    pub exe_path: PathBuf,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = String::with_capacity(64);
    for b in digest.as_ref() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn io_err(context: &str, e: impl std::fmt::Display) -> UpdateError {
    UpdateError::Io(format!("{context}: {e}"))
}

fn current_exe() -> Result<PathBuf, UpdateError> {
    // canonicalize so a symlinked install (e.g. ~/bin/copper -> ../opt/…)
    // is replaced at its real location, not by clobbering the symlink.
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| io_err("locate current executable", e))
}

/// True if we can create files in the executable's directory.
fn exe_dir_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".copper.update.probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

async fn download(client: &Client, url: &str) -> Result<Vec<u8>, UpdateError> {
    let url = Url::parse(url).map_err(|e| UpdateError::Parse(format!("asset url: {e}")))?;
    let resp = client
        .get(&url)
        .await
        .map_err(|e| UpdateError::Net(e.to_string()))?;
    if resp.status != 200 {
        return Err(UpdateError::Api {
            status: resp.status,
        });
    }
    Ok(resp.body)
}

pub async fn download_and_stage(
    client: &Client,
    info: &ReleaseInfo,
) -> Result<StagedUpdate, UpdateError> {
    let exe_path = current_exe()?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| UpdateError::Io("executable has no parent directory".into()))?
        .to_path_buf();
    if !exe_dir_writable(&exe_dir) {
        return Err(UpdateError::NotWritable);
    }

    let tarball = download(client, &info.asset_url).await?;
    let sha_body = download(client, &info.sha256_url).await?;
    // `shasum -a 256` output: "<hex>  <filename>" — first token is the digest.
    let expected = String::from_utf8_lossy(&sha_body)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if expected.len() != 64 || sha256_hex(&tarball) != expected {
        return Err(UpdateError::ChecksumMismatch);
    }

    let work = std::env::temp_dir().join(format!("copper-update-{}", std::process::id()));
    let result = stage_from_tarball(&tarball, info, &work, &exe_dir, &exe_path);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn stage_from_tarball(
    tarball: &[u8],
    info: &ReleaseInfo,
    work: &std::path::Path,
    exe_dir: &std::path::Path,
    exe_path: &std::path::Path,
) -> Result<StagedUpdate, UpdateError> {
    std::fs::create_dir_all(work).map_err(|e| io_err("create work dir", e))?;
    let tar_path = work.join(&info.asset_name);
    std::fs::write(&tar_path, tarball).map_err(|e| io_err("write tarball", e))?;

    // System tar exists on every release target (CI packs with it too);
    // avoids growing the dependency tree for one extraction.
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tar_path)
        .arg("-C")
        .arg(work)
        .status()
        .map_err(|e| UpdateError::Tar(format!("spawn tar: {e}")))?;
    if !status.success() {
        return Err(UpdateError::Tar(format!("tar exited with {status}")));
    }

    // Archive layout (from release.yml): copper-{ver}-{target}/copper
    let dir_name = info
        .asset_name
        .strip_suffix(".tar.gz")
        .unwrap_or(&info.asset_name);
    let binary = work.join(dir_name).join("copper");
    if !binary.is_file() {
        return Err(UpdateError::Tar(format!(
            "archive missing {dir_name}/copper"
        )));
    }

    let staged_path = exe_dir.join(STAGED_NAME);
    std::fs::copy(&binary, &staged_path).map_err(|e| io_err("stage binary", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| io_err("chmod staged binary", e))?;
    }

    Ok(StagedUpdate {
        version: info.version.clone(),
        staged_path,
        exe_path: exe_path.to_path_buf(),
    })
}

/// Swap the staged binary into place and relaunch. Returns only on
/// failure (success ends in `process::exit`). Every failure path
/// restores the original binary.
pub fn apply_and_relaunch(staged: &StagedUpdate) -> UpdateError {
    let exe = &staged.exe_path;
    let old = exe.with_extension("old");
    if let Err(e) = std::fs::rename(exe, &old) {
        return io_err("move current binary aside", e);
    }
    if let Err(e) = std::fs::rename(&staged.staged_path, exe) {
        let _ = std::fs::rename(&old, exe);
        return io_err("move new binary into place", e);
    }
    match Command::new(exe).args(std::env::args_os().skip(1)).spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            // New binary won't start — put the old one back.
            let _ = std::fs::rename(exe, &staged.staged_path);
            let _ = std::fs::rename(&old, exe);
            io_err("relaunch new binary", e)
        }
    }
}

/// Remove leftovers from a previous update (`<exe>.old`, stale staged
/// binary). Call once early at startup; all failures are ignored.
pub fn cleanup_stale() {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        return;
    };
    let _ = std::fs::remove_file(exe.with_extension("old"));
    if let Some(dir) = exe.parent() {
        let _ = std::fs::remove_file(dir.join(STAGED_NAME));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha_file_token_parsing() {
        let body = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  copper-0.2.0-aarch64-apple-darwin.tar.gz\n";
        let token = body.split_whitespace().next().unwrap();
        assert_eq!(token, sha256_hex(b"abc"));
    }
}
