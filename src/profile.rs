use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::{
    calendar::{date_days, date_string_from_days, local_date_from_system_time, local_today_days},
    codex::{SessionSummary, TokenUsage},
    cost_estimate::{CostDay, CostReport},
};

const PROFILE_DAYS: i64 = 365;

#[derive(Debug, Clone, Default)]
pub struct UsageProfile {
    pub active_days: u64,
    pub current_streak_days: u64,
    pub longest_streak_days: u64,
    pub last_active_date: Option<String>,
    pub activity_total_tokens: u64,
    pub activity_last_30_days_tokens: u64,
    pub activity_source: ActivitySource,
    pub peak_day_tokens: Option<u64>,
    pub longest_task_seconds: Option<u64>,
    pub today_tokens: u64,
    pub last_30_days_tokens: u64,
    pub today_cost_usd: Option<f64>,
    pub last_30_days_cost_usd: Option<f64>,
    pub top_model: Option<String>,
    pub unpriced_tokens: u64,
    pub cost_source: Option<String>,
    pub daily: Vec<DailyUsage>,
    pub billing_daily: Vec<DailyUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    pub date: String,
    pub tokens: u64,
    pub cost_usd: Option<f64>,
    pub sessions: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivitySource {
    #[default]
    Local,
    OpenAIProfile,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileActivity {
    pub daily: Vec<DailyUsage>,
    pub lifetime_tokens: Option<u64>,
    pub peak_day_tokens: Option<u64>,
    pub current_streak_days: Option<u64>,
    pub longest_streak_days: Option<u64>,
    pub longest_task_seconds: Option<u64>,
}

impl ActivitySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::OpenAIProfile => "OpenAI profile",
        }
    }

    pub fn total_label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::OpenAIProfile => "lifetime",
        }
    }
}

pub fn build_usage_profile(
    summaries: &[SessionSummary],
    cost_report: Option<CostReport>,
    indexed_activity: Option<Vec<DailyUsage>>,
) -> UsageProfile {
    let mut by_day: BTreeMap<String, DailyAccumulator> = BTreeMap::new();
    let mut model_tokens: HashMap<String, u64> = HashMap::new();
    for summary in summaries {
        if !summary.daily_usage.is_empty() {
            let mut session_dates = BTreeSet::new();
            for day_usage in &summary.daily_usage {
                session_dates.insert(day_usage.date.clone());
                record_token_day(
                    &mut by_day,
                    &mut model_tokens,
                    &summary.model,
                    &day_usage.date,
                    day_usage.usage,
                );
            }
            record_session_dates(&mut by_day, session_dates);
            continue;
        }

        let mut session_dates = BTreeSet::new();
        if let Some(date) = summary
            .activity_date
            .clone()
            .or_else(|| summary.session_date.clone())
            .or_else(|| summary.modified_at.and_then(local_date_from_system_time))
        {
            session_dates.insert(date);
        }

        let usage = summary.billable_usage();
        if !usage.has_tokens() {
            record_session_dates(&mut by_day, session_dates);
            continue;
        }

        let Some(date) = summary
            .activity_date
            .clone()
            .or_else(|| summary.modified_at.and_then(local_date_from_system_time))
        else {
            continue;
        };

        session_dates.insert(date.clone());
        record_token_day(&mut by_day, &mut model_tokens, &summary.model, &date, usage);
        record_session_dates(&mut by_day, session_dates);
    }

    let mut daily = by_day
        .into_iter()
        .map(|(date, day)| DailyUsage {
            date,
            tokens: day.tokens,
            cost_usd: day.cost_usd,
            sessions: day.sessions,
        })
        .collect::<Vec<_>>();
    let newest_day = daily
        .last()
        .and_then(|day| date_days(&day.date))
        .unwrap_or(0);
    let oldest_visible_day = newest_day.saturating_sub(PROFILE_DAYS - 1);
    daily.retain(|day| date_days(&day.date).is_some_and(|days| days >= oldest_visible_day));

    let today = local_today_days();
    let today_tokens = tokens_for_day(&daily, today);
    let last_30_days_tokens = tokens_since(&daily, today.saturating_sub(29));

    let activity_metrics = activity_metrics(&daily, today);

    let top_model = model_tokens
        .into_iter()
        .max_by_key(|(_, tokens)| *tokens)
        .map(|(model, _)| model);

    let mut profile = UsageProfile {
        active_days: activity_metrics.active_days,
        current_streak_days: activity_metrics.current_streak_days,
        longest_streak_days: activity_metrics.longest_streak_days,
        last_active_date: activity_metrics.last_active_date,
        activity_total_tokens: activity_metrics.total_tokens,
        activity_last_30_days_tokens: activity_metrics.last_30_days_tokens,
        activity_source: ActivitySource::Local,
        peak_day_tokens: None,
        longest_task_seconds: None,
        today_tokens,
        last_30_days_tokens,
        today_cost_usd: None,
        last_30_days_cost_usd: None,
        top_model,
        unpriced_tokens: 0,
        cost_source: None,
        daily,
        billing_daily: Vec::new(),
    };

    if let Some(cost_report) = cost_report {
        profile.apply_cost_days(
            cost_report.source,
            cost_report.daily,
            cost_report.top_model,
            cost_report.unpriced_tokens,
        );
    }

    if let Some(indexed_activity) = indexed_activity {
        profile.apply_indexed_activity(indexed_activity);
    }

    profile
}

