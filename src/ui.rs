use std::{
    io::{self, Stdout},
    path::PathBuf,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Padding, Paragraph, Row, Sparkline, Table},
};

use crate::{
    cli::DashboardOptions,
    codex::{MeterSnapshot, RateWindow, SessionSummary, TokenUsage, scan_codex_home},
    error::{AppError, AppResult},
    format,
};

const BG: Color = Color::Rgb(6, 8, 13);
const PANEL: Color = Color::Rgb(12, 15, 23);
const BORDER: Color = Color::Rgb(47, 57, 78);
const BORDER_DIM: Color = Color::Rgb(31, 38, 54);
const TEXT: Color = Color::Rgb(214, 220, 237);
const MUTED: Color = Color::Rgb(117, 126, 153);
const TRACK: Color = Color::Rgb(28, 34, 48);
const MINT: Color = Color::Rgb(126, 231, 168);
const GOLD: Color = Color::Rgb(246, 196, 104);
const ROSE: Color = Color::Rgb(255, 106, 136);
const SKY: Color = Color::Rgb(116, 166, 245);
const LILAC: Color = Color::Rgb(203, 152, 255);
const TEAL: Color = Color::Rgb(119, 221, 211);

pub fn run(codex_home: PathBuf, options: DashboardOptions) -> AppResult<()> {
    let mut terminal = TerminalSession::enter()?;
    let mut snapshot = None;
    let mut error = None;
    let mut last_scan = Instant::now() - options.refresh;

    loop {
        if last_scan.elapsed() >= options.refresh {
            match scan_codex_home(&codex_home, options.max_files) {
                Ok(next) => {
                    snapshot = Some(next);
                    error = None;
                }
                Err(next_error) => error = Some(next_error.to_string()),
            }
            last_scan = Instant::now();
        }

        terminal
            .terminal
            .draw(|frame| draw(frame, snapshot.as_ref(), error.as_deref(), &options))
            .map_err(|source| AppError::io("failed to draw terminal frame", source))?;

        if event::poll(Duration::from_millis(150))
            .map_err(|source| AppError::io("failed to poll terminal events", source))?
        {
            let event = event::read()
                .map_err(|source| AppError::io("failed to read terminal event", source))?;
            match input_action(event) {
                InputAction::Quit => break,
                InputAction::Refresh => last_scan = Instant::now() - options.refresh,
                InputAction::None => {}
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAction {
    Quit,
    Refresh,
    None,
}

fn input_action(event: Event) -> InputAction {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                InputAction::Quit
            } else if key.code == KeyCode::Char('r') {
                InputAction::Refresh
            } else {
                InputAction::None
            }
        }
        _ => InputAction::None,
    }
}

fn draw(
    frame: &mut Frame<'_>,
    snapshot: Option<&MeterSnapshot>,
    error: Option<&str>,
    options: &DashboardOptions,
) {
    let area = frame.area();
    if area.width < 76 || area.height < 22 {
        draw_too_small(frame, area);
        return;
    }

    let shell = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(BG))
        .title(Line::from(vec![
            Span::styled(
                " codex",
                Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-meter ",
                Style::default().fg(MINT).add_modifier(Modifier::BOLD),
            ),
        ]))
        .title_alignment(Alignment::Left);
    frame.render_widget(shell, area);

    let inner = inset(area, 1);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .spacing(1)
        .split(inner);

    draw_header(frame, rows[0], snapshot, error, options);
    draw_quota_cards(frame, rows[1], snapshot);
    draw_body(frame, rows[2], snapshot);
    draw_footer(frame, rows[3], snapshot);
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new("codex-meter needs at least 76x22")
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
    options: &DashboardOptions,
) {
    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let model = latest
        .and_then(|session| session.model.as_deref())
        .unwrap_or("unknown");
    let plan = snapshot
        .and_then(|snapshot| snapshot.current_rate_limits.as_ref())
        .and_then(|limits| limits.plan_type.as_deref())
        .unwrap_or("--");

    let status = error
        .map(|message| format!("error: {message}"))
        .unwrap_or_else(|| {
            snapshot
                .map(|snapshot| {
                    format!(
                        "tracking {} recent sessions  |  refresh {}s",
                        snapshot.scanned_files,
                        options.refresh.as_secs()
                    )
                })
                .unwrap_or_else(|| "scanning local Codex logs".to_string())
        });

    let mut identity = vec![
        muted("plan "),
        value(plan, MINT),
        muted("  model "),
        value(model, TEAL),
    ];
    if let Some(provider) = latest.and_then(|session| session.provider.as_deref()) {
        identity.extend([muted("  provider "), value(provider, TEXT)]);
    }

    let lines = vec![
        Line::from(identity),
        Line::from(Span::styled(
            status,
            Style::default().fg(if error.is_some() { ROSE } else { MUTED }),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(BG)), area);
}

fn draw_quota_cards(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .spacing(1)
        .split(area);

    let limits = snapshot.and_then(|snapshot| snapshot.current_rate_limits.as_ref());
    draw_rate_card(
        frame,
        chunks[0],
        "weekly left",
        limits.and_then(|limits| limits.secondary.as_ref()),
    );
    draw_rate_card(
        frame,
        chunks[1],
        "5h left",
        limits.and_then(|limits| limits.primary.as_ref()),
    );
    draw_context_card(frame, chunks[2], snapshot);
}

fn draw_rate_card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    window: Option<&RateWindow>,
) {
    let used = window.and_then(|window| window.used_percent);
    let remaining = remaining_ratio(used);
    let accent = remaining_color(remaining);
    let remaining_text = format::remaining_percent(used);
    let used_text = format::percent(used);
    let reset = format::reset_in(window.and_then(|window| window.resets_at));
    let bar_width = area.width.saturating_sub(6).max(8) as usize;

    let lines = vec![
        Line::from(vec![value(&remaining_text, accent), muted(" remaining")]),
        bar_line(bar_width, remaining, accent),
        Line::from(vec![
            muted(&used_text),
            muted(" used  reset "),
            value(&reset, TEXT),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines).block(panel(title, accent)), area);
}

fn draw_context_card(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let last_total = latest
        .map(|session| session.last_usage.total_tokens)
        .unwrap_or_default();
    let context = latest
        .and_then(|session| session.context_window)
        .unwrap_or(0);
    let used_ratio = if context == 0 {
        0.0
    } else {
        (last_total as f64 / context as f64).clamp(0.0, 1.0)
    };
    let remaining = 1.0 - used_ratio;
    let remaining_tokens = context.saturating_sub(last_total);
    let accent = remaining_color(remaining);
    let bar_width = area.width.saturating_sub(6).max(8) as usize;

    let lines = vec![
        Line::from(vec![
            value(&format::tokens(remaining_tokens), accent),
            muted(" headroom"),
        ]),
        bar_line(bar_width, remaining, accent),
        Line::from(vec![
            muted(&format!("{:.0}% used", used_ratio * 100.0)),
            muted("  "),
            value(&format::tokens(context), TEXT),
            muted(" max"),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(panel("context left", accent)),
        area,
    );
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .spacing(1)
        .split(area);
    draw_turn_panel(frame, chunks[0], snapshot);
    draw_recent_panel(frame, chunks[1], snapshot);
}

fn draw_turn_panel(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let usage = latest.map(|session| session.last_usage).unwrap_or_default();
    let cache_ratio = cache_ratio(usage);

    let lines = vec![
        Line::from(vec![
            muted("total "),
            value(&format::tokens(usage.total_tokens), TEAL),
        ]),
        stat_line("input", usage.input_tokens),
        stat_line("cached", usage.cached_input_tokens),
        stat_line("output", usage.output_tokens),
        stat_line("reasoning", usage.reasoning_output_tokens),
        Line::raw(""),
        Line::from(vec![
            muted("cache ratio "),
            value(&format!("{:.0}%", cache_ratio * 100.0), MINT),
        ]),
        bar_line(
            area.width.saturating_sub(6).max(8) as usize,
            cache_ratio,
            MINT,
        ),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(panel("latest turn", TEAL)),
        area,
    );
}

fn draw_recent_panel(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(6)])
        .spacing(1)
        .split(area);

    let data = snapshot
        .map(|snapshot| {
            snapshot
                .recent_sessions
                .iter()
                .rev()
                .map(|session| session.last_usage.total_tokens)
                .collect::<Vec<u64>>()
        })
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec![0]);

    let sparkline = Sparkline::default()
        .block(panel("recent turn size", LILAC))
        .style(Style::default().fg(LILAC))
        .data(&data);
    frame.render_widget(sparkline, rows[0]);

    let table = Table::new(recent_rows(snapshot), recent_columns(rows[1].width))
        .block(panel("recent sessions", SKY))
        .header(
            Row::new([
                Cell::from("updated"),
                Cell::from("turn"),
                Cell::from("context"),
            ])
            .style(muted_style()),
        );
    frame.render_widget(table, rows[1]);
}

