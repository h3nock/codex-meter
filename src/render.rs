use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

use crate::{
    calendar::{date_days, date_string_from_days},
    cli::DashboardOptions,
    codex::{CostStatus, MeterSnapshot, RateWindow},
    format,
    profile::DailyUsage,
};

const BG: Color = Color::Rgb(6, 8, 13);
const PANEL: Color = Color::Rgb(10, 14, 22);
const PANEL_ALT: Color = Color::Rgb(12, 17, 27);
const BORDER: Color = Color::Rgb(48, 58, 82);
const BORDER_DIM: Color = Color::Rgb(29, 36, 53);
const TEXT: Color = Color::Rgb(226, 232, 246);
const MUTED: Color = Color::Rgb(130, 143, 172);
const TRACK: Color = Color::Rgb(28, 35, 51);
const TRACK_BOX: Color = Color::Rgb(45, 51, 65);
const ACTIVITY_EMPTY: Color = Color::Rgb(22, 27, 34);
const ACTIVITY_LOW: Color = Color::Rgb(14, 68, 41);
const ACTIVITY_MEDIUM_LOW: Color = Color::Rgb(0, 109, 50);
const ACTIVITY_MEDIUM_HIGH: Color = Color::Rgb(38, 166, 65);
const ACTIVITY_HIGH: Color = Color::Rgb(57, 211, 83);
const MINT: Color = Color::Rgb(118, 232, 164);
const GOLD: Color = Color::Rgb(248, 197, 99);
const ROSE: Color = Color::Rgb(255, 104, 134);
const TEAL: Color = Color::Rgb(92, 205, 196);
const ACTIVITY_PANEL_MAX_HEIGHT: u16 = 12;
const ACTIVITY_LOW_MAX: f64 = 0.05;
const ACTIVITY_MEDIUM_LOW_MAX: f64 = 0.20;
const ACTIVITY_MEDIUM_HIGH_MAX: f64 = 0.75;

pub fn draw(
    frame: &mut Frame<'_>,
    snapshot: Option<&MeterSnapshot>,
    error: Option<&str>,
    _options: &DashboardOptions,
) {
    let area = frame.area();
    if area.width < 90 || area.height < 32 {
        draw_too_small(frame, area);
        return;
    }

    let shell = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(BG))
        .title(Span::styled(
            " codex-meter ",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Left);
    frame.render_widget(shell, area);

    let inner = inset(area, 1);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(12),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .spacing(1)
        .split(inner);

    draw_header(frame, rows[0], snapshot, error);
    draw_quota_cards(frame, rows[1], snapshot);
    draw_middle(frame, rows[2], snapshot);
    draw_activity_calendar(frame, rows[3], snapshot);
    draw_footer(frame, rows[4], snapshot);
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new("codex-meter needs at least 90x32")
        .alignment(Alignment::Center)
        .style(Style::default().fg(GOLD).bg(BG))
        .block(panel("codex-meter", GOLD));
    frame.render_widget(paragraph, area);
}

fn draw_header(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&MeterSnapshot>,
    error: Option<&str>,
) {
    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let model = latest
        .and_then(|session| session.model.as_deref())
        .unwrap_or("--");
    let plan = snapshot
        .and_then(|snapshot| snapshot.current_rate_limits.as_ref())
        .and_then(|limits| limits.plan_type.as_deref())
        .and_then(format::plan_display_name)
        .unwrap_or_else(|| "--".to_string());
    let left = vec![
        muted("plan "),
        value(&plan, MINT),
        muted("   latest model "),
        value(model, TEAL),
    ];
    let right = match (error, snapshot) {
        (Some(message), _) => vec![Span::styled(
            format!("error {message}"),
            Style::default().fg(ROSE),
        )],
        (None, Some(snapshot)) => header_status_spans(snapshot),
        (None, None) => vec![muted("scanning local Codex usage")],
    };

    frame.render_widget(
        Paragraph::new(header_line(area.width, left, right)).style(Style::default().bg(BG)),
        area,
    );
}

fn header_status_spans(snapshot: &MeterSnapshot) -> Vec<Span<'static>> {
    let quota_fetched_at = snapshot
        .current_rate_limits
        .as_ref()
        .and_then(|limits| limits.fetched_at);
    let updated_at = quota_fetched_at.or(snapshot.scanned_at);
    vec![muted("updated "), value(&format::age(updated_at), TEXT)]
}

