use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use tokio::time;

use super::{
    model::{
        PlatformObservation, ReleaseAvailability, UpgradeObserved, UpgradeState, UpgradeTarget,
    },
    update_cache::{CacheRead, CachedRelease, UpdateCache, UpdateMetadata},
};

pub const BARE_REFRESH_BUDGET: Duration = Duration::from_millis(800);
pub const TARGET_CHECK_BUDGET: Duration = Duration::from_secs(10);

type SourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReleaseEvidence, ReleaseSourceError>> + Send + 'a>>;

/// The release evidence needed to decide whether one managed artifact is newer
/// and whether the convention-named archive exists for this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseEvidence {
    pub version: String,
    pub assets: BTreeSet<String>,
}

pub trait ReleaseSource: Send + Sync {
    fn fetch<'a>(&'a self, target: UpgradeTarget) -> SourceFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseSourceError {
    Offline(String),
    RateLimited,
    InvalidResponse(String),
}

impl fmt::Display for ReleaseSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline(reason) => write!(formatter, "release source unavailable: {reason}"),
            Self::RateLimited => formatter.write_str("release source rate limited the check"),
            Self::InvalidResponse(reason) => {
                write!(formatter, "invalid release response: {reason}")
            }
        }
    }
}

impl Error for ReleaseSourceError {}

/// The explicit `upgrade --check` error surface. The expiry variant is separate
/// so callers can report exactly which release target consumed its full budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateCheckError {
    ExpiredTarget {
        target: UpgradeTarget,
    },
    Source {
        target: UpgradeTarget,
        source: ReleaseSourceError,
    },
    CacheWrite(String),
}

impl fmt::Display for UpdateCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExpiredTarget { target } => write!(
                formatter,
                "update check for {target} expired after {} seconds",
                TARGET_CHECK_BUDGET.as_secs()
            ),
            Self::Source { target, source } => {
                write!(formatter, "update check for {target} failed: {source}")
            }
            Self::CacheWrite(reason) => {
                write!(formatter, "could not save update metadata: {reason}")
            }
        }
    }
}

impl Error for UpdateCheckError {}

/// A GitHub latest-release source. The base URL override exists so tests and
/// air-gapped release mirrors can provide the same `/repos/<owner>/<repo>` API
/// shape without changing the daemon or its network policy.
pub struct GitHubReleaseSource {
    client: reqwest::Client,
    api_base: String,
}

impl GitHubReleaseSource {
    pub fn from_environment() -> Result<Self, ReleaseSourceError> {
        let api_base = std::env::var("CK_UPDATE_SOURCE_BASE_URL")
            .unwrap_or_else(|_| "https://api.github.com".to_string());
        let client = reqwest::Client::builder()
            .user_agent("ck-update-check")
            .build()
            .map_err(|error| ReleaseSourceError::Offline(error.to_string()))?;
        Ok(Self {
            client,
            api_base: api_base.trim_end_matches('/').to_string(),
        })
    }

    fn release_url(&self, target: UpgradeTarget) -> String {
        let repository = match target {
            UpgradeTarget::Aft => "cortexkit/aft",
            UpgradeTarget::SubcMcp | UpgradeTarget::Daemon | UpgradeTarget::Ck => {
                "cortexkit/subconscious"
            }
        };
        format!("{}/repos/{repository}/releases/latest", self.api_base)
    }
}

impl ReleaseSource for GitHubReleaseSource {
    fn fetch<'a>(&'a self, target: UpgradeTarget) -> SourceFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(self.release_url(target))
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .send()
                .await
                .map_err(|error| ReleaseSourceError::Offline(error.to_string()))?;
            let status = response.status();
            let exhausted = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok())
                == Some("0");
            if status.as_u16() == 429 || exhausted {
                return Err(ReleaseSourceError::RateLimited);
            }
            if !status.is_success() {
                return Err(ReleaseSourceError::Offline(format!("HTTP {status}")));
            }
            let release = response
                .json::<LatestRelease>()
                .await
                .map_err(|error| ReleaseSourceError::InvalidResponse(error.to_string()))?;
            let version = normalize_release_version(&release.tag_name).ok_or_else(|| {
                ReleaseSourceError::InvalidResponse("latest release tag has no version".to_string())
            })?;
            Ok(ReleaseEvidence {
                version,
                assets: release.assets.into_iter().map(|asset| asset.name).collect(),
            })
        })
    }
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<LatestAsset>,
}

