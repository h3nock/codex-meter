use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::Deserialize;

use crate::error::{AppError, AppResult};

const SESSION_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub codex_home: PathBuf,
    pub scanned_files: usize,
    pub available_session_files: usize,
    pub archived_session_files: usize,
    pub malformed_lines: usize,
    pub tail_scanned_files: usize,
    pub event_counts: BTreeMap<String, u64>,
    pub scanned_total_usage: TokenUsage,
    pub latest_session: Option<SessionSummary>,
    pub current_rate_limits: Option<RateLimits>,
    pub recent_session_totals: Vec<u64>,
    pub recent_sessions: Vec<SessionSummary>,
    pub scanned_at: Option<SystemTime>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionSummary {
    pub modified_at: Option<SystemTime>,
    pub started_at: Option<String>,
    pub last_event_at: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub context_window: Option<u64>,
    pub total_usage: TokenUsage,
    pub last_usage: TokenUsage,
    pub rate_limits: Option<RateLimits>,
    pub event_counts: BTreeMap<String, u64>,
    pub line_count: usize,
    pub malformed_lines: usize,
    pub tail_scanned: bool,
    pub bytes_scanned: u64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RateLimits {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub primary: Option<RateWindow>,
    pub secondary: Option<RateWindow>,
    pub credits: Option<serde_json::Value>,
    pub rate_limit_reached_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RateWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct LogEntry {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    payload: Option<Payload>,
}

#[derive(Debug, Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    timestamp: Option<String>,
    model: Option<String>,
    model_provider: Option<String>,
    info: Option<UsageInfo>,
    rate_limits: Option<RateLimits>,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    last_token_usage: Option<TokenUsage>,
    total_token_usage: Option<TokenUsage>,
    model_context_window: Option<u64>,
}

#[derive(Debug, Clone)]
struct SessionFile {
    path: PathBuf,
    modified_at: Option<SystemTime>,
}

pub fn scan_codex_home(codex_home: &Path, max_files: usize) -> AppResult<MeterSnapshot> {
    if !codex_home.exists() {
        return Err(AppError::CodexHomeMissing(codex_home.to_path_buf()));
    }

    let sessions_dir = codex_home.join("sessions");
    let archived_dir = codex_home.join("archived_sessions");
    let mut session_files = collect_jsonl_files(&sessions_dir)?;
    session_files.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));

    let archived_session_files = count_jsonl_files(&archived_dir)?;
    let available_session_files = session_files.len();

    let mut snapshot = MeterSnapshot {
        codex_home: codex_home.to_path_buf(),
        available_session_files,
        archived_session_files,
        scanned_at: Some(SystemTime::now()),
        ..MeterSnapshot::default()
    };
    let mut fallback_model = None;
    let mut fallback_provider = None;
    let mut fallback_context_window = None;

    for file in session_files.into_iter().take(max_files) {
        let summary = parse_session_file(&file.path, file.modified_at)?;
        snapshot.scanned_files += 1;
        snapshot.malformed_lines += summary.malformed_lines;
        if summary.tail_scanned {
            snapshot.tail_scanned_files += 1;
        }
        snapshot.scanned_total_usage += summary.total_usage;

        for (event, count) in &summary.event_counts {
            *snapshot.event_counts.entry(event.clone()).or_default() += count;
        }

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
        if fallback_context_window.is_none() {
            fallback_context_window = summary.context_window;
        }
        snapshot
            .recent_session_totals
            .push(summary.total_usage.total_tokens);
        if snapshot.latest_session.is_none() {
            snapshot.latest_session = Some(summary.clone());
        }
        snapshot.recent_sessions.push(summary);
    }

    if let Some(latest) = &mut snapshot.latest_session {
        if latest.model.is_none() {
            latest.model = fallback_model;
        }
        if latest.provider.is_none() {
            latest.provider = fallback_provider;
        }
        if latest.context_window.is_none() {
            latest.context_window = fallback_context_window;
        }
    }

    Ok(snapshot)
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

fn collect_jsonl_files(root: &Path) -> AppResult<Vec<SessionFile>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_jsonl_files_inner(root, &mut files)?;
    Ok(files)
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
            files.push(SessionFile {
                path,
                modified_at: metadata.modified().ok(),
            });
        }
    }

    Ok(())
}

fn count_jsonl_files(root: &Path) -> AppResult<usize> {
    collect_jsonl_files(root).map(|files| files.len())
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
        let mut summary = parse_session_reader(modified_at, BufReader::new(file))?;
        summary.bytes_scanned = length;
        Ok(summary)
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
    let mut summary = parse_session_reader(modified_at, Cursor::new(text.into_bytes()))?;
    summary.tail_scanned = true;
    summary.bytes_scanned = length - start;
    Ok(summary)
}

fn parse_session_reader<R: BufRead>(
    modified_at: Option<SystemTime>,
    reader: R,
) -> AppResult<SessionSummary> {
    let mut summary = SessionSummary {
        modified_at,
        ..SessionSummary::default()
    };

    for line in reader.lines() {
        let line = line.map_err(|source| AppError::io("failed to read session line", source))?;
        if line.trim().is_empty() {
            continue;
        }

        summary.line_count += 1;
        match serde_json::from_str::<LogEntry>(&line) {
            Ok(entry) => apply_entry(&mut summary, entry),
            Err(_) => summary.malformed_lines += 1,
        }
    }

    Ok(summary)
}

