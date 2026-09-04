use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::time;

use super::{
    model::{
        PlatformObservation, ReleaseAvailability, UpgradeObserved, UpgradeState, UpgradeTarget,
    },
    release_index::{self, IndexRefusal, ReleaseIndex},
    update_cache::{CacheRead, CachedRelease, UpdateCache, UpdateMetadata},
};

pub const BARE_REFRESH_BUDGET: Duration = Duration::from_millis(800);
pub const TARGET_CHECK_BUDGET: Duration = Duration::from_secs(10);

type SourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReleaseEvidence, ReleaseSourceError>> + Send + 'a>>;

/// The release evidence needed to decide whether one managed artifact is current
/// and whether the signed index lists it for this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseEvidence {
    /// Display-only release text. It is intentionally not part of currency.
    pub version: String,
    pub sha256: Option<String>,
}

/// State read from one inventory-owned binary. Currency compares
/// `archive_sha256` (the zip the binary was extracted from) to the index asset
/// digest. `sha256` is the extracted binary — a different file — and is not a
/// currency input; version text is retained only for rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledBinary {
    pub version: String,
    pub sha256: Option<String>,
    pub archive_sha256: Option<String>,
}

pub trait ReleaseSource: Send + Sync {
    fn fetch<'a>(&'a self, target: UpgradeTarget) -> SourceFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseSourceError {
    Offline(String),
    InvalidResponse(String),
    IndexStale { url: String },
}

impl fmt::Display for ReleaseSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline(reason) => write!(formatter, "release source unavailable: {reason}"),
            Self::InvalidResponse(reason) => {
                write!(formatter, "invalid release response: {reason}")
            }
            Self::IndexStale { url } => write!(formatter, "updates: index stale ({url})"),
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
    IndexStale {
        url: String,
    },
    IndexUnreachable {
        reason: String,
    },
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
            Self::IndexStale { .. } => formatter.write_str("updates: index stale"),
            Self::IndexUnreachable { reason } => {
                write!(formatter, "updates: not checked ({reason})")
            }
        }
    }
}

impl Error for UpdateCheckError {}

/// Resolves installed-vs-available from one fetched signed index. Construction
/// does not touch the network; the first `fetch` downloads the document once
/// and every later target reads the cache.
pub struct IndexReleaseSource {
    url: String,
    /// The transport's own bound, so the fetch child dies with the budget
    /// instead of surviving a caller that stopped waiting.
    deadline: Duration,
    cached: std::sync::Mutex<Option<Result<ReleaseIndex, IndexRefusal>>>,
}

impl IndexReleaseSource {
    /// `deadline` is the budget of the command that will drive this source:
    /// `BARE_REFRESH_BUDGET` for the dashboard, `TARGET_CHECK_BUDGET` for
    /// `ck upgrade --check`.
    pub fn from_environment(deadline: Duration) -> Self {
        Self {
            url: release_index::index_url(),
            deadline,
            cached: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub fn from_index(index: ReleaseIndex) -> Self {
        Self {
            url: String::new(),
            deadline: Duration::ZERO,
            cached: std::sync::Mutex::new(Some(Ok(index))),
        }
    }

    pub fn cloned_index(&self) -> Result<ReleaseIndex, IndexRefusal> {
        match self.cached.lock().unwrap().as_ref() {
            Some(Ok(index)) => Ok(index.clone()),
            Some(Err(refusal)) => Err(refusal.clone()),
            None => release_index::fetch_index(&self.url, self.deadline),
        }
    }
}

impl ReleaseSource for IndexReleaseSource {
    fn fetch<'a>(&'a self, target: UpgradeTarget) -> SourceFuture<'a> {
        Box::pin(async move {
            let cached = {
                let guard = self.cached.lock().unwrap();
                guard.clone()
            };
            let result = if let Some(result) = cached {
                result
            } else {
                let url = self.url.clone();
                let deadline = self.deadline;
                let fetched =
                    tokio::task::spawn_blocking(move || release_index::fetch_index(&url, deadline))
                        .await
                        .unwrap_or_else(|error| {
                            Err(IndexRefusal::Unreachable {
                                url: self.url.clone(),
                                reason: error.to_string(),
                            })
                        });
                let mut guard = self.cached.lock().unwrap();
                if guard.is_none() {
                    *guard = Some(fetched.clone());
                }
                guard.as_ref().expect("index was just stored").clone()
            };
            evidence_from_index(&result, target)
        })
    }
}

