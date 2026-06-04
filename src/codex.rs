use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    calendar::{date_days, date_from_timestamp},
    cost_estimate::{CostEstimateResult, CostEstimator},
    error::{AppError, AppResult},
    profile::{UsageProfile, build_usage_profile},
    remote_usage::RemoteUsageClient,
    state_db::{CostThread, load_cost_thread_metadata, load_thread_activity},
};

const SESSION_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub malformed_lines: usize,
    pub data_warnings: Vec<String>,
    pub latest_session: Option<SessionSummary>,
    pub current_rate_limits: Option<RateLimits>,
    pub profile: UsageProfile,
    pub cost_status: CostStatus,
    pub scanned_at: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CostStatus {
    #[default]
    Ready,
    Indexing,
}

#[derive(Debug, Clone, Default)]
pub struct SessionSummary {
    pub session_id: Option<String>,
    pub modified_at: Option<SystemTime>,
    pub session_date: Option<String>,
    pub activity_date: Option<String>,
    pub daily_usage: Vec<SessionDayUsage>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub last_usage: TokenUsage,
    pub total_usage: TokenUsage,
    pub summed_usage: TokenUsage,
    pub rate_limits: Option<RateLimits>,
    pub malformed_lines: usize,
}

impl SessionSummary {
    pub fn billable_usage(&self) -> TokenUsage {
        if self.summed_usage.total_tokens > 0 {
            self.summed_usage
        } else {
            self.total_usage
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionDayUsage {
    pub date: String,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default, alias = "cache_read_input_tokens")]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RateLimits {
    pub plan_type: Option<String>,
    pub primary: Option<RateWindow>,
    pub secondary: Option<RateWindow>,
    #[serde(default, skip)]
    pub fetched_at: Option<SystemTime>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RateWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct LogEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id_camel: Option<String>,
    model: Option<String>,
    model_provider: Option<String>,
    info: Option<UsageInfo>,
    rate_limits: Option<RateLimits>,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    last_token_usage: Option<TokenUsage>,
    total_token_usage: Option<TokenUsage>,
    model: Option<String>,
    model_name: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionFile {
    path: PathBuf,
    modified_at: Option<SystemTime>,
    len: u64,
    session_date: Option<String>,
}

pub fn scan_codex_home(codex_home: &Path, max_files: usize) -> AppResult<MeterSnapshot> {
    CodexScanner::default().scan(codex_home, max_files)
}

pub fn estimate_codex_cost(codex_home: &Path) -> AppResult<CostEstimateResult> {
    let cutoff_day = billing_cutoff_day();
    let cost_threads = collect_cost_threads(codex_home, cutoff_day)?;
    CostEstimator::default().estimate(&cost_threads, cutoff_day)
}

#[derive(Default)]
pub struct CodexScanner {
    cache: HashMap<PathBuf, CachedSession>,
    cost_estimator: CostEstimator,
    remote_usage: RemoteUsageClient,
}

#[derive(Debug, Clone)]
struct CachedSession {
    modified_at: Option<SystemTime>,
    len: u64,
    summary: SessionSummary,
}

impl CodexScanner {
    pub fn scan(&mut self, codex_home: &Path, max_files: usize) -> AppResult<MeterSnapshot> {
        self.scan_inner(
            codex_home,
            max_files,
            CostScanMode::Include,
            RemoteScanMode::Include,
        )
    }

    pub fn scan_without_cost(
        &mut self,
        codex_home: &Path,
        max_files: usize,
    ) -> AppResult<MeterSnapshot> {
        self.scan_inner(
            codex_home,
            max_files,
            CostScanMode::Skip,
            RemoteScanMode::Include,
        )
    }

    pub fn scan_local_without_cost(
        &mut self,
        codex_home: &Path,
        max_files: usize,
    ) -> AppResult<MeterSnapshot> {
        self.scan_inner(
            codex_home,
            max_files,
            CostScanMode::Skip,
            RemoteScanMode::Skip,
        )
    }

    fn scan_inner(
        &mut self,
        codex_home: &Path,
        max_files: usize,
        cost_mode: CostScanMode,
        remote_mode: RemoteScanMode,
    ) -> AppResult<MeterSnapshot> {
        if !codex_home.exists() {
            return Err(AppError::CodexHomeMissing(codex_home.to_path_buf()));
        }

        let mut session_files = collect_session_files(codex_home)?;
        session_files.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));

        let mut snapshot = MeterSnapshot {
            scanned_at: Some(SystemTime::now()),
            ..MeterSnapshot::default()
        };
        let mut fallback_model = None;
        let mut fallback_provider = None;
        let recent_file_count = max_files.max(1).min(session_files.len());
        let mut recent_summaries = Vec::with_capacity(recent_file_count);

        for file in session_files.iter().take(recent_file_count) {
            let summary = self.summary_for(file)?;
            snapshot.malformed_lines += summary.malformed_lines;
            recent_summaries.push(summary);
        }

        let cost_report = if cost_mode == CostScanMode::Include {
            let cutoff_day = billing_cutoff_day();
            let cost_threads = collect_cost_threads(codex_home, cutoff_day)?;
            let cost_estimate = self.cost_estimator.estimate(&cost_threads, cutoff_day)?;
            snapshot.data_warnings.extend(cost_estimate.warnings);
            cost_estimate.report
        } else {
            snapshot.cost_status = CostStatus::Indexing;
            None
        };
        let indexed_activity = load_thread_activity(codex_home)?.map(|report| report.daily);
        snapshot.profile = build_usage_profile(&recent_summaries, cost_report, indexed_activity);

        for summary in &recent_summaries {
            if should_replace_rate_limits(
                snapshot.current_rate_limits.as_ref(),
                summary.rate_limits.as_ref(),
            ) {
                snapshot.current_rate_limits = summary.rate_limits.clone();
            }
            if fallback_model.is_none() {
                fallback_model = summary.model.clone();
            }
            if fallback_provider.is_none() {
                fallback_provider = summary.provider.clone();
            }
            if snapshot.latest_session.is_none() {
                snapshot.latest_session = Some(summary.clone());
            }
        }

        if let Some(latest) = &mut snapshot.latest_session {
            if latest.model.is_none() {
                latest.model = fallback_model;
            }
            if latest.provider.is_none() {
                latest.provider = fallback_provider;
            }
        }

        if remote_mode == RemoteScanMode::Include {
            let remote_report = self.remote_usage.fetch(codex_home);
            snapshot.data_warnings.extend(remote_report.warnings);
            if let Some(rate_limits) = remote_report.rate_limits {
                snapshot.current_rate_limits = Some(rate_limits);
            }
            if let Some(profile_activity) = remote_report.profile {
                snapshot.profile.apply_remote_activity(profile_activity);
            }
        }

        Ok(snapshot)
    }

    fn summary_for(&mut self, file: &SessionFile) -> AppResult<SessionSummary> {
        if let Some(cached) = self.cache.get(&file.path)
            && cached.modified_at == file.modified_at
            && cached.len == file.len
        {
            return Ok(cached.summary.clone());
        }

        let mut summary = parse_session_file(&file.path, file.modified_at)?;
        summary.session_date = file.session_date.clone();
        self.cache.insert(
            file.path.clone(),
            CachedSession {
                modified_at: file.modified_at,
                len: file.len,
                summary: summary.clone(),
            },
        );
        Ok(summary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CostScanMode {
    Include,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteScanMode {
    Include,
    Skip,
}

fn should_replace_rate_limits(
    current: Option<&RateLimits>,
    candidate: Option<&RateLimits>,
) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };

    current.is_none_or(|current| rate_limit_score(candidate) > rate_limit_score(current))
}

fn rate_limit_score(rate_limits: &RateLimits) -> u8 {
    let mut score = 0;
    if rate_limits.plan_type.is_some() {
        score += 3;
    }
    if rate_window_has_usage(rate_limits.primary.as_ref()) {
        score += 2;
    }
    if rate_window_has_usage(rate_limits.secondary.as_ref()) {
        score += 2;
    }
    if rate_limits
        .primary
        .as_ref()
        .and_then(|window| window.resets_at)
        .is_some()
    {
        score += 1;
    }
    if rate_limits
        .secondary
        .as_ref()
        .and_then(|window| window.resets_at)
        .is_some()
    {
        score += 1;
    }
    score
}

fn rate_window_has_usage(window: Option<&RateWindow>) -> bool {
    window
        .and_then(|window| window.used_percent)
        .is_some_and(|percent| percent > 0.0)
}

fn billing_cutoff_day() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| (duration.as_secs() / 86_400) as i64)
        .unwrap_or(0)
        .saturating_sub(29)
}

fn collect_jsonl_files(root: &Path) -> AppResult<Vec<SessionFile>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_jsonl_files_inner(root, &mut files)?;
    Ok(files)
}