#[derive(Deserialize)]
struct LatestAsset {
    name: String,
}

/// The state shown by bare `ck`. A failed refresh deliberately does not turn a
/// stale observation into an update claim, even if that stale cache contained a
/// newer version before the release source became unavailable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DashboardUpdate {
    Current {
        cache_age: Duration,
    },
    Available {
        updates: Vec<(UpgradeTarget, String)>,
        cache_age: Duration,
    },
    NotChecked {
        cache_age: Option<Duration>,
    },
}

impl DashboardUpdate {
    pub fn render(&self) -> String {
        match self {
            Self::Current { cache_age } => {
                format!(
                    "updates: current (cache {} old)",
                    format_cache_age(*cache_age)
                )
            }
            Self::Available { updates, cache_age } => {
                let updates = updates
                    .iter()
                    .map(|(target, version)| format!("{target} {version}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "updates: {updates} (cache {} old)",
                    format_cache_age(*cache_age)
                )
            }
            Self::NotChecked {
                cache_age: Some(cache_age),
            } => format!(
                "updates: not checked (cache {} old)",
                format_cache_age(*cache_age)
            ),
            Self::NotChecked { cache_age: None } => {
                "updates: not checked (cache unavailable)".to_string()
            }
        }
    }
}

/// Preserves a cached age in the fallback rendered when the release client
/// cannot even be constructed before a bare dashboard refresh begins.
pub fn not_checked_from_cache(cache: &UpdateCache) -> DashboardUpdate {
    let now_unix_secs = unix_now_secs();
    let cache_age = match cache.load() {
        CacheRead::Present(metadata) => Some(metadata.age_at(now_unix_secs)),
        CacheRead::Absent | CacheRead::Malformed | CacheRead::Unreadable(_) => None,
    };
    DashboardUpdate::NotChecked { cache_age }
}

/// Reads fresh metadata without a request; otherwise attempts one complete
/// refresh within the shared bare-ck budget. Any failure returns normally with
/// the stale `not checked` form rather than blocking dashboard completion.
pub async fn dashboard_update<S: ReleaseSource>(
    cache: &UpdateCache,
    source: &S,
    installed_versions: &BTreeMap<String, String>,
) -> DashboardUpdate {
    dashboard_update_at(
        cache,
        source,
        installed_versions,
        unix_now_secs(),
        BARE_REFRESH_BUDGET,
    )
    .await
}

pub async fn dashboard_update_at<S: ReleaseSource>(
    cache: &UpdateCache,
    source: &S,
    installed_versions: &BTreeMap<String, String>,
    now_unix_secs: u64,
    budget: Duration,
) -> DashboardUpdate {
    let cached = match cache.load() {
        CacheRead::Present(metadata) => Some(metadata),
        CacheRead::Absent | CacheRead::Malformed | CacheRead::Unreadable(_) => None,
    };
    if let Some(metadata) = &cached {
        if metadata.is_fresh_at(now_unix_secs) {
            return dashboard_state(metadata, installed_versions, now_unix_secs);
        }
    }

    let refreshed = time::timeout(budget, refresh_all(source, now_unix_secs)).await;
    let Ok(Ok(metadata)) = refreshed else {
        return DashboardUpdate::NotChecked {
            cache_age: cached.map(|metadata| metadata.age_at(now_unix_secs)),
        };
    };
    if cache.write(&metadata).is_err() {
        return DashboardUpdate::NotChecked {
            cache_age: cached.map(|metadata| metadata.age_at(now_unix_secs)),
        };
    }
    dashboard_state(&metadata, installed_versions, now_unix_secs)
}

/// Refreshes every target using a separate ten-second deadline. This is only
/// called by `ck upgrade --check`; bare `ck` instead uses the shared 800 ms
/// deadline above and never acquires a per-target wait.
pub async fn check_update_metadata<S: ReleaseSource>(
    cache: &UpdateCache,
    source: &S,
) -> Result<UpdateMetadata, UpdateCheckError> {
    let now_unix_secs = unix_now_secs();
    let mut targets = BTreeMap::new();
    for target in UpgradeTarget::ORDERED {
        let evidence = match time::timeout(TARGET_CHECK_BUDGET, source.fetch(target)).await {
            Ok(Ok(evidence)) => evidence,
            Ok(Err(source)) => return Err(UpdateCheckError::Source { target, source }),
            Err(_) => return Err(UpdateCheckError::ExpiredTarget { target }),
        };
        targets.insert(
            target.label().to_string(),
            CachedRelease {
                version: evidence.version,
                assets: evidence.assets.into_iter().collect(),
            },
        );
    }
    let metadata = UpdateMetadata {
        checked_at_unix_secs: now_unix_secs,
        targets,
    };
    cache
        .write(&metadata)
        .map_err(UpdateCheckError::CacheWrite)?;
    Ok(metadata)
}

/// The current alpha planner remains independent of process discovery. Until
/// the execution backend owns each binary probe, all managed artifacts share
/// this build's version as their installed-version observation.
pub fn compiled_installed_versions() -> BTreeMap<String, String> {
    UpgradeTarget::ORDERED
        .into_iter()
        .map(|target| {
            (
                target.label().to_string(),
                env!("CARGO_PKG_VERSION").to_string(),
            )
        })
        .collect()
}

/// Applies release evidence to the existing upgrade planner's explicit state,
/// preserving its ordering and release-incomplete refusal behavior.
pub fn observed_from_metadata(
    metadata: &UpdateMetadata,
    installed_versions: &BTreeMap<String, String>,
) -> UpgradeObserved {
    let mut observed = UpgradeObserved::no_updates_on_current_host();
    let platform = observed.platform.clone();
    for target in UpgradeTarget::ORDERED {
        let Some(release) = metadata.targets.get(target.label()) else {
            continue;
        };
        if let Some(expected_asset) = expected_asset_name(target, &platform) {
            if !release.assets.iter().any(|asset| asset == &expected_asset) {
                observed.releases.insert(
                    target.label().to_string(),
                    ReleaseAvailability::Incomplete {
                        missing_asset: expected_asset,
                    },
                );
            }
        }
        let installed = installed_versions
            .get(target.label())
            .map(String::as_str)
            .unwrap_or(env!("CARGO_PKG_VERSION"));
        if is_newer_version(&release.version, installed) {
            observed.targets.insert(
                target.label().to_string(),
                UpgradeState::UpdateAvailable {
                    from: installed.to_string(),
                    to: release.version.clone(),
                },
            );
        }
    }
    observed
}

async fn refresh_all<S: ReleaseSource>(
    source: &S,
    now_unix_secs: u64,
) -> Result<UpdateMetadata, ReleaseSourceError> {
    let mut targets = BTreeMap::new();
    for target in UpgradeTarget::ORDERED {
        let evidence = source.fetch(target).await?;
        targets.insert(
            target.label().to_string(),
            CachedRelease {
                version: evidence.version,
                assets: evidence.assets.into_iter().collect(),
            },
        );
    }
    Ok(UpdateMetadata {
        checked_at_unix_secs: now_unix_secs,
        targets,
    })
}

fn dashboard_state(
    metadata: &UpdateMetadata,
    installed_versions: &BTreeMap<String, String>,
    now_unix_secs: u64,
) -> DashboardUpdate {
    let updates = UpgradeTarget::ORDERED
        .into_iter()
        .filter_map(|target| {
            let release = metadata.targets.get(target.label())?;
            let installed = installed_versions
                .get(target.label())
                .map(String::as_str)
                .unwrap_or(env!("CARGO_PKG_VERSION"));
            let expected_asset = expected_asset_name(target, &PlatformObservation::current())?;
            (release.assets.iter().any(|asset| asset == &expected_asset)
                && is_newer_version(&release.version, installed))
            .then(|| (target, release.version.clone()))
        })
        .collect::<Vec<_>>();
    let cache_age = metadata.age_at(now_unix_secs);
    if updates.is_empty() {
        DashboardUpdate::Current { cache_age }
    } else {
        DashboardUpdate::Available { updates, cache_age }
    }
}

fn expected_asset_name(target: UpgradeTarget, platform: &PlatformObservation) -> Option<String> {
    let PlatformObservation::Supported(platform) = platform else {
        return None;
    };
    Some(format!("{}-{}.zip", target.label(), platform.label()))
}

fn normalize_release_version(tag: &str) -> Option<String> {
    let version = tag.rsplit_once("-v").map_or(tag, |(_, version)| version);
    let version = version.trim_start_matches('v');
    (!version.is_empty()).then(|| version.to_string())
}

fn is_newer_version(candidate: &str, installed: &str) -> bool {
    match (parse_version(candidate), parse_version(installed)) {
        (Some(candidate), Some(installed)) => candidate > installed,
        _ => false,
    }
}

fn parse_version(version: &str) -> Option<[u64; 3]> {
    let version = normalize_release_version(version)?;
    let mut parts = version.split('.');
    let major = parse_version_part(parts.next()?)?;
    let minor = parse_version_part(parts.next()?)?;
    let patch = parse_version_part(parts.next()?)?;
    Some([major, minor, patch])
}

fn parse_version_part(part: &str) -> Option<u64> {
    let digits = part
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn format_cache_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds >= 24 * 60 * 60 {
        format!("{}d", seconds / (24 * 60 * 60))
    } else if seconds >= 60 * 60 {
        format!("{}h", seconds / (60 * 60))
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}s", seconds)
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        future,
        sync::{Arc, Mutex},
    };