fn draw_quota_cards(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(1)
        .split(area);

    let limits = snapshot.and_then(|snapshot| snapshot.current_rate_limits.as_ref());
    let primary = limits.and_then(|limits| limits.primary.as_ref());
    let secondary = limits.and_then(|limits| limits.secondary.as_ref());
    draw_quota_card(frame, chunks[0], "Session", primary, MINT);
    draw_quota_card(
        frame,
        chunks[1],
        "Weekly",
        secondary,
        quota_accent(secondary),
    );
}

fn draw_quota_card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    window: Option<&RateWindow>,
    accent: Color,
) {
    let used = window.and_then(|window| window.used_percent);
    let used_ratio = format::ratio(used);
    let remaining_ratio = 1.0 - used_ratio;
    let remaining = format::remaining_percent(used);
    let reset = format::reset_in(window.and_then(|window| window.resets_at));
    let (pace, pace_color) = quota_projection(window);
    let inner_width = area.width.saturating_sub(2) as usize;

    let lines = vec![
        Line::from(vec![
            value_bold(&remaining, remaining_color(remaining_ratio)),
            muted(" remaining"),
        ]),
        quota_remaining_line(inner_width, remaining_ratio),
        quota_status_line(inner_width, &reset, &pace, pace_color),
    ];

    frame.render_widget(Paragraph::new(lines).block(panel(title, accent)), area);
}

fn draw_middle(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    draw_usage_breakdown(frame, area, snapshot);
}

fn draw_usage_breakdown(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    frame.render_widget(panel("cost and tokens", TEXT), area);
    let inner = panel_inner(area);

    let Some(snapshot) = snapshot else {
        frame.render_widget(
            Paragraph::new("scanning usage").style(Style::default().bg(PANEL)),
            inner,
        );
        return;
    };

    let profile = &snapshot.profile;
    let daily = billing_daily(profile);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(47), Constraint::Min(24)])
        .spacing(2)
        .split(inner);

    let table_lines = vec![
        Line::from(vec![muted("period        cost          tokens")]),
        Line::from(""),
        usage_table_line(
            "Latest day",
            &cost_label(profile.today_cost_usd, snapshot.cost_status),
            &format::tokens(profile.today_tokens),
        ),
        usage_table_line(
            "Last 30 days",
            &cost_label(profile.last_30_days_cost_usd, snapshot.cost_status),
            &format::tokens(profile.last_30_days_tokens),
        ),
        Line::from(""),
        Line::from(vec![
            muted("top model    "),
            value(profile.top_model.as_deref().unwrap_or("--"), TEAL),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(table_lines).style(Style::default().bg(PANEL)),
        columns[0],
    );

    let chart_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(columns[1]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            muted("30-day token volume"),
            muted("   "),
            value(&date_range_label(daily, 30), TEXT),
        ]))
        .style(Style::default().bg(PANEL)),
        chart_rows[0],
    );
    frame.render_widget(
        Paragraph::new(token_volume_lines(
            chart_rows[1].width,
            chart_rows[1].height,
            daily,
            30,
        ))
        .style(Style::default().bg(PANEL)),
        chart_rows[1],
    );
}

fn usage_table_line(period: &str, cost: &str, tokens: &str) -> Line<'static> {
    Line::from(vec![
        value_bold(&format!("{period:<14}"), TEXT),
        value(&format!("{cost:<14}"), GOLD),
        value(tokens, TEAL),
    ])
}

fn cost_label(cost: Option<f64>, status: CostStatus) -> String {
    match (cost, status) {
        (Some(cost), _) => format::money(Some(cost)),
        (None, CostStatus::Indexing) => "indexing...".to_string(),
        (None, CostStatus::Ready) => format::money(None),
    }
}