fn collect_session_files(codex_home: &Path) -> AppResult<Vec<SessionFile>> {
    let mut files = collect_jsonl_files(&codex_home.join("sessions"))?;
    files.extend(collect_jsonl_files(&codex_home.join("archived_sessions"))?);
    Ok(files)
}

fn collect_cost_threads(codex_home: &Path, cutoff_day: i64) -> AppResult<Vec<CostThread>> {
    let metadata = load_cost_thread_metadata(codex_home)?;
    let metadata_by_id = metadata
        .iter()
        .map(|(path, metadata)| (metadata.id.as_str(), path.as_path()))
        .collect::<HashMap<_, _>>();
    let mut seen_paths = HashSet::new();
    let mut threads = Vec::new();
    let scan_start_day = cutoff_day.saturating_sub(1);
    let cutoff_secs = cutoff_day.saturating_mul(86_400);

    for file in collect_session_files(codex_home)? {
        let date_recent = file
            .session_date
            .as_deref()
            .and_then(date_days)
            .is_some_and(|days| days >= scan_start_day);
        let modified_recent = file
            .modified_at
            .and_then(system_time_secs)
            .is_some_and(|seconds| {
                i64::try_from(seconds).is_ok_and(|seconds| seconds >= cutoff_secs)
            });
        if !date_recent
            && !modified_recent
            && file.session_date.is_some()
            && file.modified_at.is_some()
        {
            continue;
        }
        if !seen_paths.insert(file.path.clone()) {
            continue;
        }

        let thread_metadata = metadata.get(&file.path);
        let parent_thread_id =
            thread_metadata.and_then(|metadata| metadata.parent_thread_id.clone());
        let parent_rollout_path = parent_thread_id
            .as_deref()
            .and_then(|parent_id| metadata_by_id.get(parent_id).copied())
            .map(Path::to_path_buf);
        threads.push(CostThread {
            id: thread_metadata
                .map(|metadata| metadata.id.clone())
                .unwrap_or_else(|| file.path.to_string_lossy().into_owned()),
            rollout_path: file.path,
            model: thread_metadata.and_then(|metadata| metadata.model.clone()),
            parent_thread_id,
            parent_rollout_path,
        });
    }

    Ok(threads)
}