fn evidence_from_index(
    result: &Result<ReleaseIndex, IndexRefusal>,
    target: UpgradeTarget,
) -> Result<ReleaseEvidence, ReleaseSourceError> {
    match result {
        Err(IndexRefusal::Unreachable { url, reason }) => {
            Err(ReleaseSourceError::Offline(format!("{url}: {reason}")))
        }
        Err(IndexRefusal::Stale { url, .. }) => {
            Err(ReleaseSourceError::IndexStale { url: url.clone() })
        }
        Err(other) => Err(ReleaseSourceError::InvalidResponse(other.to_string())),
        Ok(index) => {
            let (component, binary) = upgrade_target_index_path(target);
            let Some(entry) = index.components.get(component) else {
                return Ok(ReleaseEvidence {
                    version: String::new(),
                    sha256: None,
                });
            };
            let version = entry.version.clone().unwrap_or_default();
            let sha256 = match PlatformObservation::current() {
                PlatformObservation::Supported(platform) => entry
                    .assets
                    .get(platform.label())
                    .and_then(|binaries| binaries.get(binary))
                    .map(|asset| asset.sha256.clone()),
                PlatformObservation::Unsupported(_) => None,
            };
            Ok(ReleaseEvidence { version, sha256 })
        }
    }
}

pub(super) fn upgrade_target_index_path(target: UpgradeTarget) -> (&'static str, &'static str) {
    match target {
        UpgradeTarget::Ck => ("core", "ck"),
        UpgradeTarget::Daemon => ("core", "ck-subc"),
        UpgradeTarget::SubcMcp => ("core", "ck-subc-mcp"),
        UpgradeTarget::Aft => ("aft", "ck-aft"),
    }
}

/// The state shown by bare `ck`. A failed refresh deliberately does not turn a
/// stale observation into an update claim, even if that stale cache contained a
/// newer version before the release source became unavailable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DashboardDelta {
    pub target: UpgradeTarget,
    pub from: String,
    pub to: String,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DashboardUpdate {
    Current {
        cache_age: Duration,
    },
    Available {
        updates: Vec<DashboardDelta>,
        cache_age: Duration,
    },
    NotChecked {
        cache_age: Option<Duration>,
    },
    IndexStale,
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
                    .map(|update| match &update.reason {
                        Some(reason) => format!("{} {reason}", update.target),
                        None => format!("{} {} → {}", update.target, update.from, update.to),
                    })
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
            Self::IndexStale => "updates: index stale".to_string(),
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
    installed: &BTreeMap<String, InstalledBinary>,
) -> DashboardUpdate {
    dashboard_update_at(
        cache,
        source,
        installed,
        unix_now_secs(),
        BARE_REFRESH_BUDGET,
    )
    .await
}

