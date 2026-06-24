use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const RELEASES_URL: &str = "https://api.github.com/repos/echoVic/blade-deepseek/releases/latest";
const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/@blade-ai/orca/latest";
const ORCA_HOME_ENV: &str = "ORCA_HOME";
const UPDATE_CACHE_FILE: &str = "update-cache.json";

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

#[derive(Clone, Debug, Deserialize)]
struct NpmPackage {
    version: String,
}

pub fn check_latest(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    match check_latest_npm(current_version) {
        Ok(result) => Ok(result),
        Err(_) => check_latest_github(current_version),
    }
}

fn check_latest_npm(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let response = reqwest::blocking::Client::new()
        .get(NPM_REGISTRY_URL)
        .header("User-Agent", "orca-update-check")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .map_err(|error| format!("npm registry check failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("npm registry returned HTTP {}", response.status()));
    }
    let pkg: NpmPackage = response
        .json()
        .map_err(|error| format!("invalid npm registry response: {error}"))?;
    let latest = normalize_version(&pkg.version);
    let current = normalize_version(current_version);
    if !is_newer_version(&latest, &current) {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        current,
        url: format!("https://github.com/echoVic/blade-deepseek/releases/tag/v{latest}"),
        latest,
    }))
}

fn check_latest_github(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let response = reqwest::blocking::Client::new()
        .get(RELEASES_URL)
        .header("User-Agent", "orca-update-check")
        .timeout(std::time::Duration::from_secs(5))
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

pub fn check_latest_for_prompt(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    let Some(info) = check_latest(current_version)? else {
        return Ok(None);
    };
    if should_prompt_for_update(&info, read_update_cache().skip_until_version.as_deref()) {
        Ok(Some(info))
    } else {
        Ok(None)
    }
}

pub fn dismiss_version(version: &str) -> Result<(), String> {
    write_update_cache(&UpdatePromptCache {
        skip_until_version: Some(normalize_version(version)),
    })
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

fn should_prompt_for_update(info: &UpdateInfo, skip_until_version: Option<&str>) -> bool {
    match skip_until_version {
        Some(skipped) => is_newer_version(&info.latest, &normalize_version(skipped)),
        None => true,
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct UpdatePromptCache {
    skip_until_version: Option<String>,
}

fn read_update_cache() -> UpdatePromptCache {
    let Some(path) = update_cache_path() else {
        return UpdatePromptCache::default();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return UpdatePromptCache::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

fn write_update_cache(cache: &UpdatePromptCache) -> Result<(), String> {
    let Some(path) = update_cache_path() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create update cache directory: {error}"))?;
    }
    let contents = serde_json::to_string_pretty(cache)
        .map_err(|error| format!("failed to serialize update cache: {error}"))?;
    fs::write(path, format!("{contents}\n"))
        .map_err(|error| format!("failed to write update cache: {error}"))
}

fn update_cache_path() -> Option<PathBuf> {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join(UPDATE_CACHE_FILE))
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

    #[test]
    fn skipped_version_suppresses_prompt_until_newer_release_exists() {
        let info = UpdateInfo {
            current: "0.1.7".to_string(),
            latest: "0.1.8".to_string(),
            url: "https://example.test/releases/tag/v0.1.8".to_string(),
        };

        assert!(!should_prompt_for_update(&info, Some("0.1.8")));
        assert!(!should_prompt_for_update(&info, Some("0.1.9")));
        assert!(should_prompt_for_update(&info, Some("0.1.7")));
        assert!(should_prompt_for_update(&info, None));
    }
}