fn session_file_date(path: &Path) -> Option<String> {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();

    for window in components.windows(4) {
        if window[0] == "sessions"
            && is_year(&window[1])
            && is_month_or_day(&window[2])
            && is_month_or_day(&window[3])
        {
            return Some(format!("{}-{}-{}", window[1], window[2], window[3]));
        }
    }

    let filename = path.file_name()?.to_string_lossy();
    filename
        .strip_prefix("rollout-")
        .and_then(|rest| rest.get(0..10))
        .filter(|date| date_from_timestamp(&format!("{date}T00:00:00Z")).is_some())
        .map(ToOwned::to_owned)
}

fn is_year(value: &str) -> bool {
    value.len() == 4 && value.as_bytes().iter().all(u8::is_ascii_digit)
}

fn is_month_or_day(value: &str) -> bool {
    value.len() == 2 && value.as_bytes().iter().all(u8::is_ascii_digit)
}

fn system_time_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn collect_jsonl_files_inner(root: &Path, files: &mut Vec<SessionFile>) -> AppResult<()> {
    let entries = fs::read_dir(root)
        .map_err(|source| AppError::io(format!("failed to read {}", root.display()), source))?;

    for entry in entries {
        let entry = entry.map_err(|source| {
            AppError::io(
                format!("failed to inspect entry under {}", root.display()),
                source,
            )
        })?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|source| {
            AppError::io(format!("failed to inspect {}", path.display()), source)
        })?;

        if metadata.is_dir() {
            collect_jsonl_files_inner(&path, files)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            let session_date = session_file_date(&path);
            files.push(SessionFile {
                path,
                modified_at: metadata.modified().ok(),
                len: metadata.len(),
                session_date,
            });
        }
    }

    Ok(())
}

