use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use memchr::{memchr_iter, memmem};
use serde::{Deserialize, Serialize};

use crate::{
    calendar::{date_days, local_date_from_timestamp},
    codex::TokenUsage,
    error::{AppError, AppResult},
    pricing,
    state_db::CostThread,
};

const CACHE_VERSION: u32 = 6;
const READ_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_COST_LINE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct CostReport {
    pub source: String,
    pub daily: Vec<CostDay>,
    pub top_model: Option<String>,
    pub unpriced_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostDay {
    pub date: String,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostEstimateResult {
    pub report: Option<CostReport>,
    pub warnings: Vec<String>,
}

#[derive(Default)]
pub struct CostEstimator {
    cache: Option<PersistentCostCache>,
    cache_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistentCostCache {
    version: u32,
    files: BTreeMap<String, CachedCostFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCostFile {
    len: u64,
    modified_secs: Option<u64>,
    thread_id: String,
    session_id: Option<String>,
    current_model: Option<String>,
    parser: TokenDeltaParser,
    days: Vec<CachedCostDayModel>,
    malformed_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCostDayModel {
    date: String,
    model: String,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
struct TimestampedTotals {
    timestamp: String,
    totals: TokenUsage,
}

#[derive(Debug, Clone, Copy)]
enum ForkBaseline {
    Resolved(Option<TokenUsage>),
    Unresolved,
}

#[derive(Debug, Default)]
struct ParentTotalsResolver {
    paths_by_session: HashMap<String, PathBuf>,
    snapshots_by_session: HashMap<String, Option<Vec<TimestampedTotals>>>,
}

#[derive(Debug, Deserialize)]
struct CostLogEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<CostPayload>,
}

#[derive(Debug, Deserialize)]
struct CostPayload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id_camel: Option<String>,
    #[serde(default)]
    #[serde(
        alias = "forkedFromId",
        alias = "parent_session_id",
        alias = "parentSessionId"
    )]
    forked_from_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "type")]
    payload_type: Option<String>,
    #[serde(default)]
    info: Option<CostUsageInfo>,
}

#[derive(Debug, Deserialize)]
struct CostUsageInfo {
    last_token_usage: Option<TokenUsage>,
    total_token_usage: Option<TokenUsage>,
    model: Option<String>,
    model_name: Option<String>,
}

impl CostEstimator {
    #[cfg(test)]
    pub(crate) fn with_cache_path(cache_path: PathBuf) -> Self {
        Self {
            cache: None,
            cache_path: Some(cache_path),
        }
    }

    pub fn estimate(
        &mut self,
        threads: &[CostThread],
        cutoff_day: i64,
    ) -> AppResult<CostEstimateResult> {
        let cache_path = self.cache_path.clone().map(Ok).unwrap_or_else(cache_path)?;
        let mut cache = self.load_cache(&cache_path)?;
        let mut warnings = Vec::new();
        let mut changed = false;
        let mut active_paths = HashSet::new();
        let mut parent_resolver = ParentTotalsResolver::new(threads);

        for thread in threads {
            active_paths.insert(thread.rollout_path.clone());
            let Ok(metadata) = fs::metadata(&thread.rollout_path) else {
                warnings.push(format!(
                    "cost estimate skipped missing file {}",
                    thread.rollout_path.display()
                ));
                continue;
            };

            let modified_secs = metadata.modified().ok().and_then(system_time_secs);
            let key = thread.rollout_path.to_string_lossy().into_owned();
            let cached = cache.files.get(&key);
            if cached
                .is_some_and(|cached| cached.len == metadata.len() && cached.thread_id == thread.id)
            {
                continue;
            }

            match scan_cost_file(
                thread,
                metadata.len(),
                modified_secs,
                cached,
                &mut parent_resolver,
            ) {
                Ok(file) => {
                    cache.files.insert(key, file);
                    changed = true;
                }
                Err(error) => {
                    warnings.push(format!(
                        "cost estimate skipped {}: {error}",
                        thread.rollout_path.display()
                    ));
                }
            }
        }

        let before_prune = cache.files.len();
        cache.files.retain(|path, file| {
            active_paths.contains(Path::new(path)) || has_recent_day(file, cutoff_day)
        });
        changed |= cache.files.len() != before_prune;

        if changed && let Err(error) = save_cache(&cache_path, &cache) {
            warnings.push(format!("failed to save cost estimate cache: {error}"));
        }
        self.cache = Some(cache.clone());

        Ok(CostEstimateResult {
            report: build_report(&cache, cutoff_day),
            warnings,
        })
    }