    use super::*;
    use subc_core::test_support::TestTempDir;

    #[derive(Clone)]
    struct StaticSource {
        results: Arc<Vec<(UpgradeTarget, Result<ReleaseEvidence, ReleaseSourceError>)>>,
        calls: Arc<Mutex<Vec<UpgradeTarget>>>,
    }

    impl StaticSource {
        fn successful(version: &str) -> Self {
            let results = UpgradeTarget::ORDERED
                .into_iter()
                .map(|target| {
                    (
                        target,
                        Ok(ReleaseEvidence {
                            version: version.to_string(),
                            assets: all_alpha_assets(target),
                        }),
                    )
                })
                .collect();
            Self {
                results: Arc::new(results),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn failing(error: ReleaseSourceError) -> Self {
            let results = UpgradeTarget::ORDERED
                .into_iter()
                .map(|target| (target, Err(error.clone())))
                .collect();
            Self {
                results: Arc::new(results),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<UpgradeTarget> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ReleaseSource for StaticSource {
        fn fetch<'a>(&'a self, target: UpgradeTarget) -> SourceFuture<'a> {
            self.calls.lock().unwrap().push(target);
            let result = self
                .results
                .iter()
                .find(|(candidate, _)| *candidate == target)
                .map(|(_, result)| result.clone())
                .unwrap_or_else(|| {
                    Err(ReleaseSourceError::InvalidResponse(
                        "missing target".to_string(),
                    ))
                });
            Box::pin(async move { result })
        }
    }

    struct HangingSource {
        immediate_before: UpgradeTarget,
    }

    impl ReleaseSource for HangingSource {
        fn fetch<'a>(&'a self, target: UpgradeTarget) -> SourceFuture<'a> {
            if target == self.immediate_before {
                Box::pin(async move {
                    Ok(ReleaseEvidence {
                        version: "0.12.0".to_string(),
                        assets: all_alpha_assets(target),
                    })
                })
            } else {
                Box::pin(future::pending())
            }
        }
    }