fn draw_activity_calendar(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let area = activity_panel_area(area);
    frame.render_widget(panel("activity", TEXT), area);
    let inner = panel_inner(area);

    let Some(snapshot) = snapshot else {
        frame.render_widget(
            Paragraph::new("scanning activity").style(Style::default().bg(PANEL_ALT)),
            inner,
        );
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Length(1),
        ])
        .split(inner);

    let profile = &snapshot.profile;
    let mut header_spans = vec![
        value_bold(&format::tokens(profile.activity_total_tokens), TEAL),
        muted(&format!(" {}", profile.activity_source.total_label())),
        separator(),
    ];
    if let Some(peak_day_tokens) = profile.peak_day_tokens {
        header_spans.extend([
            value_bold(&format::tokens(peak_day_tokens), TEAL),
            muted(" peak"),
            separator(),
        ]);
    } else {
        header_spans.extend([
            value_bold(&format::tokens(profile.activity_last_30_days_tokens), TEAL),
            muted(" 30d"),
            separator(),
        ]);
    }
    header_spans.extend([
        metric("used days", &profile.active_days.to_string(), MINT),
        separator(),
        metric("streak", &format!("{}d", profile.current_streak_days), MINT),
        separator(),
        metric(
            "longest",
            &format!("{}d", profile.longest_streak_days),
            GOLD,
        ),
    ]);
    let header = vec![Line::from(header_spans)];
    frame.render_widget(
        Paragraph::new(header).style(Style::default().bg(PANEL_ALT)),
        rows[0],
    );

    frame.render_widget(
        Paragraph::new(activity_calendar_lines(
            rows[1].width,
            rows[1].height,
            &profile.daily,
        ))
        .style(Style::default().bg(PANEL_ALT)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(activity_legend_line())
            .alignment(Alignment::Right)
            .style(Style::default().bg(PANEL_ALT)),
        rows[2],
    );
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let mut spans = vec![
        key("q"),
        muted(" quit   "),
        key("esc"),
        muted(" quit   "),
        key("r"),
        muted(" refresh"),
    ];

    if let Some(snapshot) = snapshot
        && snapshot.profile.unpriced_tokens > 0
    {
        spans.extend([
            muted("   unpriced "),
            value(&format::tokens(snapshot.profile.unpriced_tokens), GOLD),
        ]);
    }

    if let Some(snapshot) = snapshot
        && snapshot.malformed_lines > 0
    {
        spans.extend([
            muted("   parse warnings "),
            value(&snapshot.malformed_lines.to_string(), GOLD),
        ]);
    }

    if let Some(snapshot) = snapshot
        && !snapshot.data_warnings.is_empty()
    {
        spans.extend([
            muted("   data warnings "),
            value(&snapshot.data_warnings.len().to_string(), GOLD),
        ]);
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .alignment(Alignment::Center)
            .style(Style::default().bg(BG))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(BORDER_DIM),
            ),
        area,
    );
}

fn billing_daily(profile: &crate::profile::UsageProfile) -> &[DailyUsage] {
    if profile.billing_daily.is_empty() {
        &profile.daily
    } else {
        &profile.billing_daily
    }
}

fn daily_token_series(daily: &[DailyUsage], days: usize) -> Vec<u64> {
    if daily.is_empty() {
        return vec![0; days.max(1)];
    }

    let newest_day = daily
        .last()
        .and_then(|day| date_days(&day.date))
        .unwrap_or_else(today_days);
    let by_day = daily
        .iter()
        .filter_map(|day| date_days(&day.date).map(|date| (date, day.tokens)))
        .collect::<HashMap<_, _>>();

    let start = newest_day.saturating_sub(days.saturating_sub(1) as i64);
    (0..days)
        .map(|offset| by_day.get(&(start + offset as i64)).copied().unwrap_or(0))
        .collect()
}

fn date_range_label(daily: &[DailyUsage], days: usize) -> String {
    let newest_day = daily
        .last()
        .and_then(|day| date_days(&day.date))
        .unwrap_or_else(today_days);
    let start_day = newest_day.saturating_sub(days.saturating_sub(1) as i64);
    format!(
        "{} to {}",
        short_date(&date_string_from_days(start_day)),
        short_date(&date_string_from_days(newest_day))
    )
}