    fn load_cache(&mut self, path: &Path) -> AppResult<PersistentCostCache> {
        if let Some(cache) = &self.cache {
            return Ok(cache.clone());
        }

        let cache = match fs::read(path) {
            Ok(data) => serde_json::from_slice::<PersistentCostCache>(&data)
                .ok()
                .filter(|cache| cache.version == CACHE_VERSION)
                .unwrap_or_else(empty_cache),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => empty_cache(),
            Err(error) => {
                return Err(AppError::io(
                    format!("failed to read cost estimate cache {}", path.display()),
                    error,
                ));
            }
        };
        self.cache = Some(cache.clone());
        Ok(cache)
    }
}

impl ParentTotalsResolver {
    fn new(threads: &[CostThread]) -> Self {
        let mut paths_by_session = HashMap::new();
        for thread in threads {
            paths_by_session.insert(thread.id.clone(), thread.rollout_path.clone());
            if let (Some(parent_id), Some(parent_path)) =
                (&thread.parent_thread_id, &thread.parent_rollout_path)
            {
                paths_by_session.insert(parent_id.clone(), parent_path.clone());
            }
        }
        Self {
            paths_by_session,
            snapshots_by_session: HashMap::new(),
        }
    }

    fn inherited_totals(&mut self, parent_session_id: &str, at_or_before: &str) -> ForkBaseline {
        if at_or_before.trim().is_empty() {
            return ForkBaseline::Unresolved;
        }
        let Some(path) = self.paths_by_session.get(parent_session_id).cloned() else {
            return ForkBaseline::Unresolved;
        };

        if !self.snapshots_by_session.contains_key(parent_session_id) {
            let snapshots = parse_parent_token_snapshots(&path)
                .ok()
                .filter(|parsed| {
                    parsed
                        .session_id
                        .as_deref()
                        .is_none_or(|session_id| session_id == parent_session_id)
                })
                .map(|parsed| parsed.snapshots);
            self.snapshots_by_session
                .insert(parent_session_id.to_string(), snapshots);
        }

        let Some(Some(snapshots)) = self.snapshots_by_session.get(parent_session_id) else {
            return ForkBaseline::Unresolved;
        };
        let inherited = snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.timestamp.as_str() <= at_or_before)
            .map(|snapshot| snapshot.totals);
        ForkBaseline::Resolved(inherited)
    }
}

struct ParentTokenSnapshots {
    session_id: Option<String>,
    snapshots: Vec<TimestampedTotals>,
}

fn empty_cache() -> PersistentCostCache {
    PersistentCostCache {
        version: CACHE_VERSION,
        files: BTreeMap::new(),
    }
}

fn scan_cost_file(
    thread: &CostThread,
    len: u64,
    modified_secs: Option<u64>,
    cached: Option<&CachedCostFile>,
    parent_resolver: &mut ParentTotalsResolver,
) -> AppResult<CachedCostFile> {
    let mut file = File::open(&thread.rollout_path).map_err(|source| {
        AppError::io(
            format!("failed to open {}", thread.rollout_path.display()),
            source,
        )
    })?;
    let can_resume =
        cached.is_some_and(|cached| cached.thread_id == thread.id && cached.len <= len);
    let start_offset = if can_resume {
        cached.map(|cached| cached.len).unwrap_or(0)
    } else {
        0
    };
    if start_offset > 0 {
        file.seek(SeekFrom::Start(start_offset)).map_err(|source| {
            AppError::io(
                format!("failed to seek {}", thread.rollout_path.display()),
                source,
            )
        })?;
    }
    let reader = BufReader::with_capacity(1024 * 1024, file);
    let mut current_model = if can_resume {
        cached
            .and_then(|cached| cached.current_model.clone())
            .or_else(|| thread.model.clone())
    } else {
        thread.model.clone()
    };
    let mut session_id = if can_resume {
        cached.and_then(|cached| cached.session_id.clone())
    } else {
        None
    };
    let mut parser = if can_resume {
        cached
            .map(|cached| cached.parser.clone())
            .unwrap_or_default()
    } else {
        TokenDeltaParser::default()
    };
    let mut by_day_model = if can_resume {
        cached.map(cached_day_models).unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let mut malformed_lines = if can_resume {
        cached.map(|cached| cached.malformed_lines).unwrap_or(0)
    } else {
        0
    };

    let oversized_lines = scan_relevant_cost_lines(reader, |line| {
        match serde_json::from_slice::<CostLogEntry>(line) {
            Ok(entry) => apply_cost_entry(
                &mut current_model,
                &mut session_id,
                &mut parser,
                &mut by_day_model,
                entry,
                parent_resolver,
            ),
            Err(_) => {
                malformed_lines += 1;
            }
        }
    })
    .map_err(|source| {
        AppError::io(
            format!("failed to read {}", thread.rollout_path.display()),
            source,
        )
    })?;
    malformed_lines += oversized_lines;

    Ok(CachedCostFile {
        len,
        modified_secs,
        thread_id: thread.id.clone(),
        session_id: session_id.or_else(|| Some(thread.id.clone())),
        current_model,
        parser,
        days: by_day_model
            .into_iter()
            .map(|((date, model), day)| CachedCostDayModel {
                date,
                model,
                input_tokens: day.usage.input_tokens,
                cached_input_tokens: day.usage.cached_input_tokens,
                output_tokens: day.usage.output_tokens,
                cost_usd: day.cost_usd,
            })
            .collect(),
        malformed_lines,
    })
}

fn scan_relevant_cost_lines<R: Read>(
    mut reader: R,
    mut handle_line: impl FnMut(&[u8]),
) -> std::io::Result<usize> {
    let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
    let mut line = Vec::with_capacity(READ_CHUNK_BYTES);
    let mut discarding = false;
    let mut oversized_lines = 0;

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            if !discarding && !line.is_empty() && is_relevant_cost_line(&line) {
                handle_line(&line);
            }
            return Ok(oversized_lines);
        }

        let mut start = 0;
        for offset in memchr_iter(b'\n', &chunk[..read]) {
            append_line_segment(
                &mut line,
                &chunk[start..offset],
                &mut discarding,
                &mut oversized_lines,
            );
            if !discarding && is_relevant_cost_line(&line) {
                handle_line(&line);
            }
            line.clear();
            discarding = false;
            start = offset + 1;
        }

        append_line_segment(
            &mut line,
            &chunk[start..read],
            &mut discarding,
            &mut oversized_lines,
        );
    }
}