fn apply_entry(summary: &mut SessionSummary, entry: LogEntry) {
    let payload = entry.payload;
    let payload_type = payload
        .as_ref()
        .and_then(|payload| payload.payload_type.as_deref());

    let event_name = event_name(entry.entry_type.as_deref(), payload_type);
    *summary.event_counts.entry(event_name).or_default() += 1;

    if summary.started_at.is_none() {
        summary.started_at = entry.timestamp.clone();
    }

    if let Some(timestamp) = entry.timestamp.or_else(|| {
        payload
            .as_ref()
            .and_then(|payload| payload.timestamp.clone())
    }) {
        summary.last_event_at = Some(timestamp);
    }

    let Some(payload) = payload else {
        return;
    };

    if let Some(model) = payload.model {
        summary.model = Some(model);
    }

    if let Some(provider) = payload.model_provider {
        summary.provider = Some(provider);
    }

    if let Some(info) = payload.info {
        if let Some(total) = info.total_token_usage {
            summary.total_usage = total;
        }
        if let Some(last) = info.last_token_usage {
            summary.last_usage = last;
        }
        if let Some(context_window) = info.model_context_window {
            summary.context_window = Some(context_window);
        }
    }

    if let Some(rate_limits) = payload.rate_limits {
        summary.rate_limits = Some(rate_limits);
    }
}

fn event_name(entry_type: Option<&str>, payload_type: Option<&str>) -> String {
    match (entry_type, payload_type) {
        (Some(entry), Some(payload)) => format!("{entry}/{payload}"),
        (Some(entry), None) => entry.to_string(),
        (None, Some(payload)) => payload.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.cached_input_tokens += rhs.cached_input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.reasoning_output_tokens += rhs.reasoning_output_tokens;
        self.total_tokens += rhs.total_tokens;
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
{"timestamp":"2026-06-03T10:00:00Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-06-03T10:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":5,"output_tokens":2,"reasoning_output_tokens":1,"total_tokens":12},"total_token_usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":20,"reasoning_output_tokens":10,"total_tokens":120},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":42.5,"window_minutes":300,"resets_at":1780492992},"secondary":{"used_percent":71.0,"window_minutes":10080,"resets_at":1780846619},"credits":null,"plan_type":"pro","rate_limit_reached_type":null}}}
"#;

        let summary =
            parse_session_reader(Some(UNIX_EPOCH), Cursor::new(input)).expect("valid fixture");

        assert_eq!(summary.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(summary.total_usage.total_tokens, 120);
        assert_eq!(summary.last_usage.cached_input_tokens, 5);
        assert_eq!(summary.context_window, Some(258400));
        assert_eq!(
            summary
                .rate_limits
                .as_ref()
                .and_then(|limits| limits.primary.as_ref())
                .and_then(|window| window.used_percent),
            Some(42.5)
        );
        assert_eq!(summary.event_counts.get("event_msg/token_count"), Some(&1));
    }

    #[test]
    fn tail_scan_skips_partial_first_line_and_reads_recent_metrics() {
        let root = unique_temp_dir("codex-meter-tail");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("large.jsonl");
        let old_line = "{\"timestamp\":\"2026-06-03T09:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"total_tokens\":1}}}}\n";
        let filler = "x".repeat(512);
        let recent_line = "{\"timestamp\":\"2026-06-03T10:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"total_tokens\":99}}}}\n";
        fs::write(&path, format!("{old_line}{filler}\n{recent_line}")).expect("write fixture");

        let summary =
            parse_session_file_with_tail_limit(&path, Some(UNIX_EPOCH), 256).expect("tail scan");

        assert!(summary.tail_scanned);
        assert_eq!(summary.total_usage.total_tokens, 99);
        assert_eq!(summary.malformed_lines, 0);

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scanner_aggregates_recent_files_and_bad_lines() {
        let root = unique_temp_dir("codex-meter-scan");
        let sessions = root.join("sessions").join("2026").join("06").join("03");
        fs::create_dir_all(&sessions).expect("create sessions");
        fs::create_dir_all(root.join("archived_sessions")).expect("create archive");

        fs::write(
            sessions.join("one.jsonl"),
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":10}}}}"#,
        )
        .expect("write one");
        fs::write(
            sessions.join("two.jsonl"),
            "not json\n{\"timestamp\":\"2026-06-03T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"total_tokens\":20}}}}\n",
        )
        .expect("write two");
        fs::write(root.join("archived_sessions").join("old.jsonl"), "{}\n").expect("archive");

        let snapshot = scan_codex_home(&root, 64).expect("scan");

        assert_eq!(snapshot.available_session_files, 2);
        assert_eq!(snapshot.archived_session_files, 1);
        assert_eq!(snapshot.scanned_files, 2);
        assert_eq!(snapshot.malformed_lines, 1);
        assert_eq!(snapshot.scanned_total_usage.total_tokens, 30);

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
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1780492992},"secondary":{"used_percent":0.0,"window_minutes":10080,"resets_at":1780846619},"credits":null,"plan_type":null,"rate_limit_reached_type":null}}}"#,
        )
        .expect("write weak");

        let strong = sessions.join("strong.jsonl");
        fs::write(
            &strong,
            r#"{"timestamp":"2026-06-03T10:01:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":69.0,"window_minutes":300,"resets_at":1780492992},"secondary":{"used_percent":74.0,"window_minutes":10080,"resets_at":1780846619},"credits":null,"plan_type":"prolite","rate_limit_reached_type":null}}}"#,
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

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
