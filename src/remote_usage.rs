use std::{
    collections::BTreeMap,
    env, fmt, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    codex::{RateLimits, RateWindow},
    profile::{DailyUsage, ProfileActivity},
};

const CHATGPT_BACKEND_BASE: &str = "https://chatgpt.com/backend-api";
const QUOTA_CACHE_VERSION: u32 = 1;
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(15);
const PROFILE_CACHE_VERSION: u32 = 2;
const PROFILE_CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const CLIENT_ID_HEADER: &str = "codex-meter";

#[derive(Default)]
pub struct RemoteUsageClient {
    agent: Option<ureq::Agent>,
    cached_quota: Option<CachedQuota>,
    cached_profile: Option<CachedProfile>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteUsageReport {
    pub rate_limits: Option<RateLimits>,
    pub profile: Option<ProfileActivity>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct CachedProfile {
    cache_key: String,
    fetched_at: SystemTime,
    profile: ProfileActivity,
}

#[derive(Clone)]
struct CachedQuota {
    cache_key: String,
    fetched_at: SystemTime,
    limits: RateLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskQuotaCache {
    version: u32,
    cache_key: String,
    fetched_at_secs: u64,
    limits: RateLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskProfileCache {
    version: u32,
    cache_key: String,
    fetched_at_secs: u64,
    profile: ProfileActivity,
}

#[derive(Debug)]
enum RemoteUsageError {
    Io {
        context: String,
        source: std::io::Error,
    },
    Json {
        context: String,
        source: serde_json::Error,
    },
    HttpStatus {
        endpoint: &'static str,
        status: u16,
    },
    Transport {
        endpoint: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone)]
struct AuthCredentials {
    access_token: String,
    account_id: Option<String>,
    cache_key: String,
}

#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    #[serde(default)]
    tokens: AuthTokens,
    #[serde(default, alias = "account_id", alias = "accountId")]
    openai_account_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<UsageRateLimit>,
}

#[derive(Debug, Deserialize)]
struct UsageRateLimit {
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Debug, Deserialize)]
struct UsageWindow {
    used_percent: Option<f64>,
    reset_at: Option<u64>,
    limit_window_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    stats: ProfileStats,
}

#[derive(Debug, Deserialize)]
struct ProfileStats {
    lifetime_tokens: Option<u64>,
    peak_daily_tokens: Option<u64>,
    current_streak_days: Option<u64>,
    longest_streak_days: Option<u64>,
    longest_running_turn_sec: Option<u64>,
    #[serde(default)]
    daily_usage_buckets: Vec<ProfileBucket>,
}

#[derive(Debug, Deserialize)]
struct ProfileBucket {
    start_date: String,
    tokens: u64,
}

impl RemoteUsageClient {
    pub fn fetch(&mut self, codex_home: &Path) -> RemoteUsageReport {
        let Some(auth) = load_auth(codex_home) else {
            return RemoteUsageReport::default();
        };

        let auth = match auth {
            Ok(auth) => auth,
            Err(error) => {
                return RemoteUsageReport {
                    warnings: vec![error.to_string()],
                    ..RemoteUsageReport::default()
                };
            }
        };

        let mut report = RemoteUsageReport::default();
        match self.quota_from_cache(&auth) {
            Some(rate_limits) => report.rate_limits = Some(rate_limits),
            None => match load_quota_cache(&auth) {
                Ok(Some(cached)) => {
                    report.rate_limits = Some(cached.limits.clone());
                    self.cached_quota = Some(cached);
                }
                Ok(None) => {
                    self.fetch_and_cache_quota(&auth, &mut report);
                }
                Err(error) => {
                    report.warnings.push(error.to_string());
                    self.fetch_and_cache_quota(&auth, &mut report);
                }
            },
        }

        match self.profile_from_cache(&auth) {
            Some(profile) => report.profile = Some(profile),
            None => match load_profile_cache(&auth) {
                Ok(Some(cached)) => {
                    report.profile = Some(cached.profile.clone());
                    self.cached_profile = Some(cached);
                }
                Ok(None) => {
                    self.fetch_and_cache_profile(&auth, &mut report);
                }
                Err(error) => {
                    report.warnings.push(error.to_string());
                    self.fetch_and_cache_profile(&auth, &mut report);
                }
            },
        }

        report
    }

    fn quota_from_cache(&self, auth: &AuthCredentials) -> Option<RateLimits> {
        let cached = self.cached_quota.as_ref()?;
        if cached.cache_key != auth.cache_key {
            return None;
        }
        let age = SystemTime::now()
            .duration_since(cached.fetched_at)
            .unwrap_or_default();
        (age < QUOTA_CACHE_TTL).then(|| cached.limits.clone())
    }

    fn profile_from_cache(&self, auth: &AuthCredentials) -> Option<ProfileActivity> {
        let cached = self.cached_profile.as_ref()?;
        if cached.cache_key != auth.cache_key {
            return None;
        }
        let age = SystemTime::now()
            .duration_since(cached.fetched_at)
            .unwrap_or_default();
        (age < PROFILE_CACHE_TTL).then(|| cached.profile.clone())
    }

    fn fetch_and_cache_quota(&mut self, auth: &AuthCredentials, report: &mut RemoteUsageReport) {
        match self.fetch_usage(auth) {
            Ok(rate_limits) => {
                let fetched_at = rate_limits.fetched_at.unwrap_or_else(SystemTime::now);
                self.cached_quota = Some(CachedQuota {
                    cache_key: auth.cache_key.clone(),
                    fetched_at,
                    limits: rate_limits.clone(),
                });
                if let Err(error) = save_quota_cache(auth, &rate_limits) {
                    report.warnings.push(error.to_string());
                }
                report.rate_limits = Some(rate_limits);
            }
            Err(error) => {
                report.warnings.push(error.to_string());
                if let Some(cached) = self.quota_from_cache(auth) {
                    report.rate_limits = Some(cached);
                }
            }
        }
    }

    fn fetch_and_cache_profile(&mut self, auth: &AuthCredentials, report: &mut RemoteUsageReport) {
        match self.fetch_profile(auth) {
            Ok(profile) => {
                self.cached_profile = Some(CachedProfile {
                    cache_key: auth.cache_key.clone(),
                    fetched_at: SystemTime::now(),
                    profile: profile.clone(),
                });
                if let Err(error) = save_profile_cache(auth, &profile) {
                    report.warnings.push(error.to_string());
                }
                report.profile = Some(profile);
            }
            Err(error) => {
                report.warnings.push(error.to_string());
                if let Some(cached) = self.profile_from_cache(auth) {
                    report.profile = Some(cached);
                }
            }
        }
    }

    fn fetch_usage(&mut self, auth: &AuthCredentials) -> Result<RateLimits, RemoteUsageError> {
        let response = self.get_json::<UsageResponse>("/wham/usage", auth)?;
        let mut limits = response.into_rate_limits();
        limits.fetched_at = Some(SystemTime::now());
        Ok(limits)
    }

    fn fetch_profile(
        &mut self,
        auth: &AuthCredentials,
    ) -> Result<ProfileActivity, RemoteUsageError> {
        let response = self.get_json::<ProfileResponse>("/wham/profiles/me", auth)?;
        Ok(response.into_activity())
    }

    fn get_json<T>(
        &mut self,
        endpoint: &'static str,
        auth: &AuthCredentials,
    ) -> Result<T, RemoteUsageError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{CHATGPT_BACKEND_BASE}{endpoint}");
        let mut request = self
            .agent()
            .get(&url)
            .set("Authorization", &format!("Bearer {}", auth.access_token))
            .set("X-OpenAI-Client", CLIENT_ID_HEADER);

        if let Some(account_id) = &auth.account_id {
            request = request.set("ChatGPT-Account-Id", account_id);
        }

        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(status, _response)) => {
                return Err(RemoteUsageError::HttpStatus { endpoint, status });
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(RemoteUsageError::Transport {
                    endpoint,
                    message: error.to_string(),
                });
            }
        };