fn append_line_segment(
    line: &mut Vec<u8>,
    segment: &[u8],
    discarding: &mut bool,
    oversized_lines: &mut usize,
) {
    if *discarding || segment.is_empty() {
        return;
    }

    if line.len().saturating_add(segment.len()) > MAX_COST_LINE_BYTES {
        line.clear();
        *discarding = true;
        *oversized_lines += 1;
        return;
    }

    line.extend_from_slice(segment);
}

fn apply_cost_entry(
    current_model: &mut Option<String>,
    session_id: &mut Option<String>,
    parser: &mut TokenDeltaParser,
    by_day_model: &mut BTreeMap<(String, String), CachedDayModelAccumulator>,
    entry: CostLogEntry,
    parent_resolver: &mut ParentTotalsResolver,
) {
    let entry_timestamp = entry.timestamp.as_deref();
    let kind = entry.kind.as_deref();
    let Some(payload) = entry.payload else {
        return;
    };

    if kind == Some("session_meta") {
        if session_id.is_none() {
            *session_id = payload
                .session_id
                .clone()
                .or(payload.session_id_camel.clone())
                .or(payload.id.clone());
        }
        if let Some(parent_id) = payload
            .forked_from_id
            .as_deref()
            .map(str::trim)
            .filter(|parent_id| !parent_id.is_empty())
        {
            let fork_timestamp = payload
                .timestamp
                .as_deref()
                .or(entry_timestamp)
                .unwrap_or("");
            parser.resolve_fork_baseline(parent_id, fork_timestamp, parent_resolver);
        }
        return;
    }

    if let Some(model) = payload
        .model
        .as_ref()
        .filter(|model| !model.trim().is_empty())
    {
        *current_model = Some(pricing::normalize_codex_model(model));
    }

    let Some(info) = payload.info else {
        return;
    };
    if payload.payload_type.as_deref() != Some("token_count")
        && info.last_token_usage.is_none()
        && info.total_token_usage.is_none()
    {
        return;
    }

    let Some(date) = entry_timestamp.and_then(local_date_from_timestamp) else {
        return;
    };
    let model = payload
        .model
        .or(info.model)
        .or(info.model_name)
        .or_else(|| current_model.clone())
        .map(|model| pricing::normalize_codex_model(&model))
        .unwrap_or_else(|| "gpt-5".to_string());

    let counted = parser
        .count_delta(info.last_token_usage, info.total_token_usage)
        .with_cached_clamped();
    if !counted.has_tokens() {
        return;
    }

    let cost = pricing::codex_cost_usd(&model, counted);
    let key = (date, model);
    let existing = by_day_model.entry(key).or_default();
    existing.usage = existing.usage.saturating_add(counted);
    existing.cost_usd = add_optional(existing.cost_usd, cost);
}