fn parse_session_file(path: &Path, modified_at: Option<SystemTime>) -> AppResult<SessionSummary> {
    parse_session_file_with_tail_limit(path, modified_at, SESSION_TAIL_BYTES)
}

fn parse_session_file_with_tail_limit(
    path: &Path,
    modified_at: Option<SystemTime>,
    tail_bytes: u64,
) -> AppResult<SessionSummary> {
    let file = File::open(path)
        .map_err(|source| AppError::io(format!("failed to open {}", path.display()), source))?;
    let length = file
        .metadata()
        .map_err(|source| AppError::io(format!("failed to inspect {}", path.display()), source))?
        .len();

    if length > tail_bytes {
        parse_session_tail(path, modified_at, file, length, tail_bytes)
    } else {
        parse_session_reader(modified_at, BufReader::new(file))
    }
}

fn parse_session_tail(
    path: &Path,
    modified_at: Option<SystemTime>,
    mut file: File,
    length: u64,
    tail_bytes: u64,
) -> AppResult<SessionSummary> {
    let start = length.saturating_sub(tail_bytes);
    file.seek(SeekFrom::Start(start))
        .map_err(|source| AppError::io(format!("failed to seek {}", path.display()), source))?;

    let mut bytes = Vec::with_capacity(tail_bytes.min(usize::MAX as u64) as usize);
    file.read_to_end(&mut bytes)
        .map_err(|source| AppError::io(format!("failed to read {}", path.display()), source))?;

    let lines = if start == 0 {
        bytes.as_slice()
    } else {
        match bytes.iter().position(|byte| *byte == b'\n') {
            Some(index) => &bytes[index + 1..],
            None => &[],
        }
    };

    let text = String::from_utf8_lossy(lines).into_owned();
    parse_session_reader(modified_at, Cursor::new(text.into_bytes()))
}

fn parse_session_reader<R: BufRead>(
    modified_at: Option<SystemTime>,
    reader: R,
) -> AppResult<SessionSummary> {
    let mut summary = SessionSummary {
        modified_at,
        ..SessionSummary::default()
    };
    let mut state = SessionParseState::default();

    for line in reader.lines() {
        let line = line.map_err(|source| AppError::io("failed to read session line", source))?;
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<LogEntry>(&line) {
            Ok(entry) => apply_entry(&mut summary, &mut state, entry),
            Err(_) => summary.malformed_lines += 1,
        }
    }

    Ok(summary)
}

#[derive(Debug, Default)]
struct SessionParseState {
    raw_totals_baseline: Option<TokenUsage>,
    counted_totals: Option<TokenUsage>,
    unresolved_fork_watermark: Option<TokenUsage>,
}

