use serde::Deserialize;

const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES_URL: &str = "https://api.github.com/repos/echoVic/blade-deepseek/releases/latest";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

pub fn check_latest() -> Result<Option<UpdateInfo>, String> {
    let response = reqwest::blocking::Client::new()
        .get(RELEASES_URL)
        .header("User-Agent", "orca-update-check")
        .send()
        .map_err(|error| format!("failed to check latest release: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("release check returned HTTP {}", response.status()));
    }
    let release: GitHubRelease = response
        .json()
        .map_err(|error| format!("invalid release response: {error}"))?;
    let latest = normalize_version(&release.tag_name);
    let current = normalize_version(CRATE_VERSION);
    if latest == current {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        current,
        latest,
        url: release.html_url,
    }))
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_version_strips_v_prefix() {
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
        assert_eq!(normalize_version(" 0.1.0 "), "0.1.0");
    }
}