#[derive(Debug, Clone, Default)]
struct CachedDayModelAccumulator {
    usage: TokenUsage,
    cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TokenDeltaParser {
    raw_totals_baseline: Option<TokenUsage>,
    counted_totals: Option<TokenUsage>,
    unresolved_fork_watermark: Option<TokenUsage>,
    forked_from_id: Option<String>,
    inherited_totals: Option<TokenUsage>,
    remaining_inherited_totals: Option<TokenUsage>,
    fork_baseline_resolved: bool,
    has_unresolved_fork_baseline: bool,
    has_divergent_totals: bool,
}

impl TokenDeltaParser {
    fn resolve_fork_baseline(
        &mut self,
        parent_id: &str,
        fork_timestamp: &str,
        parent_resolver: &mut ParentTotalsResolver,
    ) {
        if self.fork_baseline_resolved {
            return;
        }
        self.forked_from_id = Some(parent_id.to_string());
        self.fork_baseline_resolved = true;
        match parent_resolver.inherited_totals(parent_id, fork_timestamp) {
            ForkBaseline::Resolved(totals) => {
                self.inherited_totals = totals;
                self.remaining_inherited_totals = totals;
                self.has_unresolved_fork_baseline = false;
            }
            ForkBaseline::Unresolved => {
                self.has_unresolved_fork_baseline = true;
            }
        }
    }

    fn count_delta(&mut self, last: Option<TokenUsage>, total: Option<TokenUsage>) -> TokenUsage {
        let last = last.map(TokenUsage::with_cached_clamped);
        let total = total.map(TokenUsage::with_cached_clamped);
        let handled_unresolved_fork_total = self.has_unresolved_fork_baseline && total.is_some();

        if self.has_unresolved_fork_baseline
            && let Some(total) = total
        {
            let previous_watermark = self.unresolved_fork_watermark;
            self.unresolved_fork_watermark = Some(total);
            let (Some(last), Some(watermark)) = (last, previous_watermark) else {
                return TokenUsage::default();
            };
            let raw_total_delta = total.delta_from(Some(watermark));
            let delta = min_usage(last, raw_total_delta).with_cached_clamped();
            self.apply_delta(delta, None);
            return delta;
        }

        let delta = if !handled_unresolved_fork_total
            && let Some(total) = total
            && self.forked_from_id.is_some()
            && !self.has_unresolved_fork_baseline
        {
            let current_totals = self.current_totals(total);
            let delta = if self.has_divergent_totals {
                divergent_total_delta(
                    self.raw_totals_baseline,
                    self.counted_totals,
                    current_totals,
                )
            } else {
                current_totals.delta_from(self.raw_totals_baseline)
            };
            self.apply_delta(delta, Some(current_totals));
            self.remaining_inherited_totals = None;
            delta
        } else if !handled_unresolved_fork_total {
            match (last, total) {
                (Some(last), Some(total)) => self.count_last_with_total(last, total),
                (Some(last), None) => {
                    let delta = self.adjusted_last_delta(last).with_cached_clamped();
                    self.apply_delta(delta, None);
                    delta
                }
                (None, Some(total)) => {
                    let current_totals = self.current_totals(total);
                    let delta = if self.has_divergent_totals {
                        divergent_total_delta(
                            self.raw_totals_baseline,
                            self.counted_totals,
                            current_totals,
                        )
                    } else {
                        current_totals.delta_from(self.raw_totals_baseline)
                    };
                    self.apply_delta(delta, Some(current_totals));
                    delta
                }
                (None, None) => TokenUsage::default(),
            }
        } else {
            TokenUsage::default()
        };

        delta.with_cached_clamped()
    }

    fn count_last_with_total(&mut self, last: TokenUsage, total: TokenUsage) -> TokenUsage {
        let had_remaining_inherited = self.remaining_inherited_totals.is_some();
        let mut adjusted_delta = self.adjusted_last_delta(last).with_cached_clamped();
        let current_totals = self.current_totals(total);
        let total_delta = current_totals.delta_from(self.raw_totals_baseline);
        if !had_remaining_inherited
            && should_prefer_total_delta(
                self.raw_totals_baseline,
                current_totals,
                total_delta,
                last,
                self.has_divergent_totals,
            )
        {
            adjusted_delta = total_delta.with_cached_clamped();
            self.remaining_inherited_totals = None;
        }
        self.apply_delta(adjusted_delta, Some(current_totals));
        adjusted_delta
    }

    fn current_totals(&self, total: TokenUsage) -> TokenUsage {
        total
            .delta_from(self.inherited_totals)
            .with_cached_clamped()
    }

