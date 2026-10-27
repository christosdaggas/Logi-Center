use std::io::Read;
use std::process::{Command, Stdio};

use serde::Deserialize;

const API_URL: &str = "https://api.github.com/repos/christosdaggas/Logi-Center/releases/latest";
const RELEASE_PREFIX: &str = "https://github.com/christosdaggas/Logi-Center/releases/tag/";
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Debug)]
pub struct UpdateInfo {
    pub latest_version: String,
    pub download_url: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

// Same startup-only GitHub release check as Network Manager. curl handles TLS
// in a short-lived worker process, avoiding a permanent HTTP/Tokio runtime.
pub fn check_for_update(current_version: &str) -> Option<UpdateInfo> {
    let mut child = Command::new("curl")
        .args([
            "--disable",
            "--silent",
            "--fail",
            "--proto",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "--max-filesize",
            "524288",
            "--user-agent",
            &format!("Logi-Center/{current_version}"),
            "--header",
            "Accept: application/vnd.github+json",
            API_URL,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut body = Vec::new();
    let read = child
        .stdout
        .take()?
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body);
    if read.is_err() || body.len() > MAX_RESPONSE_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    if !child.wait().ok()?.success() {
        return None;
    }
    parse_release(&body, current_version)
}

fn parse_release(body: &[u8], current_version: &str) -> Option<UpdateInfo> {
    if body.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    let release: GitHubRelease = serde_json::from_slice(body).ok()?;
    let latest = release.tag_name.trim_start_matches(['v', 'V']);
    if release.draft
        || release.prerelease
        || !release.html_url.starts_with(RELEASE_PREFIX)
        || !is_newer(latest, current_version)
    {
        return None;
    }
    Some(UpdateInfo {
        latest_version: latest.to_owned(),
        download_url: release.html_url,
    })
}

fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |text: &str| -> Option<Vec<u64>> {
        text.split('.').map(|part| part.parse().ok()).collect()
    };
    let (Some(mut remote), Some(mut local)) = (parse(remote), parse(local)) else {
        return false;
    };
    let length = remote.len().max(local.len());
    remote.resize(length, 0);
    local.resize(length, 0);
    remote > local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        for remote in ["1.0.1", "1.1.0", "2.0.0", "1.0.0.1"] {
            assert!(is_newer(remote, "1.0.0"));
        }
        for remote in ["1.0", "1.0.0", "0.9.0", "invalid", "1.1.0-beta"] {
            assert!(!is_newer(remote, "1.0.0"));
        }
    }

    #[test]
    fn release_validation() {
        let valid = br#"{"tag_name":"v1.1.0","html_url":"https://github.com/christosdaggas/Logi-Center/releases/tag/v1.1.0"}"#;
        let update = parse_release(valid, "1.0.0").unwrap();
        assert_eq!(update.latest_version, "1.1.0");
        assert!(parse_release(valid, "1.1.0").is_none());
        assert!(parse_release(b"not json", "1.0.0").is_none());
        assert!(parse_release(&vec![b'x'; MAX_RESPONSE_BYTES + 1], "1.0.0").is_none());
        for extra in [",\"draft\":true", ",\"prerelease\":true"] {
            let mut value = String::from_utf8(valid.to_vec()).unwrap();
            value.pop();
            value.push_str(extra);
            value.push('}');
            assert!(parse_release(value.as_bytes(), "1.0.0").is_none());
        }
        let unsafe_url = String::from_utf8(valid.to_vec())
            .unwrap()
            .replace(RELEASE_PREFIX, "https://example.com/");
        assert!(parse_release(unsafe_url.as_bytes(), "1.0.0").is_none());
    }
}