fn apply_entry(summary: &mut SessionSummary, state: &mut SessionParseState, entry: LogEntry) {
    let entry_date = entry
        .timestamp
        .as_deref()
        .and_then(date_from_timestamp)
        .map(ToOwned::to_owned);
    if let Some(timestamp) = entry.timestamp
        && let Some(date) = date_from_timestamp(&timestamp)
    {
        summary.activity_date = Some(date.to_string());
    }

    let entry_kind = entry.kind;
    let Some(payload) = entry.payload else {
        return;
    };

    if entry_kind.as_deref() == Some("session_meta") {
        if summary.session_id.is_none() {
            summary.session_id = payload
                .session_id
                .or(payload.session_id_camel)
                .or(payload.id);
        }
        return;
    }

    let payload_model = payload.model;
    if let Some(model) = &payload_model {
        summary.model = Some(model.clone());
    }

    if let Some(provider) = payload.model_provider {
        summary.provider = Some(provider);
    }

    if let Some(info) = payload.info {
        let info_model = info.model.or(info.model_name);
        if summary.model.is_none() {
            summary.model = info_model;
        }
        if let Some(last) = info.last_token_usage {
            summary.last_usage = last;

            let counted = state.count_delta(Some(last), info.total_token_usage);
            summary.summed_usage = summary.summed_usage.saturating_add(counted);
            record_daily_usage(summary, entry_date.as_deref(), counted);
        } else if let Some(total) = info.total_token_usage {
            let counted = state.count_delta(None, Some(total));
            summary.summed_usage = summary.summed_usage.saturating_add(counted);
            record_daily_usage(summary, entry_date.as_deref(), counted);
        }
        if let Some(total) = info.total_token_usage {
            summary.total_usage = total;
        }
    }

    if let Some(rate_limits) = payload.rate_limits {
        summary.rate_limits = Some(rate_limits);
    }
}

fn record_daily_usage(summary: &mut SessionSummary, date: Option<&str>, usage: TokenUsage) {
    if !usage.has_tokens() {
        return;
    }

    let Some(date) = date else {
        return;
    };

    if let Some(day) = summary.daily_usage.iter_mut().find(|day| day.date == date) {
        day.usage = day.usage.saturating_add(usage);
    } else {
        summary.daily_usage.push(SessionDayUsage {
            date: date.to_string(),
            usage,
        });
    }
}

impl SessionParseState {
    fn count_delta(&mut self, last: Option<TokenUsage>, total: Option<TokenUsage>) -> TokenUsage {
        if let Some(total) = total
            && self.unresolved_fork_watermark.is_none()
            && self.raw_totals_baseline.is_none()
            && last.is_none()
        {
            self.unresolved_fork_watermark = Some(total);
            return TokenUsage::default();
        }

        let delta = match (last, total) {
            (Some(last), Some(total)) => {
                let total_delta = total.delta_from(self.raw_totals_baseline);
                if self.raw_totals_baseline.is_some()
                    && total.at_least(self.raw_totals_baseline)
                    && total_delta.at_most(last)
                {
                    total_delta
                } else {
                    last
                }
            }
            (Some(last), None) => last,
            (None, Some(total)) => total.delta_from(self.raw_totals_baseline),
            (None, None) => TokenUsage::default(),
        }
        .with_cached_clamped();

        let counted = self
            .counted_totals
            .unwrap_or_default()
            .saturating_add(delta);
        self.counted_totals = Some(counted);
        if let Some(total) = total {
            self.raw_totals_baseline = Some(total);
        } else {
            self.raw_totals_baseline = Some(counted);
        }
        delta
    }
}

