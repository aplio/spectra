use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use self_update::backends::github::{ReleaseList, Update as GithubUpdate};
use self_update::update::Release;
use semver::Version;
use serde::{Deserialize, Serialize};

const REPO_OWNER: &str = "aplio";
const REPO_NAME: &str = "spectra";
const BIN_NAME: &str = "spectra";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCommand {
    Check,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateRequest {
    current_version: String,
    target: String,
    expected_asset_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LatestRelease {
    version: String,
    tag: String,
    asset_name: String,
}

trait UpdateSource {
    fn latest_release(&self, request: &UpdateRequest) -> Result<LatestRelease, String>;
    fn perform_update(&self, request: &UpdateRequest, latest: &LatestRelease)
    -> Result<(), String>;
}

struct GithubUpdateSource;

impl UpdateSource for GithubUpdateSource {
    fn latest_release(&self, request: &UpdateRequest) -> Result<LatestRelease, String> {
        let releases = ReleaseList::configure()
            .repo_owner(REPO_OWNER)
            .repo_name(REPO_NAME)
            .build()
            .map_err(|e| format!("failed to build GitHub release query: {e}"))?
            .fetch()
            .map_err(|e| format!("failed to fetch releases from GitHub: {e}"))?;

        let release = releases
            .into_iter()
            .next()
            .ok_or_else(|| "no releases found in GitHub repository".to_string())?;

        latest_release_from_release(release, request)
    }

    fn perform_update(
        &self,
        request: &UpdateRequest,
        latest: &LatestRelease,
    ) -> Result<(), String> {
        GithubUpdate::configure()
            .repo_owner(REPO_OWNER)
            .repo_name(REPO_NAME)
            .bin_name(BIN_NAME)
            .target(&request.target)
            .target_version_tag(&latest.tag)
            .current_version(&request.current_version)
            .no_confirm(true)
            .show_download_progress(true)
            .build()
            .map_err(|e| format!("failed to configure updater: {e}"))?
            .update()
            .map_err(|e| format!("failed to upgrade binary: {e}"))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockUpdateState {
    UpToDate,
    HasUpdate,
    Error,
}

impl MockUpdateState {
    fn from_env() -> Self {
        match std::env::var("SPECTRA_TEST_UPDATE_STATE") {
            Ok(value) if value.eq_ignore_ascii_case("has_update") => Self::HasUpdate,
            Ok(value) if value.eq_ignore_ascii_case("error") => Self::Error,
            _ => Self::UpToDate,
        }
    }
}

struct MockUpdateSource {
    state: MockUpdateState,
}

impl MockUpdateSource {
    fn from_env() -> Self {
        Self {
            state: MockUpdateState::from_env(),
        }
    }
}

impl UpdateSource for MockUpdateSource {
    fn latest_release(&self, request: &UpdateRequest) -> Result<LatestRelease, String> {
        match self.state {
            MockUpdateState::Error => Err("mock update source failure".to_string()),
            MockUpdateState::UpToDate => Ok(LatestRelease {
                version: request.current_version.clone(),
                tag: format!("v{}", request.current_version),
                asset_name: request.expected_asset_name.clone(),
            }),
            MockUpdateState::HasUpdate => {
                let mut version = parse_semver(&request.current_version)?;
                version.patch += 1;
                Ok(LatestRelease {
                    version: version.to_string(),
                    tag: format!("v{version}"),
                    asset_name: request.expected_asset_name.clone(),
                })
            }
        }
    }

    fn perform_update(
        &self,
        _request: &UpdateRequest,
        _latest: &LatestRelease,
    ) -> Result<(), String> {
        match self.state {
            MockUpdateState::Error => Err("mock upgrade failure".to_string()),
            MockUpdateState::UpToDate | MockUpdateState::HasUpdate => Ok(()),
        }
    }
}

/// How long a cached update-check result stays valid.
pub const UPDATE_CHECK_TTL_SECS: i64 = 24 * 60 * 60;
const UPDATE_CHECK_CACHE_FILE: &str = "update_check.toml";

/// Cached result of a background update check, stored as
/// `update_check.toml` in the spectra data dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckCache {
    pub checked_at_unix: i64,
    pub latest_version: String,
    pub binary_version: String,
}

impl UpdateCheckCache {
    /// Build a cache entry for the running binary from a check result
    /// (`Some(newer)` when an update exists, `None` when up to date).
    pub fn from_check_result(checked_at_unix: i64, newer_version: Option<&str>) -> Self {
        let binary_version = env!("CARGO_PKG_VERSION").to_string();
        Self {
            checked_at_unix,
            latest_version: newer_version
                .map(str::to_string)
                .unwrap_or_else(|| binary_version.clone()),
            binary_version,
        }
    }

    /// Newer-than-binary version recorded in this cache entry, if any.
    pub fn newer_version(&self) -> Option<String> {
        let latest = parse_semver(&self.latest_version).ok()?;
        let binary = parse_semver(&self.binary_version).ok()?;
        (latest > binary).then(|| latest.to_string())
    }

    /// A cache entry is valid only when it was produced by the same binary
    /// version and its TTL has not expired.
    fn is_fresh(&self, now_unix: i64, binary_version: &str) -> bool {
        self.binary_version == binary_version
            && now_unix.saturating_sub(self.checked_at_unix) < UPDATE_CHECK_TTL_SECS
    }
}

pub fn update_check_cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join(UPDATE_CHECK_CACHE_FILE)
}

/// Read the cached update-check result, discarding invalid entries (TTL
/// expired, produced by another binary version, or unreadable file).
pub fn read_fresh_update_cache(data_dir: &Path, now_unix: i64) -> Option<UpdateCheckCache> {
    let content = fs::read_to_string(update_check_cache_path(data_dir)).ok()?;
    let cache: UpdateCheckCache = toml::from_str(&content).ok()?;
    cache
        .is_fresh(now_unix, env!("CARGO_PKG_VERSION"))
        .then_some(cache)
}

pub fn write_update_cache(data_dir: &Path, cache: &UpdateCheckCache) -> io::Result<()> {
    let content = toml::to_string(cache).map_err(io::Error::other)?;
    fs::write(update_check_cache_path(data_dir), content)
}

/// Check for a newer release: `Ok(Some(version))` when an update exists,
/// `Ok(None)` when the running binary is up to date.
pub fn check_latest() -> Result<Option<String>, String> {
    let request = build_request()?;
    if use_mock_update_source() {
        check_latest_with_source(&MockUpdateSource::from_env(), &request)
    } else {
        check_latest_with_source(&GithubUpdateSource, &request)
    }
}

fn check_latest_with_source(
    source: &dyn UpdateSource,
    request: &UpdateRequest,
) -> Result<Option<String>, String> {
    let latest = validated_latest_release(source, request)?;
    let current = parse_semver(&request.current_version)?;
    let newest = parse_semver(&latest.version)?;
    Ok((newest > current).then(|| newest.to_string()))
}

/// Result of a `--check`/`--update` run: the user-facing message plus
/// whether a new binary was actually installed (drives the live-handoff
/// hint when a server is running).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub message: String,
    pub installed: bool,
}

pub fn run(command: UpdateCommand) -> Result<UpdateOutcome, String> {
    let request = build_request()?;
    if use_mock_update_source() {
        let source = MockUpdateSource::from_env();
        run_with_source(&source, command, &request)
    } else {
        let source = GithubUpdateSource;
        run_with_source(&source, command, &request)
    }
}

fn run_with_source(
    source: &dyn UpdateSource,
    command: UpdateCommand,
    request: &UpdateRequest,
) -> Result<UpdateOutcome, String> {
    let latest = validated_latest_release(source, request)?;
    let current = parse_semver(&request.current_version)?;
    let newest = parse_semver(&latest.version)?;
    match command {
        UpdateCommand::Check => {
            let message = if newest > current {
                format!(
                    "Update available: {} -> {} ({}/{})",
                    current,
                    newest,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )
            } else {
                format!(
                    "Already up to date: {} ({}/{})",
                    current,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )
            };
            Ok(UpdateOutcome {
                message,
                installed: false,
            })
        }
        UpdateCommand::Update => {
            if newest <= current {
                return Ok(UpdateOutcome {
                    message: format!(
                        "Already up to date: {} ({}/{})",
                        current,
                        std::env::consts::OS,
                        std::env::consts::ARCH
                    ),
                    installed: false,
                });
            }
            source.perform_update(request, &latest)?;
            Ok(UpdateOutcome {
                message: format!("Upgraded spectra from {} to {}", current, newest),
                installed: true,
            })
        }
    }
}

fn validated_latest_release(
    source: &dyn UpdateSource,
    request: &UpdateRequest,
) -> Result<LatestRelease, String> {
    let latest = source.latest_release(request)?;
    if latest.asset_name != request.expected_asset_name {
        return Err(format!(
            "release asset mismatch for {}: expected {}, got {}",
            request.target, request.expected_asset_name, latest.asset_name
        ));
    }
    Ok(latest)
}

fn use_mock_update_source() -> bool {
    matches!(
        std::env::var("SPECTRA_TEST_UPDATE_SOURCE").as_deref(),
        Ok("mock")
    )
}

fn build_request() -> Result<UpdateRequest, String> {
    let target = resolve_target_triple()?;
    let expected_asset_name = format!("{BIN_NAME}-{target}.tar.gz");
    Ok(UpdateRequest {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        target,
        expected_asset_name,
    })
}

fn resolve_target_triple() -> Result<String, String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("macos", "aarch64") => "macos-arm64",
        (os, arch) => {
            return Err(format!(
                "unsupported platform for update: {os}/{arch} (supported: linux x86_64 or macos aarch64)"
            ));
        }
    };
    Ok(target.to_string())
}