fn recent_rows(snapshot: Option<&MeterSnapshot>) -> Vec<Row<'static>> {
    let Some(snapshot) = snapshot else {
        return vec![Row::new([
            Cell::from("--"),
            Cell::from("--"),
            Cell::from("--"),
        ])];
    };

    snapshot
        .recent_sessions
        .iter()
        .take(8)
        .map(|session| {
            Row::new([
                Cell::from(format::age(session.modified_at)).style(Style::default().fg(TEXT)),
                Cell::from(format::tokens(session.last_usage.total_tokens))
                    .style(Style::default().fg(TEAL)),
                Cell::from(context_used_text(session)).style(Style::default().fg(MINT)),
            ])
        })
        .collect()
}

fn recent_columns(width: u16) -> [Constraint; 3] {
    if width < 42 {
        [
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(9),
        ]
    } else {
        [
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
        ]
    }
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let mut spans = vec![
        key("q"),
        muted(" quit   "),
        key("esc"),
        muted(" close   "),
        key("r"),
        muted(" refresh   "),
        muted("updated "),
        value(
            &snapshot
                .map(|snapshot| format::age(snapshot.scanned_at))
                .unwrap_or_else(|| "--".to_string()),
            TEXT,
        ),
    ];

    if let Some(snapshot) = snapshot
        && snapshot.malformed_lines > 0
    {
        spans.extend([
            muted("   parse warnings "),
            value(&snapshot.malformed_lines.to_string(), GOLD),
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

fn stat_line(label: &'static str, tokens: u64) -> Line<'static> {
    Line::from(vec![
        muted(label),
        Span::raw(" "),
        value(&format::tokens(tokens), TEXT),
    ])
}

fn bar_line(width: usize, ratio: f64, accent: Color) -> Line<'static> {
    let width = width.clamp(8, 48);
    let filled = ((width as f64 * ratio).round() as usize).clamp(0, width);
    let empty = width - filled;

    Line::from(vec![
        Span::styled("█".repeat(filled), Style::default().fg(accent)),
        Span::styled("░".repeat(empty), Style::default().fg(TRACK)),
    ])
}

fn cache_ratio(usage: TokenUsage) -> f64 {
    if usage.input_tokens == 0 {
        0.0
    } else {
        (usage.cached_input_tokens as f64 / usage.input_tokens as f64).clamp(0.0, 1.0)
    }
}

fn context_used_text(session: &SessionSummary) -> String {
    let Some(context_window) = session.context_window else {
        return "--".to_string();
    };
    if context_window == 0 {
        return "--".to_string();
    }

    format!(
        "{:.0}%",
        (session.last_usage.total_tokens as f64 / context_window as f64).clamp(0.0, 1.0) * 100.0
    )
}

fn remaining_ratio(used_percent: Option<f64>) -> f64 {
    1.0 - format::ratio(used_percent)
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

fn panel(title: impl Into<String>, accent: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {} ", title.into()),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
}

fn value(text: &str, color: Color) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(color))
}

fn muted(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), muted_style())
}

fn key(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(ROSE))
}