impl UsageProfile {
    pub fn apply_cost_report(&mut self, report: CostReport) {
        self.apply_cost_days(
            report.source,
            report.daily,
            report.top_model,
            report.unpriced_tokens,
        );
    }

    pub fn apply_remote_activity(&mut self, mut activity: ProfileActivity) {
        if activity.daily.is_empty() {
            return;
        }

        activity
            .daily
            .sort_by(|left, right| left.date.cmp(&right.date));
        activity.daily.retain(|day| date_days(&day.date).is_some());
        let Some(newest_day) = activity.daily.last().and_then(|day| date_days(&day.date)) else {
            return;
        };
        let oldest_visible_day = newest_day.saturating_sub(PROFILE_DAYS - 1);
        activity
            .daily
            .retain(|day| date_days(&day.date).is_some_and(|days| days >= oldest_visible_day));
        if activity.daily.is_empty() {
            return;
        }

        for day in &mut activity.daily {
            day.cost_usd = None;
            day.sessions = u64::from(day.tokens > 0);
        }

        self.daily = activity.daily;
        self.activity_source = ActivitySource::OpenAIProfile;
        self.peak_day_tokens = activity.peak_day_tokens;
        self.longest_task_seconds = activity.longest_task_seconds;
        self.recompute_activity_metrics();
        self.activity_last_30_days_tokens =
            tokens_since(&self.daily, local_today_days().saturating_sub(29));

        if let Some(lifetime_tokens) = activity.lifetime_tokens {
            self.activity_total_tokens = lifetime_tokens;
        }
        if let Some(current_streak_days) = activity.current_streak_days {
            self.current_streak_days = current_streak_days;
        }
        if let Some(longest_streak_days) = activity.longest_streak_days {
            self.longest_streak_days = longest_streak_days;
        }

        if self.billing_daily.is_empty() {
            self.apply_activity_tokens_to_usage_summary();
        }
    }

    fn apply_indexed_activity(&mut self, mut activity: Vec<DailyUsage>) {
        if activity.is_empty() {
            return;
        }

        activity.sort_by(|left, right| left.date.cmp(&right.date));
        let newest_day = activity
            .last()
            .and_then(|day| date_days(&day.date))
            .unwrap_or(0);
        let oldest_visible_day = newest_day.saturating_sub(PROFILE_DAYS - 1);
        activity.retain(|day| date_days(&day.date).is_some_and(|days| days >= oldest_visible_day));
        if activity.is_empty() {
            return;
        }

        self.daily = activity;
        self.activity_source = ActivitySource::Local;
        self.recompute_activity_metrics();
    }

    fn apply_cost_days(
        &mut self,
        source: String,
        cost_days: Vec<CostDay>,
        top_model: Option<String>,
        unpriced_tokens: u64,
    ) {
        if cost_days.is_empty() {
            return;
        }

        self.billing_daily = cost_days
            .iter()
            .map(|day| DailyUsage {
                date: day.date.clone(),
                tokens: day.tokens,
                cost_usd: day.cost_usd,
                sessions: 0,
            })
            .collect();

        let today = local_today_days();
        let last_30_cutoff = today.saturating_sub(29);
        self.today_tokens = tokens_for_day(&self.billing_daily, today);
        self.today_cost_usd = cost_for_day(&self.billing_daily, today);
        self.last_30_days_tokens = tokens_since(&self.billing_daily, last_30_cutoff);
        self.last_30_days_cost_usd = cost_since(&self.billing_daily, last_30_cutoff);
        if top_model.is_some() {
            self.top_model = top_model;
        }
        self.unpriced_tokens = unpriced_tokens;
        self.cost_source = Some(source);

        if !self.daily.is_empty() {
            apply_billing_activity_volume(&mut self.daily, &self.billing_daily);
        }
        self.recompute_activity_metrics();
    }