pub async fn dashboard_update_at<S: ReleaseSource>(
    cache: &UpdateCache,
    source: &S,
    installed: &BTreeMap<String, InstalledBinary>,
    now_unix_secs: u64,
    budget: Duration,
) -> DashboardUpdate {
    let cached = match cache.load() {
        CacheRead::Present(metadata) => Some(metadata),
        CacheRead::Absent | CacheRead::Malformed | CacheRead::Unreadable(_) => None,
    };
    if let Some(metadata) = &cached {
        if metadata.is_fresh_at(now_unix_secs) {
            return dashboard_state(metadata, installed, now_unix_secs);
        }
    }

    let refreshed = time::timeout(budget, refresh_all(source, now_unix_secs)).await;
    match refreshed {
        Ok(Ok(metadata)) => {
            if cache.write(&metadata).is_err() {
                DashboardUpdate::NotChecked {
                    cache_age: cached.map(|metadata| metadata.age_at(now_unix_secs)),
                }
            } else {
                dashboard_state(&metadata, installed, now_unix_secs)
            }
        }
        Ok(Err(ReleaseSourceError::IndexStale { .. })) => DashboardUpdate::IndexStale,
        _ => DashboardUpdate::NotChecked {
            cache_age: cached.map(|metadata| metadata.age_at(now_unix_secs)),
        },
    }
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
            Ok(Err(ReleaseSourceError::IndexStale { url })) => {
                return Err(UpdateCheckError::IndexStale { url })
            }
            Ok(Err(ReleaseSourceError::Offline(reason))) => {
                return Err(UpdateCheckError::IndexUnreachable { reason })
            }
            Ok(Err(source)) => return Err(UpdateCheckError::Source { target, source }),
            Err(_) => return Err(UpdateCheckError::ExpiredTarget { target }),
        };
        targets.insert(
            target.label().to_string(),
            CachedRelease {
                version: evidence.version,
                sha256: evidence.sha256,
            },
        );
    }
    let metadata = UpdateMetadata {
        format_version: super::update_cache::UPDATE_CACHE_FORMAT_VERSION,
        checked_at_unix_secs: now_unix_secs,
        targets,
    };
    cache
        .write(&metadata)
        .map_err(UpdateCheckError::CacheWrite)?;
    Ok(metadata)
}