fn token_volume_lines(
    width: u16,
    height: u16,
    daily: &[DailyUsage],
    days: usize,
) -> Vec<Line<'static>> {
    let height = height.max(1) as usize;
    let series = daily_token_series(daily, days);
    let visible_days = series.len().min((width as usize / 2).max(1));
    let start = series.len().saturating_sub(visible_days);
    let series = &series[start..];
    let max_value = series.iter().copied().max().unwrap_or(1).max(1);

    (1..=height)
        .rev()
        .map(|row| {
            let spans = series
                .iter()
                .flat_map(|value| {
                    let level =
                        ((*value as f64 / max_value as f64) * height as f64).ceil() as usize;
                    let filled = level >= row && *value > 0;
                    [
                        Span::styled(
                            if filled { "█" } else { " " },
                            Style::default().fg(if filled { TEAL } else { TRACK }),
                        ),
                        Span::raw(" "),
                    ]
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

fn activity_calendar_lines(width: u16, _height: u16, daily: &[DailyUsage]) -> Vec<Line<'static>> {
    if daily.is_empty() {
        return vec![Line::from(muted("no Codex activity yet"))];
    }

    let max_tokens = daily.iter().map(|day| day.tokens).max().unwrap_or(1).max(1);
    let by_day = daily
        .iter()
        .filter_map(|day| date_days(&day.date).map(|date| (date, day.tokens)))
        .collect::<HashMap<_, _>>();

    let Some((oldest_day, newest_day)) = activity_date_range(daily) else {
        return vec![Line::from(muted("no Codex activity yet"))];
    };
    let geometry = activity_calendar_geometry(width, oldest_day, newest_day);
    let mut lines = Vec::with_capacity(geometry.bands.len() * 8);
    for band in geometry.bands {
        lines.extend(activity_calendar_band_lines(&band, &by_day, max_tokens));
    }

    lines
}

const CALENDAR_LABEL_WIDTH: usize = 4;
const CALENDAR_WEEKS: usize = 53;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CalendarBand {
    columns: usize,
    cell_width: usize,
    start_day: i64,
    left_padding: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CalendarGeometry {
    bands: Vec<CalendarBand>,
}

fn activity_calendar_geometry(width: u16, _oldest_day: i64, newest_day: i64) -> CalendarGeometry {
    let end_week_start = newest_day - weekday_index(newest_day);
    let spaced_columns = (width as usize)
        .saturating_sub(CALENDAR_LABEL_WIDTH)
        .saturating_div(2)
        .clamp(1, CALENDAR_WEEKS);
    let cell_width = if width as usize >= CALENDAR_LABEL_WIDTH + spaced_columns * 2 {
        2
    } else {
        1
    };
    let columns = if cell_width == 2 {
        spaced_columns
    } else {
        (width as usize)
            .saturating_sub(CALENDAR_LABEL_WIDTH)
            .clamp(1, CALENDAR_WEEKS)
    };
    let start_day = end_week_start.saturating_sub((columns.saturating_sub(1) * 7) as i64);

    CalendarGeometry {
        bands: vec![calendar_band(width, start_day, columns, cell_width)],
    }
}

fn activity_date_range(daily: &[DailyUsage]) -> Option<(i64, i64)> {
    daily
        .iter()
        .filter_map(|day| date_days(&day.date))
        .fold(None, |range, day| match range {
            Some((oldest, newest)) => Some((oldest.min(day), newest.max(day))),
            None => Some((day, day)),
        })
}

fn calendar_band(_width: u16, start_day: i64, columns: usize, cell_width: usize) -> CalendarBand {
    CalendarBand {
        columns,
        cell_width,
        start_day,
        left_padding: 0,
    }
}

fn activity_calendar_band_lines(
    band: &CalendarBand,
    by_day: &HashMap<i64, u64>,
    max_tokens: u64,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(8);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(band.left_padding)),
        Span::raw(" ".repeat(CALENDAR_LABEL_WIDTH)),
        muted(&calendar_month_labels(
            band.start_day,
            band.columns,
            band.cell_width,
        )),
    ]));

    for weekday in 0..7 {
        let mut spans = vec![
            Span::raw(" ".repeat(band.left_padding)),
            muted(weekday_label(weekday)),
        ];
        for column in 0..band.columns {
            let day = band.start_day + (column * 7) as i64 + weekday as i64;
            spans.extend(activity_cell(
                by_day.get(&day).copied().unwrap_or(0),
                max_tokens,
                band.cell_width,
            ));
        }
        lines.push(Line::from(spans));
    }

    lines
}