    fn recompute_activity_metrics(&mut self) {
        let metrics = activity_metrics(&self.daily, local_today_days());
        self.active_days = metrics.active_days;
        self.current_streak_days = metrics.current_streak_days;
        self.longest_streak_days = metrics.longest_streak_days;
        self.last_active_date = metrics.last_active_date;
        self.activity_total_tokens = metrics.total_tokens;
        self.activity_last_30_days_tokens = metrics.last_30_days_tokens;
    }

    fn apply_activity_tokens_to_usage_summary(&mut self) {
        let today = local_today_days();
        self.today_tokens = tokens_for_day(&self.daily, today);
        self.last_30_days_tokens = tokens_since(&self.daily, today.saturating_sub(29));
        self.today_cost_usd = None;
        self.last_30_days_cost_usd = None;
        self.cost_source = None;
    }
}

#[derive(Default)]
struct DailyAccumulator {
    tokens: u64,
    cost_usd: Option<f64>,
    sessions: u64,
}

fn record_token_day(
    by_day: &mut BTreeMap<String, DailyAccumulator>,
    model_tokens: &mut HashMap<String, u64>,
    model: &Option<String>,
    date: &str,
    usage: TokenUsage,
) {
    if !usage.has_tokens() {
        return;
    }

    let day = by_day.entry(date.to_string()).or_default();
    day.tokens = day.tokens.saturating_add(usage.total_tokens);

    if let Some(model) = model {
        *model_tokens.entry(model.clone()).or_default() += usage.total_tokens;
    }
}

fn record_session_dates(
    by_day: &mut BTreeMap<String, DailyAccumulator>,
    session_dates: BTreeSet<String>,
) {
    for date in session_dates {
        by_day.entry(date).or_default().sessions += 1;
    }
}