    fn all_alpha_assets(target: UpgradeTarget) -> BTreeSet<String> {
        ["darwin-arm64", "linux-x64", "windows-x64"]
            .into_iter()
            .map(|platform| format!("{}-{platform}.zip", target.label()))
            .collect()
    }

    fn metadata(checked_at_unix_secs: u64, version: &str) -> UpdateMetadata {
        UpdateMetadata {
            checked_at_unix_secs,
            targets: UpgradeTarget::ORDERED
                .into_iter()
                .map(|target| {
                    (
                        target.label().to_string(),
                        CachedRelease {
                            version: version.to_string(),
                            assets: all_alpha_assets(target).into_iter().collect(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn cache(name: &str) -> (TestTempDir, UpdateCache) {
        let dir = TestTempDir::new(name);
        let cache = UpdateCache::new(dir.path().join("update-metadata.json"));
        (dir, cache)
    }

    fn installed(version: &str) -> BTreeMap<String, String> {
        UpgradeTarget::ORDERED
            .into_iter()
            .map(|target| (target.label().to_string(), version.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn fresh_cache_is_consumed_without_a_release_request() {
        let (_dir, cache) = cache("fresh");
        cache.write(&metadata(10_000, "0.12.0")).unwrap();
        let source = StaticSource::failing(ReleaseSourceError::Offline("must not run".to_string()));

        let update = dashboard_update_at(
            &cache,
            &source,
            &installed("0.12.0"),
            10_000 + 30,
            Duration::from_millis(10),
        )
        .await;

        assert_eq!(
            update,
            DashboardUpdate::Current {
                cache_age: Duration::from_secs(30)
            }
        );
        assert!(source.calls().is_empty());
    }

    #[tokio::test]
    async fn expired_cache_refreshes_and_reports_newer_artifacts() {
        let (_dir, cache) = cache("expired");
        cache.write(&metadata(1, "0.12.0")).unwrap();
        let source = StaticSource::successful("0.13.0");
        let now = 1 + super::super::update_cache::UPDATE_CACHE_TTL.as_secs();

        let update = dashboard_update_at(
            &cache,
            &source,
            &installed("0.12.0"),
            now,
            Duration::from_millis(20),
        )
        .await;

        assert!(
            matches!(update, DashboardUpdate::Available { ref updates, cache_age } if updates.len() == 4 && cache_age.is_zero())
        );
        assert_eq!(cache.load(), CacheRead::Present(metadata(now, "0.13.0")));
        assert_eq!(source.calls(), UpgradeTarget::ORDERED.to_vec());
    }

    #[tokio::test]
    async fn absent_cache_is_populated_by_a_successful_user_check() {
        let (_dir, cache) = cache("absent");
        let source = StaticSource::successful("0.13.0");

        let update = dashboard_update_at(
            &cache,
            &source,
            &installed("0.12.0"),
            500,
            Duration::from_millis(20),
        )
        .await;

        assert!(matches!(update, DashboardUpdate::Available { .. }));
        assert!(matches!(cache.load(), CacheRead::Present(_)));
    }

    #[tokio::test]
    async fn malformed_cache_does_not_block_a_failed_refresh() {
        let (_dir, cache) = cache("malformed");
        std::fs::write(cache.path(), b"malformed").unwrap();
        let source = StaticSource::failing(ReleaseSourceError::Offline("network down".to_string()));

        let update = dashboard_update_at(
            &cache,
            &source,
            &installed("0.12.0"),
            500,
            Duration::from_millis(20),
        )
        .await;

        assert_eq!(update, DashboardUpdate::NotChecked { cache_age: None });
        assert_eq!(update.render(), "updates: not checked (cache unavailable)");
    }

    #[tokio::test]
    async fn offline_and_rate_limited_refreshes_render_the_stale_not_checked_form() {
        for error in [
            ReleaseSourceError::Offline("offline".to_string()),
            ReleaseSourceError::RateLimited,
        ] {
            let (_dir, cache) = cache("failed-refresh");
            cache.write(&metadata(100, "0.13.0")).unwrap();
            let source = StaticSource::failing(error);
            let now =
                100 + super::super::update_cache::UPDATE_CACHE_TTL.as_secs() + 3 * 24 * 60 * 60;

            let update = dashboard_update_at(
                &cache,
                &source,
                &installed("0.12.0"),
                now,
                Duration::from_millis(20),
            )
            .await;

            assert_eq!(
                update,
                DashboardUpdate::NotChecked {
                    cache_age: Some(Duration::from_secs(
                        super::super::update_cache::UPDATE_CACHE_TTL.as_secs() + 3 * 24 * 60 * 60
                    )),
                }
            );
            assert!(update
                .render()
                .starts_with("updates: not checked (cache 4d old)"));
        }
    }

    #[tokio::test]
    async fn hanging_bare_refresh_returns_stale_output_inside_its_budget() {
        assert_eq!(BARE_REFRESH_BUDGET, Duration::from_millis(800));
        let (_dir, cache) = cache("hanging-bare");
        cache.write(&metadata(100, "0.13.0")).unwrap();
        let source = HangingSource {
            immediate_before: UpgradeTarget::SubcMcp,
        };
        let now = 100 + super::super::update_cache::UPDATE_CACHE_TTL.as_secs();
        let budget = Duration::from_millis(20);
        let started = std::time::Instant::now();

        let update = dashboard_update_at(&cache, &source, &installed("0.12.0"), now, budget).await;

        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(
            update,
            DashboardUpdate::NotChecked {
                cache_age: Some(Duration::from_secs(
                    super::super::update_cache::UPDATE_CACHE_TTL.as_secs()
                )),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_check_times_out_each_target_independently_and_names_the_expired_target() {
        assert_eq!(TARGET_CHECK_BUDGET, Duration::from_secs(10));
        let (_dir, cache) = cache("check-timeout");
        let source = HangingSource {
            immediate_before: UpgradeTarget::SubcMcp,
        };
        let task = tokio::spawn(async move { check_update_metadata(&cache, &source).await });
        tokio::task::yield_now().await;
        time::advance(Duration::from_secs(10)).await;

        let error = task.await.unwrap().unwrap_err();
        assert_eq!(
            error,
            UpdateCheckError::ExpiredTarget {
                target: UpgradeTarget::Aft,
            }
        );
        assert!(error.to_string().contains("ck-aft"));
    }

    #[test]
    fn metadata_maps_newer_versions_and_missing_archives_into_the_upgrade_planner() {
        let mut metadata = metadata(100, "0.13.0");
        metadata
            .targets
            .get_mut(UpgradeTarget::Aft.label())
            .unwrap()
            .assets
            .clear();
        let observed = observed_from_metadata(&metadata, &installed("0.12.0"));

        assert!(matches!(
            observed.target_state(UpgradeTarget::Ck),
            UpgradeState::UpdateAvailable { ref from, ref to } if from == "0.12.0" && to == "0.13.0"
        ));
        assert!(matches!(
            observed.release(UpgradeTarget::Aft),
            ReleaseAvailability::Incomplete { .. }
        ));
    }

    #[test]
    fn release_version_normalization_accepts_the_tag_conventions() {
        assert_eq!(
            normalize_release_version("subc-core-v0.13.0"),
            Some("0.13.0".to_string())
        );
        assert_eq!(
            normalize_release_version("v0.13.0"),
            Some("0.13.0".to_string())
        );
        assert!(is_newer_version("0.13.0", "0.12.9"));
        assert!(!is_newer_version("unknown", "0.12.9"));
    }
}
