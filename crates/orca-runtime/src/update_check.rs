use serde::Deserialize;

const RELEASES_URL: &str = "https://api.github.com/repos/echoVic/blade-deepseek/releases/latest";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

pub fn check_latest(current_version: &str) -> Result<Option<UpdateInfo>, String> {
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
    Ok(update_info_from_release(current_version, release))
}

fn update_info_from_release(current_version: &str, release: GitHubRelease) -> Option<UpdateInfo> {
    let latest = normalize_version(&release.tag_name);
    let current = normalize_version(current_version);
    if !is_newer_version(&latest, &current) {
        return None;
    }
    Some(UpdateInfo {
        current,
        latest,
        url: release.html_url,
    })
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_semver_core(latest), parse_semver_core(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => latest != current,
    }
}

fn parse_semver_core(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .split_once('-')
        .map(|(core, _)| core)
        .unwrap_or(version);
    let core = core.split_once('+').map(|(core, _)| core).unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_version_strips_v_prefix() {
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
        assert_eq!(normalize_version(" 0.1.0 "), "0.1.0");
    }

    #[test]
    fn release_equal_to_current_version_is_not_an_update() {
        let release = GitHubRelease {
            tag_name: "v0.1.6".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.6".to_string(),
        };

        assert_eq!(update_info_from_release("0.1.6", release), None);
    }

    #[test]
    fn release_older_than_current_version_is_not_an_update() {
        let release = GitHubRelease {
            tag_name: "v0.1.6".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.6".to_string(),
        };

        assert_eq!(update_info_from_release("0.1.7", release), None);
    }

    #[test]
    fn release_newer_than_current_version_is_an_update() {
        let release = GitHubRelease {
            tag_name: "v0.1.7".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.7".to_string(),
        };

        assert_eq!(
            update_info_from_release("0.1.6", release),
            Some(UpdateInfo {
                current: "0.1.6".to_string(),
                latest: "0.1.7".to_string(),
                url: "https://example.test/releases/tag/v0.1.7".to_string(),
            })
        );
    }

    #[test]
    fn prerelease_suffix_does_not_force_a_false_update_for_same_core_version() {
        let release = GitHubRelease {
            tag_name: "v0.1.7".to_string(),
            html_url: "https://example.test/releases/tag/v0.1.7".to_string(),
        };

        assert_eq!(update_info_from_release("0.1.7-dev", release), None);
    }
}