        let text = response
            .into_string()
            .map_err(|source| RemoteUsageError::Io {
                context: format!("failed to read {endpoint} response"),
                source,
            })?;

        parse_json(endpoint, &text)
    }

    fn agent(&mut self) -> &ureq::Agent {
        self.agent.get_or_insert_with(|| {
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(4))
                .build()
        })
    }
}

impl UsageResponse {
    fn into_rate_limits(self) -> RateLimits {
        RateLimits {
            plan_type: self.plan_type,
            primary: self
                .rate_limit
                .as_ref()
                .and_then(|limits| limits.primary_window.as_ref())
                .map(UsageWindow::to_rate_window),
            secondary: self
                .rate_limit
                .as_ref()
                .and_then(|limits| limits.secondary_window.as_ref())
                .map(UsageWindow::to_rate_window),
            fetched_at: None,
        }
    }
}

impl UsageWindow {
    fn to_rate_window(&self) -> RateWindow {
        RateWindow {
            used_percent: self.used_percent,
            window_minutes: self.limit_window_seconds.map(|seconds| seconds / 60),
            resets_at: self.reset_at,
        }
    }
}

impl ProfileResponse {
    fn into_activity(self) -> ProfileActivity {
        let mut by_day = BTreeMap::<String, u64>::new();
        for bucket in self.stats.daily_usage_buckets {
            *by_day.entry(bucket.start_date).or_default() += bucket.tokens;
        }
        let daily = by_day
            .into_iter()
            .map(|(date, tokens)| DailyUsage {
                date,
                tokens,
                cost_usd: None,
                sessions: u64::from(tokens > 0),
            })
            .collect::<Vec<_>>();

        ProfileActivity {
            daily,
            lifetime_tokens: self.stats.lifetime_tokens,
            peak_day_tokens: self.stats.peak_daily_tokens,
            current_streak_days: self.stats.current_streak_days,
            longest_streak_days: self.stats.longest_streak_days,
            longest_task_seconds: self.stats.longest_running_turn_sec,
        }
    }
}