/// Applies host-specific release digests to the explicit upgrade planner state.
/// Version strings remain in the rendered plan but never decide a replacement.
pub fn observed_from_metadata(
    metadata: &UpdateMetadata,
    installed: &BTreeMap<String, InstalledBinary>,
) -> UpgradeObserved {
    let mut observed = UpgradeObserved::no_updates_on_current_host();
    let platform = observed.platform.clone();
    for target in UpgradeTarget::ORDERED {
        let Some(release) = metadata.targets.get(target.label()) else {
            continue;
        };
        let Some(release_digest) = release.sha256.as_deref() else {
            if let Some(expected_asset) = expected_asset_name(target, &platform) {
                observed.releases.insert(
                    target.label().to_string(),
                    ReleaseAvailability::Incomplete {
                        missing_asset: expected_asset,
                    },
                );
            }
            continue;
        };
        let Some(installed) = installed.get(target.label()) else {
            continue;
        };
        let (needs_replacement, reason) = match installed.archive_sha256.as_deref() {
            Some(digest) if digest == release_digest => (false, None),
            Some(_) => (true, None),
            None => (
                true,
                Some("no recorded digest; replacing to establish one".to_string()),
            ),
        };
        if needs_replacement {
            observed.targets.insert(
                target.label().to_string(),
                UpgradeState::UpdateAvailable {
                    from: installed.version.clone(),
                    to: release.version.clone(),
                    reason,
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
                sha256: evidence.sha256,
            },
        );
    }
    Ok(UpdateMetadata {
        format_version: super::update_cache::UPDATE_CACHE_FORMAT_VERSION,
        checked_at_unix_secs: now_unix_secs,
        targets,
    })
}

fn dashboard_state(
    metadata: &UpdateMetadata,
    installed: &BTreeMap<String, InstalledBinary>,
    now_unix_secs: u64,
) -> DashboardUpdate {
    let updates = UpgradeTarget::ORDERED
        .into_iter()
        .filter_map(|target| {
            let release = metadata.targets.get(target.label())?;
            let installed = installed.get(target.label())?;
            let release_digest = release.sha256.as_deref()?;
            match installed.archive_sha256.as_deref() {
                Some(digest) if digest == release_digest => None,
                Some(_) => Some(DashboardDelta {
                    target,
                    from: installed.version.clone(),
                    to: release.version.clone(),
                    reason: None,
                }),
                None => Some(DashboardDelta {
                    target,
                    from: installed.version.clone(),
                    to: release.version.clone(),
                    reason: Some("no recorded digest; run ck upgrade to establish one".to_string()),
                }),
            }
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
        collections::BTreeMap,
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
                            sha256: Some(digest(target, version)),
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
                        sha256: Some(digest(target, "0.12.0")),
                    })
                })
            } else {
                Box::pin(future::pending())
            }
        }
    }

    fn digest(target: UpgradeTarget, version: &str) -> String {
        let seed = format!("{}-{version}", target.label());
        format!("{seed:0<64}")
    }

    fn metadata(checked_at_unix_secs: u64, version: &str) -> UpdateMetadata {
        UpdateMetadata {
            format_version: super::super::update_cache::UPDATE_CACHE_FORMAT_VERSION,
            checked_at_unix_secs,
            targets: UpgradeTarget::ORDERED
                .into_iter()
                .map(|target| {
                    (
                        target.label().to_string(),
                        CachedRelease {
                            version: version.to_string(),
                            sha256: Some(digest(target, version)),
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

    fn installed(version: &str) -> BTreeMap<String, InstalledBinary> {
        UpgradeTarget::ORDERED
            .into_iter()
            .map(|target| {
                (
                    target.label().to_string(),
                    InstalledBinary {
                        version: version.to_string(),
                        sha256: Some(format!("binary-{}", digest(target, version))),
                        archive_sha256: Some(digest(target, version)),
                    },
                )
            })
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
    async fn offline_refresh_renders_the_stale_not_checked_form() {
        let (_dir, cache) = cache("failed-refresh");
        cache.write(&metadata(100, "0.13.0")).unwrap();
        let source = StaticSource::failing(ReleaseSourceError::Offline("offline".to_string()));
        let now = 100 + super::super::update_cache::UPDATE_CACHE_TTL.as_secs() + 3 * 24 * 60 * 60;

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

    #[tokio::test]
    async fn stale_index_renders_the_index_stale_form() {
        let (_dir, cache) = cache("index-stale");
        let source = StaticSource::failing(ReleaseSourceError::IndexStale {
            url: "https://cortexkit.io/releases/v1/index.json".to_string(),
        });
        let update = dashboard_update_at(
            &cache,
            &source,
            &installed("0.12.0"),
            500,
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(update, DashboardUpdate::IndexStale);
        assert_eq!(update.render(), "updates: index stale");
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
    fn metadata_maps_digest_deltas_and_missing_archives_into_the_upgrade_planner() {
        let mut metadata = metadata(100, "0.13.0");
        metadata
            .targets
            .get_mut(UpgradeTarget::Aft.label())
            .unwrap()
            .sha256 = None;
        let observed = observed_from_metadata(&metadata, &installed("0.12.0"));

        assert!(matches!(
            observed.target_state(UpgradeTarget::Ck),
            UpgradeState::UpdateAvailable { ref from, ref to, .. } if from == "0.12.0" && to == "0.13.0"
        ));
        assert!(matches!(
            observed.release(UpgradeTarget::Aft),
            ReleaseAvailability::Incomplete { .. }
        ));
    }

    #[test]
    fn release_placed_binary_is_current_when_archive_digest_matches_index() {
        let target = UpgradeTarget::SubcMcp;
        let binary_digest = "ab".repeat(32);
        let archive_digest = "cd".repeat(32);
        assert_ne!(
            binary_digest, archive_digest,
            "fixture must keep the two hashes distinct so currency cannot silently read the binary digest"
        );
        let mut metadata = metadata(100, "0.16.2");
        metadata.targets.get_mut(target.label()).unwrap().sha256 = Some(archive_digest.clone());
        let installed = BTreeMap::from([(
            target.label().to_string(),
            InstalledBinary {
                // ck-subc-mcp reports its own crate version, not core's release
                // version. Equal archive bytes must still be current even when
                // the extracted binary hashes to something else.
                version: "0.1.0".to_string(),
                sha256: Some(binary_digest),
                archive_sha256: Some(archive_digest),
            },
        )]);

        let observed = observed_from_metadata(&metadata, &installed);

        assert_eq!(observed.target_state(target), UpgradeState::Current);
    }

    #[test]
    fn changed_sibling_digest_plans_a_replacement() {
        let target = UpgradeTarget::SubcMcp;
        let mut metadata = metadata(100, "0.16.2");
        metadata.targets.get_mut(target.label()).unwrap().sha256 = Some("ab".repeat(32));
        let installed = BTreeMap::from([(
            target.label().to_string(),
            InstalledBinary {
                version: "0.1.0".to_string(),
                sha256: Some("ab".repeat(32)),
                archive_sha256: Some("cd".repeat(32)),
            },
        )]);

        let observed = observed_from_metadata(&metadata, &installed);

        assert!(matches!(
            observed.target_state(target),
            UpgradeState::UpdateAvailable { reason: None, .. }
        ));
    }

    #[test]
    fn sibling_without_a_recorded_digest_plans_one_replacement_to_establish_it() {
        let target = UpgradeTarget::SubcMcp;
        let mut metadata = metadata(100, "0.16.2");
        metadata.targets.get_mut(target.label()).unwrap().sha256 = Some("ab".repeat(32));
        let installed = BTreeMap::from([(
            target.label().to_string(),
            InstalledBinary {
                version: "0.1.0".to_string(),
                sha256: Some("ab".repeat(32)),
                archive_sha256: None,
            },
        )]);

        let observed = observed_from_metadata(&metadata, &installed);

        assert!(matches!(
            observed.target_state(target),
            UpgradeState::UpdateAvailable {
                reason: Some(ref reason),
                ..
            } if reason == "no recorded digest; replacing to establish one"
        ));
    }

    #[tokio::test]
    async fn check_from_a_shared_index_writes_the_cache_once() {
        let sha = "ab".repeat(32);
        let asset = super::super::release_index::IndexAsset {
            url: "http://127.0.0.1/a.zip".to_string(),
            sha256: sha,
            bytes: 99,
            reports: Some("0.99.0".to_string()),
        };
        let mut binaries = std::collections::BTreeMap::new();
        binaries.insert("ck".to_string(), asset.clone());
        binaries.insert("ck-subc".to_string(), asset.clone());
        binaries.insert("ck-subc-mcp".to_string(), asset.clone());
        let mut aft = std::collections::BTreeMap::new();
        aft.insert("ck-aft".to_string(), asset);
        let mut core_targets = std::collections::BTreeMap::new();
        let mut aft_targets = std::collections::BTreeMap::new();
        for platform in ["darwin-arm64", "linux-x64", "windows-x64"] {
            core_targets.insert(platform.to_string(), binaries.clone());
            aft_targets.insert(platform.to_string(), aft.clone());
        }
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "core".to_string(),
            super::super::release_index::IndexComponent {
                release: "subc-core-v0.99.0".to_string(),
                version: Some("0.99.0".to_string()),
                assets: core_targets,
            },
        );
        components.insert(
            "aft".to_string(),
            super::super::release_index::IndexComponent {
                release: "v0.99.0".to_string(),
                version: Some("0.99.0".to_string()),
                assets: aft_targets,
            },
        );
        let index = super::super::release_index::ReleaseIndex {
            schema: 1,
            channel: "alpha".to_string(),
            generated_at_ms: 1_788_425_000_000,
            components,
        };
        let (_dir, cache) = cache("from-index");
        let source = IndexReleaseSource::from_index(index);
        let metadata = check_update_metadata(&cache, &source).await.unwrap();
        assert_eq!(
            metadata.format_version,
            super::super::update_cache::UPDATE_CACHE_FORMAT_VERSION
        );
        assert!(matches!(cache.load(), CacheRead::Present(_)));
    }
}
