//! GitHub releases API: fetch `/releases/latest` and pick the asset that
//! matches this build's target triple.
//!
//! `/releases/latest` excludes prereleases and drafts, which gives
//! stable-only update semantics for free (release.yml marks tags
//! containing `-` as prereleases).

use bui_net::{Client, Request};
use bui_url::Url;

use crate::json::Json;
use crate::version::Version;
use crate::UpdateError;

pub const DEFAULT_API_URL: &str =
    "https://api.github.com/repos/florekio/Copper/releases/latest";

/// Target triples release.yml publishes assets for. Other platforms get
/// an empty string, which disables the updater entirely.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const TARGET: &str = "x86_64-apple-darwin";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const TARGET: &str = "x86_64-unknown-linux-gnu";
#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
)))]
pub const TARGET: &str = "";

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: Version,
    /// Asset file name, e.g. `copper-0.2.0-aarch64-apple-darwin.tar.gz`.
    /// The tarball contains a directory of the same name minus `.tar.gz`.
    pub asset_name: String,
    pub asset_url: String,
    pub sha256_url: String,
    pub html_url: String,
}

pub async fn fetch_latest(client: &Client, api_url: &str) -> Result<Json, UpdateError> {
    let url = Url::parse(api_url).map_err(|e| UpdateError::Parse(format!("api url: {e}")))?;
    let req = Request::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    let resp = client
        .send(req)
        .await
        .map_err(|e| UpdateError::Net(e.to_string()))?;
    match resp.status {
        200 => {}
        404 => return Err(UpdateError::NoRelease),
        status => return Err(UpdateError::Api { status }),
    }
    crate::json::parse(&resp.body_str()).map_err(UpdateError::Parse)
}

pub fn parse_release(doc: &Json, target: &str) -> Result<ReleaseInfo, UpdateError> {
    let tag = doc
        .get("tag_name")
        .and_then(Json::as_str)
        .ok_or_else(|| UpdateError::Parse("missing tag_name".into()))?
        .to_string();
    let version = Version::parse(&tag)
        .ok_or_else(|| UpdateError::Parse(format!("unparseable tag '{tag}'")))?;
    let html_url = doc
        .get("html_url")
        .and_then(Json::as_str)
        .unwrap_or("https://github.com/florekio/Copper/releases")
        .to_string();

    let asset_name = format!("copper-{version}-{target}.tar.gz");
    let sha_name = format!("{asset_name}.sha256");
    let mut asset_url = None;
    let mut sha256_url = None;
    for asset in doc
        .get("assets")
        .and_then(Json::as_arr)
        .unwrap_or_default()
    {
        let name = asset.get("name").and_then(Json::as_str);
        let url = asset.get("browser_download_url").and_then(Json::as_str);
        match (name, url) {
            (Some(n), Some(u)) if n == asset_name => asset_url = Some(u.to_string()),
            (Some(n), Some(u)) if n == sha_name => sha256_url = Some(u.to_string()),
            _ => {}
        }
    }
    let asset_url = asset_url.ok_or_else(|| UpdateError::AssetMissing(asset_name.clone()))?;
    let sha256_url = sha256_url.ok_or_else(|| UpdateError::AssetMissing(sha_name))?;

    Ok(ReleaseInfo {
        tag,
        version,
        asset_name,
        asset_url,
        sha256_url,
        html_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "tag_name": "v0.2.0",
      "html_url": "https://github.com/florekio/Copper/releases/tag/v0.2.0",
      "assets": [
        {"name": "copper-0.2.0-x86_64-apple-darwin.tar.gz", "browser_download_url": "https://dl/x86.tar.gz"},
        {"name": "copper-0.2.0-x86_64-apple-darwin.tar.gz.sha256", "browser_download_url": "https://dl/x86.sha256"},
        {"name": "copper-0.2.0-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://dl/arm.tar.gz"},
        {"name": "copper-0.2.0-aarch64-apple-darwin.tar.gz.sha256", "browser_download_url": "https://dl/arm.sha256"}
      ]
    }"#;

    #[test]
    fn selects_asset_for_target() {
        let doc = crate::json::parse(FIXTURE).unwrap();
        let info = parse_release(&doc, "aarch64-apple-darwin").unwrap();
        assert_eq!(info.tag, "v0.2.0");
        assert_eq!(info.version.to_string(), "0.2.0");
        assert_eq!(info.asset_name, "copper-0.2.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(info.asset_url, "https://dl/arm.tar.gz");
        assert_eq!(info.sha256_url, "https://dl/arm.sha256");

        let info = parse_release(&doc, "x86_64-apple-darwin").unwrap();
        assert_eq!(info.asset_url, "https://dl/x86.tar.gz");
    }

    #[test]
    fn missing_target_asset_errors() {
        let doc = crate::json::parse(FIXTURE).unwrap();
        let err = parse_release(&doc, "x86_64-unknown-linux-gnu").unwrap_err();
        assert!(matches!(err, UpdateError::AssetMissing(_)));
    }
}