fn muted_style() -> Style {
    Style::default().fg(MUTED)
}

fn inset(area: Rect, margin: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(margin),
        y: area.y.saturating_add(margin),
        width: area.width.saturating_sub(margin * 2),
        height: area.height.saturating_sub(margin * 2),
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> AppResult<Self> {
        enable_raw_mode().map_err(|source| AppError::io("failed to enable raw mode", source))?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .map_err(|source| AppError::io("failed to enter alternate screen", source))?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)
            .map_err(|source| AppError::io("failed to initialize terminal", source))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_ratio_inverts_used_percent() {
        assert_eq!(remaining_ratio(Some(75.0)), 0.25);
        assert_eq!(remaining_ratio(Some(0.0)), 1.0);
    }

    #[test]
    fn cache_ratio_handles_empty_input() {
        assert_eq!(cache_ratio(TokenUsage::default()), 0.0);
        assert_eq!(
            cache_ratio(TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 75,
                ..TokenUsage::default()
            }),
            0.75
        );
    }

    #[test]
    fn remaining_color_changes_at_pressure_thresholds() {
        assert_eq!(remaining_color(0.10), ROSE);
        assert_eq!(remaining_color(0.25), GOLD);
        assert_eq!(remaining_color(0.60), MINT);
    }

    #[test]
    fn recent_rows_render_missing_snapshot_placeholder() {
        assert_eq!(recent_rows(None).len(), 1);
    }

    #[test]
    fn context_used_text_formats_percentage() {
        let session = SessionSummary {
            context_window: Some(200),
            last_usage: TokenUsage {
                total_tokens: 50,
                ..TokenUsage::default()
            },
            ..SessionSummary::default()
        };

        assert_eq!(context_used_text(&session), "25%");
    }
}