fn latest_release_from_release(
    release: Release,
    request: &UpdateRequest,
) -> Result<LatestRelease, String> {
    let asset = release.asset_for(&request.target, None).ok_or_else(|| {
        format!(
            "latest release does not include an asset for target {}",
            request.target
        )
    })?;
    Ok(LatestRelease {
        version: normalize_version_string(&release.version),
        tag: release.version,
        asset_name: asset.name,
    })
}

fn normalize_version_string(version: &str) -> String {
    version.trim_start_matches('v').to_string()
}

fn parse_semver(value: &str) -> Result<Version, String> {
    Version::parse(value.trim_start_matches('v'))
        .map_err(|e| format!("invalid version '{value}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::{
        MockUpdateSource, MockUpdateState, UPDATE_CHECK_TTL_SECS, UpdateCheckCache, UpdateRequest,
        check_latest_with_source, parse_semver, read_fresh_update_cache, resolve_target_triple,
        write_update_cache,
    };

    fn request_for_tests() -> UpdateRequest {
        UpdateRequest {
            current_version: "0.1.5".to_string(),
            target: "linux-x86_64".to_string(),
            expected_asset_name: "spectra-linux-x86_64.tar.gz".to_string(),
        }
    }

    #[test]
    fn check_latest_reports_newer_version_from_injected_source() {
        let source = MockUpdateSource {
            state: MockUpdateState::HasUpdate,
        };
        assert_eq!(
            check_latest_with_source(&source, &request_for_tests()),
            Ok(Some("0.1.6".to_string()))
        );
    }

    #[test]
    fn check_latest_reports_none_when_up_to_date() {
        let source = MockUpdateSource {
            state: MockUpdateState::UpToDate,
        };
        assert_eq!(
            check_latest_with_source(&source, &request_for_tests()),
            Ok(None)
        );
    }

    #[test]
    fn check_latest_propagates_source_errors() {
        let source = MockUpdateSource {
            state: MockUpdateState::Error,
        };
        let err = check_latest_with_source(&source, &request_for_tests())
            .expect_err("mock error state fails");
        assert!(err.contains("mock update source failure"), "got: {err}");
    }

    #[test]
    fn cache_is_fresh_within_ttl_for_same_binary() {
        let cache = UpdateCheckCache {
            checked_at_unix: 1_000,
            latest_version: "0.2.0".to_string(),
            binary_version: "0.1.5".to_string(),
        };
        assert!(cache.is_fresh(1_000 + UPDATE_CHECK_TTL_SECS - 1, "0.1.5"));
    }

    #[test]
    fn cache_expires_after_ttl() {
        let cache = UpdateCheckCache {
            checked_at_unix: 1_000,
            latest_version: "0.2.0".to_string(),
            binary_version: "0.1.5".to_string(),
        };
        assert!(!cache.is_fresh(1_000 + UPDATE_CHECK_TTL_SECS, "0.1.5"));
    }

    #[test]
    fn cache_is_invalidated_by_binary_version_change() {
        let cache = UpdateCheckCache {
            checked_at_unix: 1_000,
            latest_version: "0.2.0".to_string(),
            binary_version: "0.1.4".to_string(),
        };
        assert!(!cache.is_fresh(1_001, "0.1.5"));
    }

    #[test]
    fn newer_version_is_derived_from_cache_versions() {
        let with_update = UpdateCheckCache::from_check_result(1_000, Some("99.0.0"));
        assert_eq!(with_update.newer_version(), Some("99.0.0".to_string()));
        let up_to_date = UpdateCheckCache::from_check_result(1_000, None);
        assert_eq!(up_to_date.newer_version(), None);
    }

    #[test]
    fn update_cache_roundtrips_through_toml_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = UpdateCheckCache::from_check_result(1_000, Some("99.0.0"));
        write_update_cache(dir.path(), &cache).expect("write cache");
        assert_eq!(read_fresh_update_cache(dir.path(), 1_001), Some(cache));
    }

    #[test]
    fn missing_or_garbage_cache_file_reads_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_fresh_update_cache(dir.path(), 1_000), None);
        std::fs::write(dir.path().join("update_check.toml"), "not = [valid").expect("write");
        assert_eq!(read_fresh_update_cache(dir.path(), 1_000), None);
    }

    #[test]
    fn expired_cache_file_reads_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = UpdateCheckCache::from_check_result(1_000, Some("99.0.0"));
        write_update_cache(dir.path(), &cache).expect("write cache");
        assert_eq!(
            read_fresh_update_cache(dir.path(), 1_000 + UPDATE_CHECK_TTL_SECS),
            None
        );
    }

    #[test]
    fn semver_parser_accepts_with_or_without_v() {
        assert_eq!(
            parse_semver("0.1.12").expect("parse"),
            semver::Version::new(0, 1, 12)
        );
        assert_eq!(
            parse_semver("v0.1.12").expect("parse"),
            semver::Version::new(0, 1, 12)
        );
    }

    #[test]
    fn target_triple_matches_supported_platforms() {
        let target = resolve_target_triple().expect("resolve platform");
        let valid = matches!(target.as_str(), "linux-x86_64" | "macos-arm64");
        assert!(valid, "unexpected target: {target}");
    }
}
