use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::codex::{MeterSnapshot, RateLimits, RateWindow};

pub fn plain_summary(snapshot: &MeterSnapshot) -> String {
    let latest = snapshot.latest_session.as_ref();
    let model = latest
        .and_then(|session| session.model.as_deref())
        .unwrap_or("unknown");
    let provider = latest
        .and_then(|session| session.provider.as_deref())
        .unwrap_or("unknown");
    let last = latest.map(|session| session.last_usage).unwrap_or_default();

    let mut lines = Vec::new();
    lines.push("codex-meter".to_string());
    lines.push(format!("home: {}", snapshot.codex_home.display()));
    lines.push(format!(
        "sessions: {} scanned / {} available ({} archived)",
        snapshot.scanned_files, snapshot.available_session_files, snapshot.archived_session_files
    ));
    if snapshot.tail_scanned_files > 0 {
        lines.push(format!(
            "bounded scan: {} large logs read from tail",
            snapshot.tail_scanned_files
        ));
    }
    lines.push(format!("latest model: {model} ({provider})"));
    lines.push(format!(
        "last turn: {} total, {} input, {} output, {} cached",
        tokens(last.total_tokens),
        tokens(last.input_tokens),
        tokens(last.output_tokens),
        tokens(last.cached_input_tokens)
    ));
    lines.push(format!(
        "scanned total: {} total tokens",
        tokens(snapshot.scanned_total_usage.total_tokens)
    ));

    if let Some(limits) = &snapshot.current_rate_limits {
        lines.push(format!("remaining windows: {}", rate_limit_line(limits)));
    }

    if snapshot.malformed_lines > 0 {
        lines.push(format!("malformed lines: {}", snapshot.malformed_lines));
    }

    lines.join("\n")
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

pub fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.0}%", value.clamp(0.0, 999.0)))
        .unwrap_or_else(|| "--".to_string())
}

pub fn ratio(value: Option<f64>) -> f64 {
    value.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0
}

pub fn remaining_percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.0}%", (100.0 - value.clamp(0.0, 100.0)).max(0.0)))
        .unwrap_or_else(|| "--".to_string())
}

pub fn rate_limit_line(limits: &RateLimits) -> String {
    let plan = limits.plan_type.as_deref().unwrap_or("unknown plan");
    let weekly = window_line("weekly left", limits.secondary.as_ref());
    let short = window_line("5h left", limits.primary.as_ref());
    format!("{plan}; {weekly}; {short}")
}

pub fn window_line(label: &str, window: Option<&RateWindow>) -> String {
    match window {
        Some(window) => format!(
            "{label} {} reset {} ({} used)",
            remaining_percent(window.used_percent),
            reset_in(window.resets_at),
            percent(window.used_percent)
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
    fn labels_weekly_remaining_before_short_window() {
        let limits = RateLimits {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some("pro".to_string()),
            primary: Some(RateWindow {
                used_percent: Some(42.0),
                window_minutes: Some(300),
                resets_at: None,
            }),
            secondary: Some(RateWindow {
                used_percent: Some(71.0),
                window_minutes: Some(10080),
                resets_at: None,
            }),
            credits: None,
            rate_limit_reached_type: None,
        };

        let line = rate_limit_line(&limits);

        assert!(line.contains("weekly left 29%"));
        assert!(line.contains("5h left 58%"));
        assert!(line.contains("(71% used)"));
        assert!(line.find("weekly left").expect("weekly") < line.find("5h left").expect("5h"));
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
}