    fn adjusted_last_delta(&mut self, raw_delta: TokenUsage) -> TokenUsage {
        let Some(remaining) = self.remaining_inherited_totals else {
            return raw_delta;
        };

        let adjusted = raw_delta.delta_from(Some(remaining)).with_cached_clamped();
        let next_remaining = TokenUsage {
            input_tokens: remaining
                .input_tokens
                .saturating_sub(raw_delta.input_tokens),
            cached_input_tokens: remaining
                .cached_input_tokens
                .saturating_sub(raw_delta.cached_input_tokens),
            output_tokens: remaining
                .output_tokens
                .saturating_sub(raw_delta.output_tokens),
            ..TokenUsage::default()
        };
        self.remaining_inherited_totals = next_remaining.has_tokens().then_some(next_remaining);
        adjusted
    }

    fn apply_delta(&mut self, delta: TokenUsage, raw_totals_baseline: Option<TokenUsage>) {
        let counted = self
            .counted_totals
            .unwrap_or_default()
            .saturating_add(delta)
            .with_cached_clamped();
        self.counted_totals = Some(counted);
        let raw_baseline = raw_totals_baseline.unwrap_or(counted).with_cached_clamped();
        self.raw_totals_baseline = Some(raw_baseline);
        if !usage_totals_equal(raw_baseline, counted) {
            self.has_divergent_totals = true;
        }
    }
}

fn min_usage(left: TokenUsage, right: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: left.input_tokens.min(right.input_tokens),
        cached_input_tokens: left.cached_input_tokens.min(right.cached_input_tokens),
        output_tokens: left.output_tokens.min(right.output_tokens),
        ..TokenUsage::default()
    }
}

fn usage_totals_equal(left: TokenUsage, right: TokenUsage) -> bool {
    left.input_tokens == right.input_tokens
        && left.cached_input_tokens == right.cached_input_tokens
        && left.output_tokens == right.output_tokens
}

fn should_prefer_total_delta(
    raw_baseline: Option<TokenUsage>,
    current_total: TokenUsage,
    total_delta: TokenUsage,
    last_delta: TokenUsage,
    saw_divergent_totals: bool,
) -> bool {
    !saw_divergent_totals
        && raw_baseline.is_some()
        && current_total.at_least(raw_baseline)
        && total_delta.at_most(last_delta)
}

fn divergent_total_delta(
    raw_baseline: Option<TokenUsage>,
    counted_baseline: Option<TokenUsage>,
    current: TokenUsage,
) -> TokenUsage {
    let raw_baseline = raw_baseline.unwrap_or_default();
    let counted_baseline = counted_baseline.unwrap_or_default();

    fn delta(raw: u64, counted: u64, current: u64) -> u64 {
        if current >= raw {
            current.saturating_sub(raw)
        } else {
            current.saturating_sub(counted)
        }
    }

    TokenUsage {
        input_tokens: delta(
            raw_baseline.input_tokens,
            counted_baseline.input_tokens,
            current.input_tokens,
        ),
        cached_input_tokens: delta(
            raw_baseline.cached_input_tokens,
            counted_baseline.cached_input_tokens,
            current.cached_input_tokens,
        ),
        output_tokens: delta(
            raw_baseline.output_tokens,
            counted_baseline.output_tokens,
            current.output_tokens,
        ),
        ..TokenUsage::default()
    }
    .with_cached_clamped()
}

fn parse_parent_token_snapshots(path: &Path) -> AppResult<ParentTokenSnapshots> {
    let file = File::open(path)
        .map_err(|source| AppError::io(format!("failed to open {}", path.display()), source))?;
    let reader = BufReader::with_capacity(1024 * 1024, file);
    let mut session_id = None;
    let mut parser = TokenDeltaParser::default();
    let mut snapshots = Vec::new();
    let mut malformed_lines = 0_usize;

    let oversized_lines = scan_relevant_cost_lines(reader, |line| {
        match serde_json::from_slice::<CostLogEntry>(line) {
            Ok(entry) => {
                let timestamp = entry.timestamp.clone();
                let kind = entry.kind.as_deref();
                let Some(payload) = entry.payload else {
                    return;
                };

                if kind == Some("session_meta") {
                    if session_id.is_none() {
                        session_id = payload
                            .session_id
                            .or(payload.session_id_camel)
                            .or(payload.id);
                    }
                    return;
                }

                let Some(info) = payload.info else {
                    return;
                };
                if payload.payload_type.as_deref() != Some("token_count")
                    && info.last_token_usage.is_none()
                    && info.total_token_usage.is_none()
                {
                    return;
                }
                let Some(timestamp) = timestamp else {
                    return;
                };

                let counted = parser
                    .count_delta(info.last_token_usage, info.total_token_usage)
                    .with_cached_clamped();
                if counted.has_tokens() {
                    snapshots.push(TimestampedTotals {
                        timestamp,
                        totals: parser.counted_totals.unwrap_or_default(),
                    });
                }
            }
            Err(_) => {
                malformed_lines += 1;
            }
        }
    })
    .map_err(|source| AppError::io(format!("failed to read {}", path.display()), source))?;

    if malformed_lines + oversized_lines > 0 && snapshots.is_empty() {
        return Err(AppError::Argument(format!(
            "parent token snapshot parse failed for {}",
            path.display()
        )));
    }

    Ok(ParentTokenSnapshots {
        session_id,
        snapshots,
    })
}