fn load_auth(codex_home: &Path) -> Option<Result<AuthCredentials, RemoteUsageError>> {
    let path = codex_home.join("auth.json");
    if !path.exists() {
        return None;
    }

    Some(load_auth_file(path))
}

fn load_quota_cache(auth: &AuthCredentials) -> Result<Option<CachedQuota>, RemoteUsageError> {
    let path = quota_cache_path()?;
    load_quota_cache_from_path(&path, auth)
}

fn load_quota_cache_from_path(
    path: &Path,
    auth: &AuthCredentials,
) -> Result<Option<CachedQuota>, RemoteUsageError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RemoteUsageError::Io {
                context: format!("failed to read quota cache {}", path.display()),
                source,
            });
        }
    };
    let mut cache = serde_json::from_slice::<DiskQuotaCache>(&data).map_err(|source| {
        RemoteUsageError::Json {
            context: format!("failed to parse quota cache {}", path.display()),
            source,
        }
    })?;
    if cache.version != QUOTA_CACHE_VERSION || cache.cache_key != auth.cache_key {
        return Ok(None);
    }

    let fetched_at = UNIX_EPOCH + Duration::from_secs(cache.fetched_at_secs);
    let age = SystemTime::now()
        .duration_since(fetched_at)
        .unwrap_or_default();
    if age >= QUOTA_CACHE_TTL {
        return Ok(None);
    }

    cache.limits.fetched_at = Some(fetched_at);
    Ok(Some(CachedQuota {
        cache_key: cache.cache_key,
        fetched_at,
        limits: cache.limits,
    }))
}

fn save_quota_cache(auth: &AuthCredentials, limits: &RateLimits) -> Result<(), RemoteUsageError> {
    let path = quota_cache_path()?;
    save_quota_cache_to_path(&path, auth, limits)
}

fn save_quota_cache_to_path(
    path: &Path,
    auth: &AuthCredentials,
    limits: &RateLimits,
) -> Result<(), RemoteUsageError> {
    let fetched_at = limits.fetched_at.unwrap_or_else(SystemTime::now);
    let cache = DiskQuotaCache {
        version: QUOTA_CACHE_VERSION,
        cache_key: auth.cache_key.clone(),
        fetched_at_secs: system_time_secs(fetched_at),
        limits: limits.clone(),
    };
    save_json_cache(path, &cache, "quota")
}