fn calendar_month_labels(start_day: i64, columns: usize, cell_width: usize) -> String {
    let mut chars = vec![' '; columns * cell_width];
    let mut next_free = 0;
    for column in 0..columns {
        let label = (0..7)
            .map(|weekday| date_string_from_days(start_day + (column * 7 + weekday) as i64))
            .find(|date| date.get(8..10) == Some("01"))
            .or_else(|| (column == 0).then(|| date_string_from_days(start_day)));
        let Some(label_date) = label else {
            continue;
        };
        let name = month_name(label_date.get(5..7).unwrap_or(""));
        let start = column * cell_width;
        if start < next_free || start + name.len() > chars.len() {
            continue;
        }
        for (offset, ch) in name.chars().enumerate() {
            chars[start + offset] = ch;
        }
        next_free = start + name.len() + 1;
    }
    chars.into_iter().collect()
}

fn weekday_index(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

fn weekday_label(weekday: usize) -> &'static str {
    match weekday {
        1 => "Mon ",
        3 => "Wed ",
        5 => "Fri ",
        _ => "    ",
    }
}

fn activity_color(tokens: u64, max_tokens: u64) -> Color {
    let Some(ratio) = activity_intensity_ratio(tokens, max_tokens) else {
        return ACTIVITY_EMPTY;
    };
    if ratio < ACTIVITY_LOW_MAX {
        ACTIVITY_LOW
    } else if ratio < ACTIVITY_MEDIUM_LOW_MAX {
        ACTIVITY_MEDIUM_LOW
    } else if ratio < ACTIVITY_MEDIUM_HIGH_MAX {
        ACTIVITY_MEDIUM_HIGH
    } else {
        ACTIVITY_HIGH
    }
}

fn activity_intensity_ratio(tokens: u64, max_tokens: u64) -> Option<f64> {
    if tokens == 0 {
        return None;
    }
    Some(tokens as f64 / max_tokens.max(1) as f64)
}

fn activity_cell(tokens: u64, max_tokens: u64, cell_width: usize) -> Vec<Span<'static>> {
    let glyph = "■";
    let cell = if tokens == 0 {
        Span::styled(glyph, Style::default().fg(ACTIVITY_EMPTY))
    } else {
        Span::styled(
            glyph,
            Style::default().fg(activity_color(tokens, max_tokens)),
        )
    };
    if cell_width <= 1 {
        vec![cell]
    } else {
        vec![cell, Span::raw(" ")]
    }
}

fn activity_legend_line() -> Line<'static> {
    Line::from(vec![
        muted("less "),
        Span::styled("■", Style::default().fg(ACTIVITY_EMPTY)),
        Span::raw(" "),
        Span::styled("■", Style::default().fg(ACTIVITY_LOW)),
        Span::raw(" "),
        Span::styled("■", Style::default().fg(ACTIVITY_MEDIUM_LOW)),
        Span::raw(" "),
        Span::styled("■", Style::default().fg(ACTIVITY_MEDIUM_HIGH)),
        Span::raw(" "),
        Span::styled("■", Style::default().fg(ACTIVITY_HIGH)),
        muted(" more"),
    ])
}

fn quota_projection(window: Option<&RateWindow>) -> (String, Color) {
    let Some(window) = window else {
        return ("waiting for quota".to_string(), MUTED);
    };
    let used = window.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    if used >= 100.0 {
        return ("empty until reset".to_string(), ROSE);
    }

    let Some(seconds_to_empty) = projected_empty_seconds(window) else {
        return ("on pace".to_string(), MINT);
    };
    let Some(seconds_to_reset) = reset_seconds(window) else {
        return ("on pace".to_string(), MINT);
    };

    if seconds_to_empty < seconds_to_reset {
        (
            format!("runs out in {}", format::duration(Some(seconds_to_empty))),
            if seconds_to_empty < 60 * 60 {
                ROSE
            } else {
                GOLD
            },
        )
    } else {
        ("on pace".to_string(), MINT)
    }
}

fn quota_accent(window: Option<&RateWindow>) -> Color {
    let (_, projection_color) = quota_projection(window);
    if projection_color == GOLD || projection_color == ROSE {
        projection_color
    } else {
        MINT
    }
}