fn build_report(cache: &PersistentCostCache, cutoff_day: i64) -> Option<CostReport> {
    let mut seen_sessions = HashSet::new();
    let mut by_day = BTreeMap::<String, BillingDayAccumulator>::new();
    let mut model_tokens = HashMap::<String, u64>::new();
    let mut unpriced_tokens = 0_u64;

    for file in cache.files.values() {
        if let Some(session_id) = &file.session_id
            && !seen_sessions.insert(session_id.clone())
        {
            continue;
        }

        for day in &file.days {
            if date_days(&day.date).is_none_or(|days| days < cutoff_day) {
                continue;
            }

            let usage = TokenUsage {
                input_tokens: day.input_tokens,
                cached_input_tokens: day.cached_input_tokens,
                output_tokens: day.output_tokens,
                total_tokens: day.input_tokens.saturating_add(day.output_tokens),
                ..TokenUsage::default()
            };
            let tokens = usage.input_tokens.saturating_add(usage.output_tokens);
            if tokens == 0 {
                continue;
            }
            let cost = day.cost_usd;
            if cost.is_none() {
                unpriced_tokens = unpriced_tokens.saturating_add(tokens);
            }

            let entry = by_day.entry(day.date.clone()).or_default();
            entry.tokens = entry.tokens.saturating_add(tokens);
            entry.cost_usd = add_optional(entry.cost_usd, cost);
            *model_tokens.entry(day.model.clone()).or_default() += tokens;
        }
    }

    let daily = by_day
        .into_iter()
        .map(|(date, day)| CostDay {
            date,
            tokens: day.tokens,
            cost_usd: day.cost_usd,
        })
        .collect::<Vec<_>>();
    if daily.is_empty() {
        return None;
    }

    Some(CostReport {
        source: "local estimate".to_string(),
        daily,
        top_model: model_tokens
            .into_iter()
            .max_by_key(|(_, tokens)| *tokens)
            .map(|(model, _)| model),
        unpriced_tokens,
    })
}

fn cached_day_models(
    file: &CachedCostFile,
) -> BTreeMap<(String, String), CachedDayModelAccumulator> {
    file.days
        .iter()
        .map(|day| {
            (
                (day.date.clone(), day.model.clone()),
                CachedDayModelAccumulator {
                    usage: TokenUsage {
                        input_tokens: day.input_tokens,
                        cached_input_tokens: day.cached_input_tokens,
                        output_tokens: day.output_tokens,
                        total_tokens: day.input_tokens.saturating_add(day.output_tokens),
                        ..TokenUsage::default()
                    },
                    cost_usd: day.cost_usd,
                },
            )
        })
        .collect()
}

#[derive(Default)]
struct BillingDayAccumulator {
    tokens: u64,
    cost_usd: Option<f64>,
}

fn has_recent_day(file: &CachedCostFile, cutoff_day: i64) -> bool {
    file.days
        .iter()
        .any(|day| date_days(&day.date).is_some_and(|days| days >= cutoff_day))
}

fn cache_path() -> AppResult<PathBuf> {
    let root = if let Some(path) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(path)
    } else if cfg!(target_os = "macos") {
        let home = env::var_os("HOME").ok_or_else(|| {
            AppError::Argument("HOME is not set; cannot resolve cache directory".to_string())
        })?;
        PathBuf::from(home).join("Library/Caches")
    } else {
        let home = env::var_os("HOME").ok_or_else(|| {
            AppError::Argument("HOME is not set; cannot resolve cache directory".to_string())
        })?;
        PathBuf::from(home).join(".cache")
    };
    Ok(root.join("codex-meter").join("cost-v1.json"))
}