fn load_profile_cache(auth: &AuthCredentials) -> Result<Option<CachedProfile>, RemoteUsageError> {
    let path = profile_cache_path()?;
    load_profile_cache_from_path(&path, auth)
}

fn load_profile_cache_from_path(
    path: &Path,
    auth: &AuthCredentials,
) -> Result<Option<CachedProfile>, RemoteUsageError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RemoteUsageError::Io {
                context: format!("failed to read profile cache {}", path.display()),
                source,
            });
        }
    };
    let cache = serde_json::from_slice::<DiskProfileCache>(&data).map_err(|source| {
        RemoteUsageError::Json {
            context: format!("failed to parse profile cache {}", path.display()),
            source,
        }
    })?;
    if cache.version != PROFILE_CACHE_VERSION || cache.cache_key != auth.cache_key {
        return Ok(None);
    }

    let fetched_at = UNIX_EPOCH + Duration::from_secs(cache.fetched_at_secs);
    let age = SystemTime::now()
        .duration_since(fetched_at)
        .unwrap_or_default();
    if age >= PROFILE_CACHE_TTL {
        return Ok(None);
    }

    Ok(Some(CachedProfile {
        cache_key: cache.cache_key,
        fetched_at,
        profile: cache.profile,
    }))
}

fn save_profile_cache(
    auth: &AuthCredentials,
    profile: &ProfileActivity,
) -> Result<(), RemoteUsageError> {
    let path = profile_cache_path()?;
    save_profile_cache_to_path(&path, auth, profile)
}

fn save_profile_cache_to_path(
    path: &Path,
    auth: &AuthCredentials,
    profile: &ProfileActivity,
) -> Result<(), RemoteUsageError> {
    let cache = DiskProfileCache {
        version: PROFILE_CACHE_VERSION,
        cache_key: auth.cache_key.clone(),
        fetched_at_secs: system_time_secs(SystemTime::now()),
        profile: profile.clone(),
    };
    save_json_cache(path, &cache, "profile")
}

fn save_json_cache<T: Serialize>(
    path: &Path,
    cache: &T,
    label: &'static str,
) -> Result<(), RemoteUsageError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| RemoteUsageError::Io {
            context: format!("failed to create {}", parent.display()),
            source,
        })?;
    }
    let data = serde_json::to_vec(cache).map_err(|source| RemoteUsageError::Json {
        context: format!("failed to encode {label} cache"),
        source,
    })?;
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&tmp, data).map_err(|source| RemoteUsageError::Io {
        context: format!("failed to write {label} cache {}", tmp.display()),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| RemoteUsageError::Io {
        context: format!("failed to replace {label} cache {}", path.display()),
        source,
    })
}

fn quota_cache_path() -> Result<PathBuf, RemoteUsageError> {
    app_cache_path("quota-v1.json")
}

fn profile_cache_path() -> Result<PathBuf, RemoteUsageError> {
    app_cache_path("profile-v1.json")
}

fn app_cache_path(filename: &str) -> Result<PathBuf, RemoteUsageError> {
    let root = if let Some(path) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(path)
    } else if cfg!(target_os = "macos") {
        let home = env::var_os("HOME").ok_or_else(|| RemoteUsageError::Transport {
            endpoint: "remote usage cache",
            message: "HOME is not set; cannot resolve cache directory".to_string(),
        })?;
        PathBuf::from(home).join("Library/Caches")
    } else {
        let home = env::var_os("HOME").ok_or_else(|| RemoteUsageError::Transport {
            endpoint: "remote usage cache",
            message: "HOME is not set; cannot resolve cache directory".to_string(),
        })?;
        PathBuf::from(home).join(".cache")
    };
    Ok(root.join("codex-meter").join(filename))
}