impl TokenUsage {
    pub fn saturating_add(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_add(other.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
    }

    pub fn delta_from(self, baseline: Option<TokenUsage>) -> TokenUsage {
        let baseline = baseline.unwrap_or_default();
        TokenUsage {
            input_tokens: self.input_tokens.saturating_sub(baseline.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(baseline.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(baseline.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(baseline.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(baseline.total_tokens),
        }
    }

    pub fn at_least(self, baseline: Option<TokenUsage>) -> bool {
        let baseline = baseline.unwrap_or_default();
        self.input_tokens >= baseline.input_tokens
            && self.cached_input_tokens >= baseline.cached_input_tokens
            && self.output_tokens >= baseline.output_tokens
    }

    pub fn at_most(self, other: TokenUsage) -> bool {
        self.input_tokens <= other.input_tokens
            && self.cached_input_tokens <= other.cached_input_tokens
            && self.output_tokens <= other.output_tokens
    }

    pub fn with_cached_clamped(mut self) -> TokenUsage {
        self.cached_input_tokens = self.cached_input_tokens.min(self.input_tokens);
        if self.total_tokens == 0 {
            self.total_tokens = self.input_tokens.saturating_add(self.output_tokens);
        }
        self
    }

    pub fn has_tokens(self) -> bool {
        self.total_tokens > 0
            || self.input_tokens > 0
            || self.cached_input_tokens > 0
            || self.output_tokens > 0
            || self.reasoning_output_tokens > 0
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Cursor,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn parses_usage_rate_limits_and_model_without_content() {
        let input = r#"
{"type":"session_meta","payload":{"session_id":"session-one"}}
{"timestamp":"2026-06-03T10:00:00Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-06-03T10:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":5,"output_tokens":2,"reasoning_output_tokens":1,"total_tokens":12}},"rate_limits":{"primary":{"used_percent":42.5,"window_minutes":300,"resets_at":1780492992},"secondary":{"used_percent":71.0,"window_minutes":10080,"resets_at":1780846619},"plan_type":"pro"}}}
"#;

        let summary =
            parse_session_reader(Some(UNIX_EPOCH), Cursor::new(input)).expect("valid fixture");

        assert_eq!(summary.session_id.as_deref(), Some("session-one"));
        assert_eq!(summary.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(summary.last_usage.total_tokens, 12);
        assert_eq!(summary.last_usage.cached_input_tokens, 5);
        assert_eq!(
            summary.daily_usage,
            vec![SessionDayUsage {
                date: "2026-06-03".to_string(),
                usage: TokenUsage {
                    input_tokens: 10,
                    cached_input_tokens: 5,
                    output_tokens: 2,
                    reasoning_output_tokens: 1,
                    total_tokens: 12,
                },
            }]
        );
        assert_eq!(
            summary
                .rate_limits
                .as_ref()
                .and_then(|limits| limits.primary.as_ref())
                .and_then(|window| window.used_percent),
            Some(42.5)
        );
    }

    #[test]
    fn records_daily_usage_inside_multi_day_session() {
        let input = r#"
{"timestamp":"2026-06-01T23:58:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":10}}}}
{"timestamp":"2026-06-02T00:03:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":20}}}}
"#;

        let summary =
            parse_session_reader(Some(UNIX_EPOCH), Cursor::new(input)).expect("valid fixture");

        assert_eq!(
            summary.daily_usage,
            vec![
                SessionDayUsage {
                    date: "2026-06-01".to_string(),
                    usage: TokenUsage {
                        total_tokens: 10,
                        ..TokenUsage::default()
                    },
                },
                SessionDayUsage {
                    date: "2026-06-02".to_string(),
                    usage: TokenUsage {
                        total_tokens: 20,
                        ..TokenUsage::default()
                    },
                },
            ]
        );
    }

    #[test]
    fn extracts_session_date_from_nested_session_path() {
        let path = Path::new("/tmp/.codex/sessions/2026/06/03/rollout-demo.jsonl");

        assert_eq!(session_file_date(path).as_deref(), Some("2026-06-03"));
    }

    #[test]
    fn extracts_session_date_from_archived_rollout_filename() {
        let path = Path::new("/tmp/.codex/archived_sessions/rollout-2026-04-17T10-00-00-id.jsonl");

        assert_eq!(session_file_date(path).as_deref(), Some("2026-04-17"));
    }

    #[test]
    fn tail_scan_skips_partial_first_line_and_reads_recent_metrics() {
        let root = unique_temp_dir("codex-meter-tail");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("large.jsonl");
        let old_line = "{\"timestamp\":\"2026-06-03T09:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":1}}}}\n";
        let filler = "x".repeat(512);
        let recent_line = "{\"timestamp\":\"2026-06-03T10:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":99}}}}\n";
        fs::write(&path, format!("{old_line}{filler}\n{recent_line}")).expect("write fixture");

        let summary =
            parse_session_file_with_tail_limit(&path, Some(UNIX_EPOCH), 256).expect("tail scan");

        assert_eq!(summary.last_usage.total_tokens, 99);
        assert_eq!(summary.malformed_lines, 0);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scanner_collects_recent_files_and_bad_lines() {
        let root = unique_temp_dir("codex-meter-scan");
        let sessions = root.join("sessions").join("2026").join("06").join("03");
        fs::create_dir_all(&sessions).expect("create sessions");

        fs::write(
            sessions.join("one.jsonl"),
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":10}}}}"#,
        )
        .expect("write one");
        fs::write(
            sessions.join("two.jsonl"),
            "not json\n{\"timestamp\":\"2026-06-03T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":20}}}}\n",
        )
        .expect("write two");

        let snapshot = CodexScanner::default()
            .scan_local_without_cost(&root, 64)
            .expect("scan");

        assert_eq!(snapshot.malformed_lines, 1);
        assert_eq!(snapshot.profile.activity_total_tokens, 30);
        assert_eq!(snapshot.profile.active_days, 1);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scanner_keeps_more_informative_rate_limits() {
        let root = unique_temp_dir("codex-meter-rate-limits");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("create sessions");

        let weak = sessions.join("weak.jsonl");
        fs::write(
            &weak,
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1780492992},"secondary":{"used_percent":0.0,"window_minutes":10080,"resets_at":1780846619},"plan_type":null}}}"#,
        )
        .expect("write weak");

        let strong = sessions.join("strong.jsonl");
        fs::write(
            &strong,
            r#"{"timestamp":"2026-06-03T10:01:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":69.0,"window_minutes":300,"resets_at":1780492992},"secondary":{"used_percent":74.0,"window_minutes":10080,"resets_at":1780846619},"plan_type":"prolite"}}}"#,
        )
        .expect("write strong");

        let snapshot = scan_codex_home(&root, 8).expect("scan");
        let limits = snapshot.current_rate_limits.expect("limits");

        assert_eq!(limits.plan_type.as_deref(), Some("prolite"));
        assert_eq!(
            limits.primary.and_then(|window| window.used_percent),
            Some(69.0)
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scanner_includes_archived_sessions() {
        let root = unique_temp_dir("codex-meter-archived");
        let live_sessions = root.join("sessions");
        let archived_sessions = root.join("archived_sessions");
        fs::create_dir_all(&live_sessions).expect("create live sessions");
        fs::create_dir_all(&archived_sessions).expect("create archived sessions");

        fs::write(
            live_sessions.join("live.jsonl"),
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":10}}}}"#,
        )
        .expect("write live");
        fs::write(
            archived_sessions.join("archived.jsonl"),
            r#"{"timestamp":"2026-01-05T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":20}}}}"#,
        )
        .expect("write archived");

        let snapshot = scan_codex_home(&root, 64).expect("scan");

        assert_eq!(snapshot.profile.active_days, 2);
        assert_eq!(snapshot.profile.daily.len(), 2);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cost_estimate_scans_jsonl_without_state_db() {
        let root = unique_temp_dir("codex-meter-cost-jsonl");
        let date = current_test_date();
        let sessions = dated_session_dir(&root, &date);
        fs::create_dir_all(&sessions).expect("create sessions");
        fs::write(
            sessions.join("live.jsonl"),
            session_cost_fixture(&date, "standalone-session", 1_000, 100, 200),
        )
        .expect("write session");

        let cutoff_day = billing_cutoff_day();
        let threads = collect_cost_threads(&root, cutoff_day).expect("collect cost threads");
        let mut estimator =
            crate::cost_estimate::CostEstimator::with_cache_path(root.join("cache.json"));
        let report = estimator
            .estimate(&threads, cutoff_day)
            .expect("estimate cost")
            .report
            .expect("report");

        assert_eq!(threads.len(), 1);
        assert_eq!(report.top_model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            report
                .daily
                .iter()
                .find(|day| day.date == date)
                .map(|day| day.tokens),
            Some(1_200)
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cost_estimate_deduplicates_session_meta_ids() {
        let root = unique_temp_dir("codex-meter-cost-dedupe");
        let date = current_test_date();
        let sessions = dated_session_dir(&root, &date);
        fs::create_dir_all(&sessions).expect("create sessions");
        fs::write(
            sessions.join("one.jsonl"),
            session_cost_fixture(&date, "same-session", 1_000, 100, 200),
        )
        .expect("write one");
        fs::write(
            sessions.join("two.jsonl"),
            session_cost_fixture(&date, "same-session", 1_000, 100, 200),
        )
        .expect("write two");

        let cutoff_day = billing_cutoff_day();
        let threads = collect_cost_threads(&root, cutoff_day).expect("collect cost threads");
        let mut estimator =
            crate::cost_estimate::CostEstimator::with_cache_path(root.join("cache.json"));
        let report = estimator
            .estimate(&threads, cutoff_day)
            .expect("estimate cost")
            .report
            .expect("report");

        assert_eq!(threads.len(), 2);
        assert_eq!(
            report
                .daily
                .iter()
                .find(|day| day.date == date)
                .map(|day| day.tokens),
            Some(1_200)
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scan_without_cost_marks_cost_indexing() {
        let root = unique_temp_dir("codex-meter-no-cost");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("create sessions");
        fs::write(
            sessions.join("live.jsonl"),
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":10}}}}"#,
        )
        .expect("write live");

        let mut scanner = CodexScanner::default();
        let snapshot = scanner
            .scan_without_cost(&root, 8)
            .expect("scan without cost");

        assert_eq!(snapshot.cost_status, CostStatus::Indexing);
        assert_eq!(snapshot.profile.today_cost_usd, None);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scan_local_without_cost_skips_auth_work() {
        let root = unique_temp_dir("codex-meter-local-fast");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("create sessions");
        fs::write(root.join("auth.json"), "{").expect("write invalid auth");
        fs::write(
            sessions.join("live.jsonl"),
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":10}}}}"#,
        )
        .expect("write live");

        let mut scanner = CodexScanner::default();
        let snapshot = scanner
            .scan_local_without_cost(&root, 8)
            .expect("local scan without cost");

        assert_eq!(snapshot.cost_status, CostStatus::Indexing);
        assert!(snapshot.data_warnings.is_empty());
        assert_eq!(snapshot.profile.activity_last_30_days_tokens, 10);

        fs::remove_dir_all(root).expect("cleanup");
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }

    fn current_test_date() -> String {
        let today = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs()
            / 86_400;
        crate::calendar::date_string_from_days(today as i64)
    }

    fn dated_session_dir(root: &Path, date: &str) -> PathBuf {
        root.join("sessions")
            .join(&date[0..4])
            .join(&date[5..7])
            .join(&date[8..10])
    }

    fn session_cost_fixture(
        date: &str,
        session_id: &str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
    ) -> String {
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{session_id}\"}}}}\n\
             {{\"timestamp\":\"{date}T10:00:00Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.5\"}}}}\n\
             {{\"timestamp\":\"{date}T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":{input_tokens},\"cached_input_tokens\":{cached_input_tokens},\"output_tokens\":{output_tokens},\"total_tokens\":{}}}}}}}}}\n",
            input_tokens + output_tokens
        )
    }
}