fn projected_empty_seconds(window: &RateWindow) -> Option<u64> {
    let used_ratio = window.used_percent?.clamp(0.0, 100.0) / 100.0;
    if used_ratio <= 0.0 {
        return None;
    }
    let window_seconds = window.window_minutes?.saturating_mul(60);
    let remaining = reset_seconds(window)?;
    let elapsed = window_seconds.saturating_sub(remaining);
    if elapsed == 0 {
        return None;
    }

    let remaining_at_current_rate = (elapsed as f64 * ((1.0 / used_ratio) - 1.0)).max(0.0);
    Some(remaining_at_current_rate.round() as u64)
}

fn reset_seconds(window: &RateWindow) -> Option<u64> {
    let reset = UNIX_EPOCH + std::time::Duration::from_secs(window.resets_at?);
    reset
        .duration_since(SystemTime::now())
        .ok()
        .map(|d| d.as_secs())
}

fn quota_remaining_line(width: usize, ratio: f64) -> Line<'static> {
    let percent = format!("{:.0}%", (ratio * 100.0).clamp(0.0, 999.0));
    let reserved = "left ".len() + percent.len() + 1;
    let segments = width.saturating_sub(reserved).max(1);
    let remaining = ratio.clamp(0.0, 1.0);
    let mut filled = ((segments as f64 * remaining).round() as usize).clamp(0, segments);
    if remaining > 0.0 && filled == 0 {
        filled = 1;
    }
    let empty = segments - filled;
    let accent = remaining_color(remaining);

    let mut spans = vec![muted("left ")];
    spans.extend((0..filled).map(|_| Span::styled("▮", Style::default().fg(accent))));
    spans.extend((0..empty).map(|_| Span::styled("▮", Style::default().fg(TRACK_BOX))));
    spans.push(Span::raw(" "));
    spans.push(value(&percent, TEXT));
    Line::from(spans)
}

fn activity_panel_area(area: Rect) -> Rect {
    Rect {
        height: area.height.min(ACTIVITY_PANEL_MAX_HEIGHT),
        ..area
    }
}

fn quota_status_line(
    width: usize,
    reset: &str,
    projection: &str,
    projection_color: Color,
) -> Line<'static> {
    split_line(
        width,
        vec![muted("resets "), value(reset, TEXT)],
        vec![value(projection, projection_color)],
    )
}

fn remaining_color(remaining: f64) -> Color {
    if remaining <= 0.15 {
        ROSE
    } else if remaining <= 0.35 {
        GOLD
    } else {
        MINT
    }
}

fn short_date(date: &str) -> String {
    let month = month_name(date.get(5..7).unwrap_or(""));
    let day = date
        .get(8..10)
        .and_then(|day| day.parse::<u8>().ok())
        .unwrap_or(0);
    format!("{month} {day}")
}

fn month_name(month: &str) -> &'static str {
    match month {
        "01" => "Jan",
        "02" => "Feb",
        "03" => "Mar",
        "04" => "Apr",
        "05" => "May",
        "06" => "Jun",
        "07" => "Jul",
        "08" => "Aug",
        "09" => "Sep",
        "10" => "Oct",
        "11" => "Nov",
        "12" => "Dec",
        _ => "--",
    }
}

fn today_days() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

fn metric(label: &str, metric_value: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!("{metric_value} {label}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn separator() -> Span<'static> {
    muted("  |  ")
}

fn header_line(width: u16, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let width = width as usize;
    let right_margin = 1;
    let mut line = split_line(width.saturating_sub(right_margin), left, right).spans;
    let remaining = width.saturating_sub(spans_width(&line));
    if remaining > 0 {
        line.push(Span::raw(" ".repeat(remaining)));
    }
    Line::from(line)
}

fn split_line(
    width: usize,
    mut left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) -> Line<'static> {
    let left_width = spans_width(&left);
    let right_width = spans_width(&right);
    let gap = width.saturating_sub(left_width + right_width);
    if gap > 0 {
        left.push(Span::raw(" ".repeat(gap)));
    } else {
        left.push(Span::raw(" "));
    }
    left.extend(right);
    Line::from(left)
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn panel(title: impl Into<String>, accent: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
}

fn panel_inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(2),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    }
}

fn value(text: &str, color: Color) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(color))
}

fn value_bold(text: &str, color: Color) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn muted(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(MUTED))
}

fn key(text: &str) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
    )
}

