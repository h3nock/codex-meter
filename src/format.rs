use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codex::{MeterSnapshot, RateWindow};

pub fn plain_summary(snapshot: &MeterSnapshot) -> String {
    let latest = snapshot.latest_session.as_ref();
    let model = latest
        .and_then(|session| session.model.as_deref())
        .unwrap_or("unknown");
    let provider = latest
        .and_then(|session| session.provider.as_deref())
        .unwrap_or("unknown");
    let plan = snapshot
        .current_rate_limits
        .as_ref()
        .and_then(|limits| limits.plan_type.as_deref())
        .and_then(plan_display_name)
        .unwrap_or_else(|| "unknown".to_string());

    let mut lines = Vec::new();
    lines.push("codex-meter".to_string());
    lines.push(format!("plan: {plan}"));
    lines.push(format!("latest model: {model} ({provider})"));
    if let Some(limits) = &snapshot.current_rate_limits {
        if let Some(primary) = limits.primary.as_ref() {
            lines.push(format!("session: {}", window_line("left", Some(primary))));
        }
        if let Some(secondary) = limits.secondary.as_ref() {
            lines.push(format!("weekly: {}", window_line("left", Some(secondary))));
        }
    }
    lines.push(format!(
        "usage: today {} / {}, 30d {} / {}",
        money(snapshot.profile.today_cost_usd),
        tokens(snapshot.profile.today_tokens),
        money(snapshot.profile.last_30_days_cost_usd),
        tokens(snapshot.profile.last_30_days_tokens)
    ));
    lines.push(format!(
        "activity: {} {} tokens, {} 30d tokens, {} used days, {}d current streak, {}d longest streak",
        tokens(snapshot.profile.activity_total_tokens),
        snapshot.profile.activity_source.total_label(),
        tokens(snapshot.profile.activity_last_30_days_tokens),
        snapshot.profile.active_days,
        snapshot.profile.current_streak_days,
        snapshot.profile.longest_streak_days
    ));
    if let Some(peak_day_tokens) = snapshot.profile.peak_day_tokens {
        lines.push(format!("peak day: {}", tokens(peak_day_tokens)));
    }
    if let Some(longest_task_seconds) = snapshot.profile.longest_task_seconds {
        lines.push(format!(
            "longest task: {}",
            duration(Some(longest_task_seconds))
        ));
    }
    lines.push(format!(
        "activity source: {}",
        snapshot.profile.activity_source.label()
    ));
    lines.push(format!(
        "cost source: {}",
        snapshot
            .profile
            .cost_source
            .as_deref()
            .unwrap_or("unavailable")
    ));
    if snapshot.profile.unpriced_tokens > 0 {
        lines.push(format!(
            "unpriced billing tokens: {}",
            tokens(snapshot.profile.unpriced_tokens)
        ));
    }
    if snapshot.malformed_lines > 0 {
        lines.push(format!("malformed lines: {}", snapshot.malformed_lines));
    }
    if !snapshot.data_warnings.is_empty() {
        lines.push(format!(
            "data warnings: {}",
            snapshot.data_warnings.join("; ")
        ));
    }

    lines.join("\n")
}

pub fn plan_display_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    match raw.to_ascii_lowercase().as_str() {
        "pro" => Some("Pro 20x".to_string()),
        "prolite" | "pro_lite" | "pro-lite" | "pro lite" => Some("Pro 5x".to_string()),
        _ => Some(
            raw.split(['_', '-', ' '])
                .filter(|part| !part.is_empty())
                .map(title_word)
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

fn title_word(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if matches!(lower.as_str(), "cbp" | "k12") {
        return lower.to_ascii_uppercase();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}",
            first.to_uppercase(),
            chars.as_str().to_ascii_lowercase()
        ),
        None => String::new(),
    }
}

pub fn tokens(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

pub fn money(value: Option<f64>) -> String {
    match value {
        Some(value) if value >= 1_000.0 => {
            let cents = (value * 100.0).round() as u64;
            format!("${}.{:02}", grouped(cents / 100), cents % 100)
        }
        Some(value) if value >= 100.0 => format!("${value:.2}"),
        Some(value) if value >= 1.0 => format!("${value:.2}"),
        Some(value) if value > 0.0 => format!("${value:.4}"),
        Some(_) => "$0".to_string(),
        None => "$--".to_string(),
    }
}

fn grouped(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub fn duration(value: Option<u64>) -> String {
    let Some(seconds) = value else {
        return "--".to_string();
    };
    let minutes = seconds / 60;
    if minutes >= 24 * 60 {
        format!("{}d {}h", minutes / (24 * 60), (minutes / 60) % 24)
    } else if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".to_string()
    }
}

pub fn ratio(value: Option<f64>) -> f64 {
    value.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0
}

pub fn remaining_percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.0}%", (100.0 - value.clamp(0.0, 100.0)).max(0.0)))
        .unwrap_or_else(|| "--".to_string())
}

pub fn window_line(label: &str, window: Option<&RateWindow>) -> String {
    match window {
        Some(window) => format!(
            "{} {label}, resets {}",
            remaining_percent(window.used_percent),
            reset_in(window.resets_at)
        ),
        None => format!("{label} --"),
    }
}

pub fn reset_in(epoch_seconds: Option<u64>) -> String {
    let Some(epoch_seconds) = epoch_seconds else {
        return "--".to_string();
    };

    let reset = UNIX_EPOCH + Duration::from_secs(epoch_seconds);
    let now = SystemTime::now();
    let duration = match reset.duration_since(now) {
        Ok(duration) => duration,
        Err(_) => return "now".to_string(),
    };

    let total_minutes = duration.as_secs() / 60;
    if total_minutes >= 24 * 60 {
        format!(
            "{}d {}h",
            total_minutes / (24 * 60),
            (total_minutes / 60) % 24
        )
    } else if total_minutes >= 60 {
        format!("{}h {}m", total_minutes / 60, total_minutes % 60)
    } else if total_minutes == 0 {
        "<1m".to_string()
    } else {
        format!("{total_minutes}m")
    }
}

pub fn age(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "--".to_string();
    };

    match SystemTime::now().duration_since(time) {
        Ok(duration) if duration.as_secs() >= 86_400 => {
            format!("{}d ago", duration.as_secs() / 86_400)
        }
        Ok(duration) if duration.as_secs() >= 3_600 => {
            format!("{}h ago", duration.as_secs() / 3_600)
        }
        Ok(duration) if duration.as_secs() >= 60 => format!("{}m ago", duration.as_secs() / 60),
        Ok(duration) => format!("{}s ago", duration.as_secs()),
        Err(_) => "now".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_token_units() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_200), "1.2K");
        assert_eq!(tokens(2_500_000), "2.5M");
    }

    #[test]
    fn maps_codex_plan_names() {
        assert_eq!(plan_display_name("pro").as_deref(), Some("Pro 20x"));
        assert_eq!(plan_display_name("prolite").as_deref(), Some("Pro 5x"));
        assert_eq!(
            plan_display_name("enterprise_cbp_usage_based").as_deref(),
            Some("Enterprise CBP Usage Based")
        );
    }

    #[test]
    fn reset_in_shows_sub_minute_window() {
        let soon = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs()
            + 30;

        assert_eq!(reset_in(Some(soon)), "<1m");
    }

    #[test]
    fn formats_large_money_with_grouping() {
        assert_eq!(money(Some(405.386)), "$405.39");
        assert_eq!(money(Some(25_713.039)), "$25,713.04");
    }
}