fn save_cache(path: &Path, cache: &PersistentCostCache) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            AppError::io(format!("failed to create {}", parent.display()), source)
        })?;
    }
    let data = serde_json::to_vec(cache)
        .map_err(|source| AppError::json("failed to encode cost estimate cache", source))?;
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&tmp, data).map_err(|source| {
        AppError::io(
            format!("failed to write cost estimate cache {}", tmp.display()),
            source,
        )
    })?;
    fs::rename(&tmp, path).map_err(|source| {
        AppError::io(
            format!("failed to replace cost estimate cache {}", path.display()),
            source,
        )
    })
}

fn system_time_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn is_relevant_cost_line(line: &[u8]) -> bool {
    memmem::find(line, b"token_count").is_some()
        || memmem::find(line, b"session_meta").is_some()
        || memmem::find(line, b"turn_context").is_some()
}

fn add_optional(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_report_from_cached_file() {
        let cache = PersistentCostCache {
            version: CACHE_VERSION,
            files: BTreeMap::from([(
                "one".to_string(),
                CachedCostFile {
                    len: 10,
                    modified_secs: Some(1),
                    thread_id: "thread".to_string(),
                    session_id: Some("thread".to_string()),
                    current_model: Some("gpt-5.5".to_string()),
                    parser: TokenDeltaParser::default(),
                    malformed_lines: 0,
                    days: vec![CachedCostDayModel {
                        date: "2026-06-03".to_string(),
                        model: "gpt-5.5".to_string(),
                        input_tokens: 300_000,
                        cached_input_tokens: 100_000,
                        output_tokens: 10_000,
                        cost_usd: Some(2.55),
                    }],
                },
            )]),
        };

        let report = build_report(&cache, date_days("2026-06-01").expect("date")).expect("report");

        assert_eq!(report.source, "local estimate");
        assert_eq!(report.top_model.as_deref(), Some("gpt-5.5"));
        assert_eq!(report.daily[0].tokens, 310_000);
        let cost = report.daily[0].cost_usd.expect("cost");
        assert!((cost - 2.55).abs() < 1e-9);
    }

    #[test]
    fn report_deduplicates_session_ids() {
        let file = CachedCostFile {
            len: 10,
            modified_secs: Some(1),
            thread_id: "thread".to_string(),
            session_id: Some("same".to_string()),
            current_model: Some("gpt-5.5".to_string()),
            parser: TokenDeltaParser::default(),
            malformed_lines: 0,
            days: vec![CachedCostDayModel {
                date: "2026-06-03".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 300_000,
                cached_input_tokens: 100_000,
                output_tokens: 10_000,
                cost_usd: Some(2.55),
            }],
        };
        let cache = PersistentCostCache {
            version: CACHE_VERSION,
            files: BTreeMap::from([("one".to_string(), file.clone()), ("two".to_string(), file)]),
        };

        let report = build_report(&cache, date_days("2026-06-01").expect("date")).expect("report");

        assert_eq!(report.daily[0].tokens, 310_000);
        let cost = report.daily[0].cost_usd.expect("cost");
        assert!((cost - 2.55).abs() < 1e-9);
    }

    #[test]
    fn estimator_prices_long_context_threshold_per_token_count_row() {
        let root = unique_temp_dir("codex-meter-row-pricing");
        std::fs::create_dir_all(&root).expect("create root");
        let path = root.join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"session_meta","payload":{"id":"session"}}
{"timestamp":"2026-06-03T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-06-03T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200000,"cached_input_tokens":100000,"output_tokens":10000}}}}
{"timestamp":"2026-06-03T10:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200000,"cached_input_tokens":100000,"output_tokens":10000}}}}
"#,
        )
        .expect("write session");
        let threads = vec![CostThread {
            id: "session".to_string(),
            rollout_path: path,
            model: Some("gpt-5.5".to_string()),
            parent_thread_id: None,
            parent_rollout_path: None,
        }];

        let mut estimator = CostEstimator::with_cache_path(root.join("cache.json"));
        let report = estimator
            .estimate(&threads, date_days("2026-06-01").expect("date"))
            .expect("estimate")
            .report
            .expect("report");

        assert_eq!(report.daily[0].tokens, 420_000);
        let cost = report.daily[0].cost_usd.expect("cost");
        assert!((cost - 1.70).abs() < 1e-9);

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn estimator_buckets_cost_by_machine_local_date() {
        let root = unique_temp_dir("codex-meter-local-cost-date");
        std::fs::create_dir_all(&root).expect("create root");
        let path = root.join("session.jsonl");
        let timestamp = "2026-06-05T00:10:00Z";
        let expected_date = expected_local_date(timestamp);
        std::fs::write(
            &path,
            format!(
                "{{\"timestamp\":\"{timestamp}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"session\"}}}}\n\
                 {{\"timestamp\":\"{timestamp}\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.5\"}}}}\n\
                 {{\"timestamp\":\"{timestamp}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":1000,\"cached_input_tokens\":500,\"output_tokens\":100,\"total_tokens\":1100}}}}}}}}\n"
            ),
        )
        .expect("write session");
        let threads = vec![CostThread {
            id: "session".to_string(),
            rollout_path: path,
            model: Some("gpt-5.5".to_string()),
            parent_thread_id: None,
            parent_rollout_path: None,
        }];

        let mut estimator = CostEstimator::with_cache_path(root.join("cache.json"));
        let report = estimator
            .estimate(&threads, date_days("2026-06-01").expect("date"))
            .expect("estimate")
            .report
            .expect("report");

        assert_eq!(report.daily[0].date, expected_date);
        assert_eq!(report.daily[0].tokens, 1_100);

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    fn expected_local_date(timestamp: &str) -> String {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .expect("timestamp")
            .with_timezone(&chrono::Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }

    #[test]
    fn relevant_line_scanner_discards_oversized_irrelevant_rows() {
        let mut input = vec![b'x'; MAX_COST_LINE_BYTES + 1];
        input.push(b'\n');
        input.extend_from_slice(
            br#"{"timestamp":"2026-06-03T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":2}}}}"#,
        );
        input.push(b'\n');
        let mut seen = 0;

        let oversized =
            scan_relevant_cost_lines(input.as_slice(), |_| seen += 1).expect("scan lines");

        assert_eq!(oversized, 1);
        assert_eq!(seen, 1);
    }

    #[test]
    fn estimator_subtracts_parent_totals_for_forked_child_sessions() {
        let root = unique_temp_dir("codex-meter-fork-cost");
        std::fs::create_dir_all(&root).expect("create root");
        let parent_path = root.join("parent.jsonl");
        let child_path = root.join("child.jsonl");
        std::fs::write(
            &parent_path,
            r#"{"timestamp":"2026-06-03T10:00:00Z","type":"session_meta","payload":{"id":"parent","timestamp":"2026-06-03T10:00:00Z"}}
{"timestamp":"2026-06-03T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-06-03T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"output_tokens":100},"total_token_usage":{"input_tokens":1000,"output_tokens":100}}}}
"#,
        )
        .expect("write parent");
        std::fs::write(
            &child_path,
            r#"{"timestamp":"2026-06-03T10:05:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"parent","timestamp":"2026-06-03T10:05:00Z"}}
{"timestamp":"2026-06-03T10:05:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-06-03T10:05:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1200,"output_tokens":150},"total_token_usage":{"input_tokens":1200,"output_tokens":150}}}}
"#,
        )
        .expect("write child");
        let threads = vec![
            CostThread {
                id: "parent".to_string(),
                rollout_path: parent_path,
                model: Some("gpt-5.5".to_string()),
                parent_thread_id: None,
                parent_rollout_path: None,
            },
            CostThread {
                id: "child".to_string(),
                rollout_path: child_path,
                model: Some("gpt-5.5".to_string()),
                parent_thread_id: Some("parent".to_string()),
                parent_rollout_path: Some(root.join("parent.jsonl")),
            },
        ];

        let mut estimator = CostEstimator::with_cache_path(root.join("cache.json"));
        let report = estimator
            .estimate(&threads, date_days("2026-06-01").expect("date"))
            .expect("estimate")
            .report
            .expect("report");

        assert_eq!(report.daily[0].tokens, 1_350);

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn estimator_uses_first_total_as_watermark_for_explicit_unresolved_forks() {
        let root = unique_temp_dir("codex-meter-unresolved-fork");
        std::fs::create_dir_all(&root).expect("create root");
        let child_path = root.join("child.jsonl");
        std::fs::write(
            &child_path,
            r#"{"timestamp":"2026-06-03T10:05:00Z","type":"session_meta","payload":{"id":"child","forked_from_id":"missing-parent"}}
{"timestamp":"2026-06-03T10:05:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-06-03T10:05:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"output_tokens":100}}}}
{"timestamp":"2026-06-03T10:05:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":20},"total_token_usage":{"input_tokens":1100,"output_tokens":120}}}}
"#,
        )
        .expect("write child");
        let threads = vec![CostThread {
            id: "child".to_string(),
            rollout_path: child_path,
            model: Some("gpt-5.5".to_string()),
            parent_thread_id: None,
            parent_rollout_path: None,
        }];

        let mut estimator = CostEstimator::with_cache_path(root.join("cache.json"));
        let report = estimator
            .estimate(&threads, date_days("2026-06-01").expect("date"))
            .expect("estimate")
            .report
            .expect("report");

        assert_eq!(report.daily[0].tokens, 120);

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