fn system_time_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn load_auth_file(path: PathBuf) -> Result<AuthCredentials, RemoteUsageError> {
    let metadata = fs::metadata(&path).map_err(|source| RemoteUsageError::Io {
        context: format!("failed to inspect {}", path.display()),
        source,
    })?;
    let data = fs::read(&path).map_err(|source| RemoteUsageError::Io {
        context: format!("failed to read {}", path.display()),
        source,
    })?;
    let auth =
        serde_json::from_slice::<AuthFile>(&data).map_err(|source| RemoteUsageError::Json {
            context: format!("failed to parse {}", path.display()),
            source,
        })?;
    let cache_key = auth_cache_key(&auth, &metadata);
    let access_token = auth
        .tokens
        .access_token
        .ok_or_else(|| RemoteUsageError::Transport {
            endpoint: "auth.json",
            message: "missing access token".to_string(),
        })?;
    Ok(AuthCredentials {
        access_token,
        cache_key,
        account_id: auth.openai_account_id,
    })
}

fn auth_cache_key(auth: &AuthFile, metadata: &fs::Metadata) -> String {
    if let Some(account_id) = &auth.openai_account_id {
        return format!("account:{account_id}");
    }

    let modified_secs = metadata
        .modified()
        .ok()
        .map(system_time_secs)
        .unwrap_or_default();
    format!("auth-file:{}:{modified_secs}", metadata.len())
}

fn parse_json<T>(endpoint: &'static str, text: &str) -> Result<T, RemoteUsageError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(text).map_err(|source| RemoteUsageError::Json {
        context: format!("failed to parse {endpoint} response"),
        source,
    })
}

impl fmt::Display for RemoteUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::Json { context, source } => write!(formatter, "{context}: {source}"),
            Self::HttpStatus { endpoint, status } => {
                write!(formatter, "{endpoint} returned HTTP {status}")
            }
            Self::Transport { endpoint, message } => {
                write!(formatter, "{endpoint} request failed: {message}")
            }
        }
    }
}

impl std::error::Error for RemoteUsageError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn decodes_usage_rate_limits_from_wham_usage() {
        let usage = parse_json::<UsageResponse>(
            "/wham/usage",
            r#"
            {
              "plan_type": "prolite",
              "rate_limit": {
                "primary_window": {
                  "used_percent": 13,
                  "reset_at": 1780575479,
                  "limit_window_seconds": 18000
                },
                "secondary_window": {
                  "used_percent": 2,
                  "reset_at": 1781162279,
                  "limit_window_seconds": 604800
                }
              }
            }
            "#,
        )
        .expect("usage response");

        let limits = usage.into_rate_limits();