fn inset(area: Rect, margin: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(margin),
        y: area.y.saturating_add(margin),
        width: area.width.saturating_sub(margin * 2),
        height: area.height.saturating_sub(margin * 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_color_changes_at_pressure_thresholds() {
        assert_eq!(remaining_color(0.10), ROSE);
        assert_eq!(remaining_color(0.25), GOLD);
        assert_eq!(remaining_color(0.60), MINT);
    }

    #[test]
    fn activity_calendar_renders_month_header_and_week_rows() {
        let lines = activity_calendar_lines(
            200,
            16,
            &[
                DailyUsage {
                    date: "2026-05-01".to_string(),
                    tokens: 5,
                    cost_usd: None,
                    sessions: 1,
                },
                DailyUsage {
                    date: "2026-06-20".to_string(),
                    tokens: 10,
                    cost_usd: None,
                    sessions: 1,
                },
            ],
        );

        assert_eq!(lines.len(), 8);
        assert!(line_text(&lines[0]).contains("May"));
        assert!(line_text(&lines[0]).contains("Jun"));
    }

    #[test]
    fn activity_calendar_uses_spaced_local_range_geometry_when_it_fits() {
        let geometry = activity_calendar_geometry(
            90,
            date_days("2026-01-05").expect("date"),
            date_days("2026-06-03").expect("date"),
        );

        assert_eq!(geometry.bands.len(), 1);
        assert_eq!(geometry.bands[0].columns, 43);
        assert_eq!(geometry.bands[0].cell_width, 2);
        assert_eq!(geometry.bands[0].left_padding, 0);
    }

    #[test]
    fn activity_calendar_caps_long_ranges_to_53_weeks() {
        let newest = date_days("2026-06-03").expect("date");
        let geometry = activity_calendar_geometry(120, newest - 700, newest);

        assert_eq!(geometry.bands.len(), 1);
        assert_eq!(geometry.bands[0].columns, 53);
        assert_eq!(geometry.bands[0].cell_width, 2);
        assert_eq!(geometry.bands[0].left_padding, 0);
    }

    #[test]
    fn activity_calendar_uses_recent_spaced_view_when_full_year_would_not_fit() {
        let newest = date_days("2026-06-03").expect("date");
        let geometry = activity_calendar_geometry(90, newest - 364, newest);

        assert_eq!(geometry.bands.len(), 1);
        assert_eq!(geometry.bands[0].columns, 43);
        assert_eq!(geometry.bands[0].cell_width, 2);
        assert_eq!(geometry.bands[0].left_padding, 0);
    }

    #[test]
    fn activity_calendar_uses_as_many_spaced_weeks_as_fit_when_narrow() {
        let newest = date_days("2026-06-03").expect("date");
        let geometry = activity_calendar_geometry(50, newest - 364, newest);

        assert_eq!(geometry.bands.len(), 1);
        assert_eq!(geometry.bands[0].columns, 23);
        assert_eq!(geometry.bands[0].cell_width, 2);
        assert_eq!(geometry.bands[0].left_padding, 0);
    }

    #[test]
    fn activity_cells_use_smaller_glyphs_in_compact_mode() {
        assert_eq!(line_text(&Line::from(activity_cell(10, 10, 1))), "■");
        assert_eq!(line_text(&Line::from(activity_cell(10, 10, 2))), "■ ");
        assert_eq!(line_text(&Line::from(activity_cell(0, 10, 1))), "■");
    }

    #[test]
    fn activity_color_uses_empty_plus_four_active_levels() {
        assert_eq!(activity_color(0, 1_000), ACTIVITY_EMPTY);
        assert_eq!(activity_color(25, 1_000), ACTIVITY_LOW);
        assert_eq!(activity_color(100, 1_000), ACTIVITY_MEDIUM_LOW);
        assert_eq!(activity_color(600, 1_000), ACTIVITY_MEDIUM_HIGH);
        assert_eq!(activity_color(800, 1_000), ACTIVITY_HIGH);
    }

    #[test]
    fn activity_intensity_ratio_is_peak_relative_and_skips_empty_days() {
        assert_eq!(activity_intensity_ratio(0, 1_000), None);
        assert_eq!(activity_intensity_ratio(1_000, 1_000), Some(1.0));
        assert_eq!(activity_intensity_ratio(50, 1_000), Some(ACTIVITY_LOW_MAX));
    }

    #[test]
    fn activity_legend_labels_less_to_more() {
        let line = activity_legend_line();
        let text = line_text(&line);

        assert!(text.starts_with("less "));
        assert!(text.ends_with(" more"));
        assert_eq!(text.matches('■').count(), 5);
    }

    #[test]
    fn quota_meter_shows_remaining_reserve_not_used_pressure() {
        let line = quota_remaining_line(18, 0.25);

        assert!(line_text(&line).contains("left"));
        assert_eq!(spans_width(&line.spans), 18);
        assert_eq!(line_text(&line).matches('▮').count(), 9);
        assert!(line_text(&line).ends_with("25%"));
    }

    #[test]
    fn quota_meter_uses_available_card_width() {
        let line = quota_remaining_line(44, 0.50);

        assert_eq!(spans_width(&line.spans), 44);
        assert_eq!(line_text(&line).matches('▮').count(), 35);
        assert!(line_text(&line).ends_with("50%"));
    }

    #[test]
    fn activity_panel_height_caps_to_calendar_content() {
        let area = Rect::new(2, 3, 80, 24);
        let capped = activity_panel_area(area);

        assert_eq!(capped.x, area.x);
        assert_eq!(capped.y, area.y);
        assert_eq!(capped.width, area.width);
        assert_eq!(capped.height, ACTIVITY_PANEL_MAX_HEIGHT);
    }

    #[test]
    fn activity_panel_keeps_short_allocations() {
        let area = Rect::new(2, 3, 80, 8);

        assert_eq!(activity_panel_area(area).height, 8);
    }

    #[test]
    fn header_status_only_names_freshness() {
        let scanned_at = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .expect("time before now");
        let snapshot = MeterSnapshot {
            scanned_at: Some(scanned_at),
            ..MeterSnapshot::default()
        };
        let spans = header_status_spans(&snapshot);
        let text = line_text(&Line::from(spans));

        assert!(text.starts_with("updated "));
        assert!(!text.contains("refresh"));
    }

    #[test]
    fn quota_status_line_right_aligns_projection() {
        let line = quota_status_line(30, "3h 40m", "on pace", MINT);
        let text = line_text(&line);

        assert_eq!(text.len(), 30);
        assert!(text.starts_with("resets 3h 40m"));
        assert!(text.ends_with("on pace"));
    }

    #[test]
    fn header_line_places_status_on_the_right() {
        let line = header_line(30, vec![value("left", TEXT)], vec![value("right", TEXT)]);

        assert_eq!(line_text(&line), "left                    right ");
    }

    #[test]
    fn daily_series_fills_empty_days() {
        let daily = vec![DailyUsage {
            date: "2026-06-03".to_string(),
            tokens: 10,
            cost_usd: None,
            sessions: 1,
        }];

        let series = daily_token_series(&daily, 3);

        assert_eq!(series, vec![0, 0, 10]);
    }

    #[test]
    fn token_volume_chart_renders_spaced_day_columns() {
        let daily = vec![
            DailyUsage {
                date: "2026-06-01".to_string(),
                tokens: 10,
                cost_usd: None,
                sessions: 1,
            },
            DailyUsage {
                date: "2026-06-02".to_string(),
                tokens: 0,
                cost_usd: None,
                sessions: 0,
            },
            DailyUsage {
                date: "2026-06-03".to_string(),
                tokens: 30,
                cost_usd: None,
                sessions: 1,
            },
        ];

        let lines = token_volume_lines(20, 4, &daily, 3);

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].spans.len(), 6);
    }

    #[test]
    fn projection_marks_over_budget_pace() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_secs();
        let window = RateWindow {
            used_percent: Some(90.0),
            window_minutes: Some(300),
            resets_at: Some(now + 4 * 60 * 60),
        };

        let (label, _) = quota_projection(Some(&window));

        assert!(label.starts_with("runs out in"));
    }

    #[test]
    fn cost_label_names_indexing_state() {
        assert_eq!(cost_label(None, CostStatus::Indexing), "indexing...");
        assert_eq!(cost_label(None, CostStatus::Ready), "$--");
        assert_eq!(cost_label(Some(4.25), CostStatus::Indexing), "$4.25");
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }
}