fn add_optional(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn apply_billing_activity_volume(activity: &mut [DailyUsage], billing: &[DailyUsage]) {
    let billing_by_day = billing
        .iter()
        .map(|day| (day.date.as_str(), day))
        .collect::<HashMap<_, _>>();
    for day in activity {
        if let Some(billing_day) = billing_by_day.get(day.date.as_str()) {
            day.tokens = billing_day.tokens;
            day.cost_usd = billing_day.cost_usd;
        }
    }
}

fn is_activity_day(day: &DailyUsage) -> bool {
    day.sessions > 0
}

fn tokens_for_day(daily: &[DailyUsage], target_day: i64) -> u64 {
    daily
        .iter()
        .find(|day| date_days(&day.date) == Some(target_day))
        .map(|day| day.tokens)
        .unwrap_or(0)
}

fn cost_for_day(daily: &[DailyUsage], target_day: i64) -> Option<f64> {
    daily
        .iter()
        .find(|day| date_days(&day.date) == Some(target_day))
        .and_then(|day| day.cost_usd)
}

fn tokens_since(daily: &[DailyUsage], cutoff_day: i64) -> u64 {
    daily
        .iter()
        .filter(|day| date_days(&day.date).is_some_and(|days| days >= cutoff_day))
        .map(|day| day.tokens)
        .fold(0_u64, u64::saturating_add)
}

fn cost_since(daily: &[DailyUsage], cutoff_day: i64) -> Option<f64> {
    daily
        .iter()
        .filter(|day| date_days(&day.date).is_some_and(|days| days >= cutoff_day))
        .fold(None, |total, day| add_optional(total, day.cost_usd))
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ActivityMetrics {
    active_days: u64,
    current_streak_days: u64,
    longest_streak_days: u64,
    last_active_date: Option<String>,
    total_tokens: u64,
    last_30_days_tokens: u64,
}

fn activity_metrics(daily: &[DailyUsage], today: i64) -> ActivityMetrics {
    let mut active_days = daily
        .iter()
        .filter(|day| is_activity_day(day))
        .filter_map(|day| date_days(&day.date).map(|days| (days, day)))
        .collect::<Vec<_>>();
    active_days.sort_by_key(|(days, _)| *days);
    active_days.dedup_by_key(|(days, _)| *days);

    if active_days.is_empty() {
        return ActivityMetrics::default();
    }

    let mut longest_streak = 0_u64;
    let mut running_streak = 0_u64;
    let mut previous_day = None;
    for (day, _) in &active_days {
        if previous_day.is_some_and(|previous| previous + 1 == *day) {
            running_streak += 1;
        } else {
            running_streak = 1;
        }
        longest_streak = longest_streak.max(running_streak);
        previous_day = Some(*day);
    }

    let last_active_day = active_days.last().map(|(day, _)| *day).unwrap_or(today);
    let current_streak_days = if last_active_day >= today.saturating_sub(1) {
        active_days
            .iter()
            .rev()
            .scan(None, |previous: &mut Option<i64>, (day, _)| {
                let keep_counting = previous.is_none_or(|previous_day| *day + 1 == previous_day);
                if keep_counting {
                    *previous = Some(*day);
                    Some(())
                } else {
                    None
                }
            })
            .count() as u64
    } else {
        0
    };

    ActivityMetrics {
        active_days: active_days.len() as u64,
        current_streak_days,
        longest_streak_days: longest_streak,
        last_active_date: Some(date_string_from_days(last_active_day)),
        total_tokens: active_days
            .iter()
            .map(|(_, day)| day.tokens)
            .fold(0_u64, u64::saturating_add),
        last_30_days_tokens: active_days
            .iter()
            .filter(|(day, _)| *day >= today.saturating_sub(29))
            .map(|(_, day)| day.tokens)
            .fold(0_u64, u64::saturating_add),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::codex::{SessionDayUsage, SessionSummary, TokenUsage};

    use super::*;

    #[test]
    fn builds_daily_profile_and_streaks() {
        let summaries = vec![
            summary(&date_offset(-2), 10),
            summary(&date_offset(-1), 20),
            summary(&date_offset(0), 30),
        ];

        let profile = build_usage_profile(&summaries, None, None);

        assert_eq!(profile.active_days, 3);
        assert_eq!(profile.activity_total_tokens, 60);
        assert_eq!(profile.activity_last_30_days_tokens, 60);
        assert_eq!(profile.today_tokens, 30);
        assert_eq!(profile.last_30_days_tokens, 60);
    }

    #[test]
    fn cost_report_keeps_session_activity_days() {
        let summaries = vec![summary(&date_offset(0), 10)];
        let first_day = local_today_days().saturating_sub(69);
        let cost_report = CostReport {
            source: "local estimate".to_string(),
            top_model: Some("gpt-5.5".to_string()),
            unpriced_tokens: 0,
            daily: (0..70)
                .map(|offset| {
                    let day = first_day + offset;
                    CostDay {
                        date: crate::calendar::date_string_from_days(day),
                        tokens: 100,
                        cost_usd: Some(1.0),
                    }
                })
                .collect(),
        };

        let profile = build_usage_profile(&summaries, Some(cost_report), None);

        assert_eq!(profile.daily.len(), 1);
        assert_eq!(profile.activity_total_tokens, 100);
        assert_eq!(profile.active_days, 1);
        assert_eq!(profile.activity_last_30_days_tokens, 100);
        assert_eq!(profile.today_tokens, 100);
        assert_eq!(profile.last_30_days_tokens, 3_000);
    }

    #[test]
    fn cost_report_updates_activity_volume_for_session_days() {
        let today = date_offset(0);
        let summaries = vec![summary(&today, 10)];
        let cost_report = CostReport {
            source: "local estimate".to_string(),
            top_model: Some("gpt-5.5".to_string()),
            unpriced_tokens: 0,
            daily: vec![CostDay {
                date: today,
                tokens: 100,
                cost_usd: Some(1.0),
            }],
        };

        let profile = build_usage_profile(&summaries, Some(cost_report), None);

        assert_eq!(profile.daily.len(), 1);
        assert_eq!(profile.activity_total_tokens, 100);
        assert_eq!(profile.activity_last_30_days_tokens, 100);
        assert_eq!(profile.active_days, 1);
    }

    #[test]
    fn cost_report_does_not_create_activity_days() {
        let today = date_offset(0);
        let cost_report = CostReport {
            source: "local estimate".to_string(),
            top_model: Some("gpt-5.5".to_string()),
            unpriced_tokens: 0,
            daily: vec![CostDay {
                date: today,
                tokens: 100,
                cost_usd: Some(1.0),
            }],
        };

        let profile = build_usage_profile(&[], Some(cost_report), None);

        assert_eq!(profile.daily.len(), 0);
        assert_eq!(profile.activity_total_tokens, 0);
        assert_eq!(profile.active_days, 0);
        assert_eq!(profile.current_streak_days, 0);
        assert_eq!(profile.longest_streak_days, 0);
        assert_eq!(profile.activity_last_30_days_tokens, 0);
        assert_eq!(profile.today_tokens, 100);
    }

    #[test]
    fn calculates_streaks_from_local_activity_days() {
        let today = crate::calendar::date_days("2026-06-10").expect("date");
        let daily = vec![
            day("2026-06-01", 10, 1),
            day("2026-06-02", 10, 1),
            day("2026-06-04", 10, 1),
            day("2026-06-08", 10, 1),
            day("2026-06-09", 10, 1),
            day("2026-06-10", 10, 1),
        ];

        let metrics = activity_metrics(&daily, today);

        assert_eq!(metrics.active_days, 6);
        assert_eq!(metrics.current_streak_days, 3);
        assert_eq!(metrics.longest_streak_days, 3);
        assert_eq!(metrics.last_30_days_tokens, 60);
        assert_eq!(metrics.last_active_date.as_deref(), Some("2026-06-10"));
    }

    #[test]
    fn profile_uses_per_day_session_usage_for_streaks() {
        let summaries = vec![SessionSummary {
            model: Some("gpt-5.5".to_string()),
            session_date: Some("2026-06-01".to_string()),
            daily_usage: vec![
                session_day("2026-06-01", 10),
                session_day("2026-06-02", 20),
                session_day("2026-06-03", 30),
            ],
            ..SessionSummary::default()
        }];

        let profile = build_usage_profile(&summaries, None, None);

        assert_eq!(profile.active_days, 3);
        assert_eq!(profile.activity_total_tokens, 60);
        assert_eq!(profile.activity_last_30_days_tokens, 60);
        assert_eq!(profile.daily.len(), 3);
    }

    #[test]
    fn indexed_activity_replaces_jsonl_activity_without_replacing_billing() {
        let today = date_offset(0);
        let yesterday = date_offset(-1);
        let two_days_ago = date_offset(-2);
        let summaries = vec![summary(&today, 10)];
        let indexed_activity = vec![day(&two_days_ago, 100, 1), day(&yesterday, 200, 1)];

        let profile = build_usage_profile(&summaries, None, Some(indexed_activity));

        assert_eq!(profile.today_tokens, 10);
        assert_eq!(profile.last_30_days_tokens, 10);
        assert_eq!(profile.active_days, 2);
        assert_eq!(profile.activity_total_tokens, 300);
        assert_eq!(profile.activity_last_30_days_tokens, 300);
        assert_eq!(
            profile
                .daily
                .iter()
                .map(|day| (day.date.as_str(), day.tokens, day.sessions))
                .collect::<Vec<_>>(),
            vec![
                (two_days_ago.as_str(), 100, 1),
                (yesterday.as_str(), 200, 1)
            ]
        );
    }

    #[test]
    fn remote_profile_activity_overrides_local_activity_metrics() {
        let today = local_today_days();
        let yesterday = today.saturating_sub(1);
        let mut profile = build_usage_profile(&[summary(&date_offset(-2), 10)], None, None);

        profile.apply_remote_activity(ProfileActivity {
            daily: vec![
                day(&date_string_from_days(yesterday), 100, 0),
                day(&date_string_from_days(today), 200, 0),
            ],
            lifetime_tokens: Some(999),
            peak_day_tokens: Some(300),
            current_streak_days: Some(48),
            longest_streak_days: Some(50),
            longest_task_seconds: Some(3_600),
        });

        assert_eq!(profile.activity_source, ActivitySource::OpenAIProfile);
        assert_eq!(profile.activity_total_tokens, 999);
        assert_eq!(profile.activity_last_30_days_tokens, 300);
        assert_eq!(profile.active_days, 2);
        assert_eq!(profile.current_streak_days, 48);
        assert_eq!(profile.longest_streak_days, 50);
        assert_eq!(profile.peak_day_tokens, Some(300));
        assert_eq!(profile.longest_task_seconds, Some(3_600));
        assert_eq!(profile.today_tokens, 200);
        assert_eq!(profile.last_30_days_tokens, 300);
        assert_eq!(profile.today_cost_usd, None);
        assert_eq!(profile.last_30_days_cost_usd, None);
        assert_eq!(profile.cost_source, None);
    }

    #[test]
    fn remote_profile_activity_keeps_explicit_cost_report_totals() {
        let today = date_string_from_days(local_today_days());
        let cost_report = CostReport {
            source: "local estimate".to_string(),
            top_model: Some("gpt-5.5".to_string()),
            unpriced_tokens: 0,
            daily: vec![CostDay {
                date: today.clone(),
                tokens: 500,
                cost_usd: Some(4.25),
            }],
        };
        let mut profile = build_usage_profile(&[], Some(cost_report), None);

        profile.apply_remote_activity(ProfileActivity {
            daily: vec![day(&today, 200, 0)],
            lifetime_tokens: Some(999),
            current_streak_days: Some(1),
            longest_streak_days: Some(1),
            ..ProfileActivity::default()
        });

        assert_eq!(profile.activity_total_tokens, 999);
        assert_eq!(profile.today_tokens, 500);
        assert_eq!(profile.today_cost_usd, Some(4.25));
        assert_eq!(profile.cost_source.as_deref(), Some("local estimate"));
    }

    #[test]
    fn session_dates_count_for_activity_even_without_tokens() {
        let summaries = vec![
            session_only("2026-06-01"),
            session_only("2026-06-02"),
            session_only("2026-06-03"),
        ];

        let profile = build_usage_profile(&summaries, None, None);

        assert_eq!(profile.active_days, 3);
        assert_eq!(profile.activity_total_tokens, 0);
        assert_eq!(
            profile
                .daily
                .iter()
                .map(|day| (day.date.as_str(), day.sessions))
                .collect::<Vec<_>>(),
            vec![("2026-06-01", 1), ("2026-06-02", 1), ("2026-06-03", 1)]
        );
    }

    #[test]
    fn current_streak_expires_after_missing_yesterday() {
        let today = crate::calendar::date_days("2026-06-10").expect("date");
        let daily = vec![day("2026-06-07", 10, 1), day("2026-06-08", 10, 1)];

        let metrics = activity_metrics(&daily, today);

        assert_eq!(metrics.current_streak_days, 0);
        assert_eq!(metrics.longest_streak_days, 2);
    }

    #[test]
    fn streaks_ignore_billing_only_days() {
        let today = crate::calendar::date_days("2026-06-10").expect("date");
        let daily = vec![
            day("2026-06-08", 10, 1),
            day("2026-06-09", 100, 0),
            day("2026-06-10", 10, 1),
        ];

        let metrics = activity_metrics(&daily, today);

        assert_eq!(metrics.active_days, 2);
        assert_eq!(metrics.current_streak_days, 1);
        assert_eq!(metrics.longest_streak_days, 1);
    }

    fn summary(date: &str, tokens: u64) -> SessionSummary {
        let day = crate::calendar::date_days(date).expect("date");
        SessionSummary {
            modified_at: Some(UNIX_EPOCH + Duration::from_secs(day as u64 * 86_400)),
            activity_date: Some(date.to_string()),
            model: Some("gpt-5.5".to_string()),
            total_usage: TokenUsage {
                total_tokens: tokens,
                input_tokens: tokens,
                ..TokenUsage::default()
            },
            ..SessionSummary::default()
        }
    }

    fn day(date: &str, tokens: u64, sessions: u64) -> DailyUsage {
        DailyUsage {
            date: date.to_string(),
            tokens,
            cost_usd: None,
            sessions,
        }
    }

    fn session_day(date: &str, tokens: u64) -> SessionDayUsage {
        SessionDayUsage {
            date: date.to_string(),
            usage: TokenUsage {
                total_tokens: tokens,
                input_tokens: tokens,
                ..TokenUsage::default()
            },
        }
    }

    fn session_only(date: &str) -> SessionSummary {
        SessionSummary {
            session_date: Some(date.to_string()),
            ..SessionSummary::default()
        }
    }

    fn date_offset(offset: i64) -> String {
        date_string_from_days(local_today_days().saturating_add(offset))
    }
}