        assert_eq!(limits.plan_type.as_deref(), Some("prolite"));
        assert_eq!(
            limits
                .primary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(13.0)
        );
        assert_eq!(
            limits
                .primary
                .as_ref()
                .and_then(|window| window.window_minutes),
            Some(300)
        );
        assert_eq!(
            limits
                .secondary
                .as_ref()
                .and_then(|window| window.window_minutes),
            Some(10_080)
        );
    }

    #[test]
    fn decodes_profile_activity_from_wham_profile() {
        let profile = parse_json::<ProfileResponse>(
            "/wham/profiles/me",
            r#"
            {
              "profile": {"username": "h3nok"},
              "stats": {
                "lifetime_tokens": 35659911729,
                "peak_daily_tokens": 2461833705,
                "current_streak_days": 48,
                "longest_streak_days": 48,
                "longest_running_turn_sec": 62894,
                "daily_usage_buckets": [
                  {"start_date": "2025-08-07", "tokens": 210001},
                  {"start_date": "2026-06-03", "tokens": 718974783}
                ]
              }
            }
            "#,
        )
        .expect("profile response")
        .into_activity();

        assert_eq!(profile.lifetime_tokens, Some(35_659_911_729));
        assert_eq!(profile.peak_day_tokens, Some(2_461_833_705));
        assert_eq!(profile.current_streak_days, Some(48));
        assert_eq!(profile.longest_task_seconds, Some(62_894));
        assert_eq!(profile.daily.len(), 2);
        assert_eq!(profile.daily[0].date, "2025-08-07");
        assert_eq!(profile.daily[0].sessions, 1);
    }

    #[test]
    fn profile_cache_is_account_scoped() {
        let dir = test_dir("profile-cache-account");
        let path = dir.join("profile-v1.json");
        let account_auth = auth("account-a");
        let profile = ProfileActivity {
            lifetime_tokens: Some(42),
            daily: vec![DailyUsage {
                date: "2026-06-03".to_string(),
                tokens: 42,
                cost_usd: None,
                sessions: 1,
            }],
            ..ProfileActivity::default()
        };

        save_profile_cache_to_path(&path, &account_auth, &profile).expect("save cache");

        let cached = load_profile_cache_from_path(&path, &account_auth)
            .expect("load cache")
            .expect("cached profile");
        assert_eq!(cached.profile.lifetime_tokens, Some(42));
        assert!(
            load_profile_cache_from_path(&path, &auth("account-b"))
                .expect("load cache")
                .is_none()
        );
    }

    #[test]
    fn quota_cache_is_keyed_and_restores_fetch_time() {
        let dir = test_dir("quota-cache-account");
        let path = dir.join("quota-v1.json");
        let account_auth = auth("account-a");
        let fetched_at = SystemTime::now();
        let mut limits = rate_limits(13.0);
        limits.fetched_at = Some(fetched_at);

        save_quota_cache_to_path(&path, &account_auth, &limits).expect("save cache");

        let cached = load_quota_cache_from_path(&path, &account_auth)
            .expect("load cache")
            .expect("cached quota");
        assert_eq!(
            cached
                .limits
                .primary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(13.0)
        );
        assert!(cached.limits.fetched_at.is_some());
        assert!(
            load_quota_cache_from_path(&path, &auth("account-b"))
                .expect("load cache")
                .is_none()
        );
    }

    #[test]
    fn quota_cache_ignores_stale_entries() {
        let dir = test_dir("quota-cache-stale");
        let path = dir.join("quota-v1.json");
        let account_auth = auth("account-a");
        let stale = DiskQuotaCache {
            version: QUOTA_CACHE_VERSION,
            cache_key: "account:account-a".to_string(),
            fetched_at_secs: 1,
            limits: rate_limits(13.0),
        };
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(
            &path,
            serde_json::to_vec(&stale).expect("encode stale cache"),
        )
        .expect("write stale cache");

        assert!(
            load_quota_cache_from_path(&path, &account_auth)
                .expect("load cache")
                .is_none()
        );
    }

    #[test]
    fn profile_cache_ignores_stale_entries() {
        let dir = test_dir("profile-cache-stale");
        let path = dir.join("profile-v1.json");
        let auth = auth("account-a");
        let stale = DiskProfileCache {
            version: PROFILE_CACHE_VERSION,
            cache_key: "account:account-a".to_string(),
            fetched_at_secs: 1,
            profile: ProfileActivity {
                lifetime_tokens: Some(42),
                ..ProfileActivity::default()
            },
        };
        fs::create_dir_all(&dir).expect("create temp dir");
        fs::write(
            &path,
            serde_json::to_vec(&stale).expect("encode stale cache"),
        )
        .expect("write stale cache");

        assert!(
            load_profile_cache_from_path(&path, &auth)
                .expect("load cache")
                .is_none()
        );
    }

    #[test]
    fn profile_cache_persists_without_account_id_using_auth_file_key() {
        let dir = test_dir("profile-cache-no-account");
        let path = dir.join("profile-v1.json");
        let auth = AuthCredentials {
            access_token: "token".to_string(),
            account_id: None,
            cache_key: "auth-file:10:20".to_string(),
        };
        let profile = ProfileActivity {
            lifetime_tokens: Some(99),
            ..ProfileActivity::default()
        };

        save_profile_cache_to_path(&path, &auth, &profile).expect("save cache");

        let cached = load_profile_cache_from_path(&path, &auth)
            .expect("load cache")
            .expect("cached profile");
        assert_eq!(cached.profile.lifetime_tokens, Some(99));
    }

    fn auth(account_id: &str) -> AuthCredentials {
        AuthCredentials {
            access_token: "token".to_string(),
            account_id: Some(account_id.to_string()),
            cache_key: format!("account:{account_id}"),
        }
    }

    fn rate_limits(used_percent: f64) -> RateLimits {
        RateLimits {
            plan_type: Some("prolite".to_string()),
            primary: Some(RateWindow {
                used_percent: Some(used_percent),
                window_minutes: Some(300),
                resets_at: None,
            }),
            secondary: None,
            fetched_at: None,
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codex-meter-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("remove old temp dir");
        }
        dir
    }
}
