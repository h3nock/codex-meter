use std::{
    collections::BTreeMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
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
    codex::{MeterSnapshot, RateWindow, scan_codex_home},
    error::{AppError, AppResult},
    format,
};

const BG: Color = Color::Rgb(7, 10, 16);
const PANEL: Color = Color::Rgb(12, 16, 24);
const BORDER: Color = Color::Rgb(54, 64, 84);
const BORDER_DIM: Color = Color::Rgb(35, 43, 60);
const TEXT: Color = Color::Rgb(204, 212, 232);
const MUTED: Color = Color::Rgb(108, 116, 140);
const TRACK: Color = Color::Rgb(31, 38, 52);
const CYAN: Color = Color::Rgb(124, 236, 224);
const GREEN: Color = Color::Rgb(141, 235, 159);
const MAGENTA: Color = Color::Rgb(238, 151, 220);
const BLUE: Color = Color::Rgb(111, 158, 239);
const AMBER: Color = Color::Rgb(245, 199, 116);
const RED: Color = Color::Rgb(255, 111, 134);

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
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-meter ",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
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
    draw_meters(frame, rows[1], snapshot);
    draw_body(frame, rows[2], snapshot);
    draw_footer(frame, rows[3], snapshot);
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new("codex-meter needs at least 76x22")
        .alignment(Alignment::Center)
        .style(Style::default().fg(AMBER).bg(BG))
        .block(panel("codex-meter", AMBER));
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
                        "{} / {} sessions  |  {} archived  |  refresh {}s",
                        snapshot.scanned_files,
                        snapshot.available_session_files,
                        snapshot.archived_session_files,
                        options.refresh.as_secs()
                    )
                })
                .unwrap_or_else(|| "scanning local Codex logs".to_string())
        });

    let mut identity = vec![muted("model "), value(model, CYAN)];
    if let Some(provider) = latest.and_then(|session| session.provider.as_deref()) {
        identity.extend([muted("  provider "), value(provider, TEXT)]);
    }
    identity.extend([muted("  plan "), value(plan, GREEN)]);

    let lines = vec![
        Line::from(identity),
        Line::from(Span::styled(
            status,
            Style::default().fg(if error.is_some() { RED } else { MUTED }),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(BG)), area);
}

fn draw_meters(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34),
        ])
        .spacing(1)
        .split(area);

    let limits = snapshot.and_then(|snapshot| snapshot.current_rate_limits.as_ref());
    draw_rate_card(
        frame,
        chunks[0],
        "weekly usage",
        limits.and_then(|limits| limits.secondary.as_ref()),
        GREEN,
    );
    draw_rate_card(
        frame,
        chunks[1],
        "5h usage",
        limits.and_then(|limits| limits.primary.as_ref()),
        AMBER,
    );
    draw_context_card(frame, chunks[2], snapshot);
}

fn draw_rate_card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    window: Option<&RateWindow>,
    accent: Color,
) {
    let ratio = format::ratio(window.and_then(|window| window.used_percent));
    let percent = format::percent(window.and_then(|window| window.used_percent));
    let reset = format::reset_in(window.and_then(|window| window.resets_at));
    let bar_width = area.width.saturating_sub(6).max(8) as usize;

    let lines = vec![
        Line::from(vec![value(&percent, accent), muted(" used")]),
        bar_line(bar_width, ratio, accent),
        Line::from(vec![muted("resets in "), value(&reset, TEXT)]),
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
        .unwrap_or_default();
    let ratio = if context == 0 {
        0.0
    } else {
        (last_total as f64 / context as f64).clamp(0.0, 1.0)
    };
    let percent = format!("{:.0}%", ratio * 100.0);
    let bar_width = area.width.saturating_sub(6).max(8) as usize;

    let lines = vec![
        Line::from(vec![
            value(&format::tokens(last_total), BLUE),
            muted(" / "),
            value(&format::tokens(context), TEXT),
        ]),
        bar_line(bar_width, ratio, BLUE),
        Line::from(vec![muted("context used "), value(&percent, TEXT)]),
    ];

    frame.render_widget(Paragraph::new(lines).block(panel("context", BLUE)), area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
        .spacing(1)
        .split(area);
    draw_token_panel(frame, chunks[0], snapshot);
    draw_activity_panel(frame, chunks[1], snapshot);
}

fn draw_token_panel(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let last = latest.map(|session| session.last_usage).unwrap_or_default();
    let total = snapshot
        .map(|snapshot| snapshot.scanned_total_usage)
        .unwrap_or_default();
    let content_width = area.width.saturating_sub(6) as usize;

    let mut lines = vec![
        metric_line("last turn", last.total_tokens, CYAN),
        usage_line("in", last.input_tokens, "cached", last.cached_input_tokens),
        usage_line(
            "out",
            last.output_tokens,
            "reason",
            last.reasoning_output_tokens,
        ),
        Line::raw(""),
        metric_line("scanned total", total.total_tokens, GREEN),
        usage_line(
            "in",
            total.input_tokens,
            "cached",
            total.cached_input_tokens,
        ),
        usage_line(
            "out",
            total.output_tokens,
            "reason",
            total.reasoning_output_tokens,
        ),
    ];

    if let Some(session) = latest {
        let file = compact_file_name(&session.path, content_width.saturating_sub(7));
        let scan_mode = if session.tail_scanned {
            format!("tail {}", bytes(session.bytes_scanned))
        } else {
            "full".to_string()
        };
        lines.extend([
            Line::raw(""),
            Line::from(vec![muted("file "), value(&file, TEXT)]),
            Line::from(vec![
                muted("updated "),
                value(&format::age(session.modified_at), TEXT),
                muted("  scan "),
                value(&scan_mode, TEXT),
            ]),
        ]);
    }

    frame.render_widget(Paragraph::new(lines).block(panel("tokens", CYAN)), area);
}

fn draw_activity_panel(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(5)])
        .spacing(1)
        .split(area);

    let data = snapshot
        .map(|snapshot| {
            snapshot
                .recent_session_totals
                .iter()
                .rev()
                .copied()
                .collect::<Vec<u64>>()
        })
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec![0]);

    let sparkline = Sparkline::default()
        .block(panel("recent session totals", GREEN))
        .style(Style::default().fg(GREEN))
        .data(&data);
    frame.render_widget(sparkline, rows[0]);

    let table = Table::new(
        event_rows(snapshot),
        [Constraint::Min(16), Constraint::Length(8)],
    )
    .block(panel("events", MAGENTA))
    .header(Row::new([Cell::from("event"), Cell::from("count")]).style(muted_style()))
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(table, rows[1]);
}

fn event_rows(snapshot: Option<&MeterSnapshot>) -> Vec<Row<'static>> {
    let Some(snapshot) = snapshot else {
        return vec![Row::new([Cell::from("waiting"), Cell::from("--")])];
    };

    let mut grouped = BTreeMap::new();
    for (event, count) in &snapshot.event_counts {
        *grouped.entry(event_label(event).to_string()).or_insert(0) += count;
    }

    let mut events = grouped.into_iter().collect::<Vec<_>>();
    events.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    events
        .into_iter()
        .take(8)
        .map(|(event, count)| {
            Row::new([
                Cell::from(event).style(Style::default().fg(TEXT)),
                Cell::from(count.to_string()).style(Style::default().fg(GREEN)),
            ])
        })
        .collect()
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let line = Line::from(vec![
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
        muted("   malformed "),
        value(
            &snapshot
                .map(|snapshot| snapshot.malformed_lines.to_string())
                .unwrap_or_else(|| "--".to_string()),
            AMBER,
        ),
        muted("   tail "),
        value(
            &snapshot
                .map(|snapshot| snapshot.tail_scanned_files.to_string())
                .unwrap_or_else(|| "--".to_string()),
            AMBER,
        ),
    ]);

    frame.render_widget(
        Paragraph::new(line)
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

fn metric_line(label: &'static str, tokens: u64, accent: Color) -> Line<'static> {
    Line::from(vec![
        muted(label),
        Span::raw(" "),
        value(&format::tokens(tokens), accent),
    ])
}

fn usage_line(
    left_label: &'static str,
    left_value: u64,
    right_label: &'static str,
    right_value: u64,
) -> Line<'static> {
    Line::from(vec![
        muted(left_label),
        Span::raw(" "),
        value(&format::tokens(left_value), TEXT),
        muted("   "),
        muted(right_label),
        Span::raw(" "),
        value(&format::tokens(right_value), TEXT),
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

fn event_label(event: &str) -> &'static str {
    match event {
        "response_item/function_call_output" => "tool output",
        "response_item/function_call" => "tool calls",
        "response_item/message" => "assistant text",
        "response_item/reasoning" => "reasoning",
        "event_msg/token_count" => "token updates",
        "event_msg/agent_message" => "agent messages",
        "event_msg/user_message" => "user messages",
        "event_msg/mcp_tool_call_end" => "mcp tools",
        "event_msg/task_complete" => "tasks done",
        "event_msg/task_started" => "tasks started",
        "event_msg/context_compacted" => "compactions",
        "event_msg/patch_apply_end" => "patches",
        "response_item/custom_tool_call" => "custom tools",
        "response_item/custom_tool_call_output" => "custom output",
        "response_item/web_search_call" | "event_msg/web_search_end" => "web search",
        "session_meta" => "sessions",
        "turn_context" => "turns",
        _ => "other",
    }
}

fn compact_file_name(path: &Path, max_chars: usize) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    truncate_middle(name, max_chars)
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }

    let left = (max_chars - 1) / 2;
    let right = max_chars - 1 - left;
    let prefix = chars.iter().take(left).collect::<String>();
    let suffix = chars
        .iter()
        .skip(chars.len().saturating_sub(right))
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn bytes(value: u64) -> String {
    if value >= 1024 * 1024 {
        format!("{:.1} MiB", value as f64 / (1024.0 * 1024.0))
    } else if value >= 1024 {
        format!("{:.0} KiB", value as f64 / 1024.0)
    } else {
        format!("{value} B")
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
    Span::styled(text.to_string(), Style::default().fg(RED))
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
    fn truncates_middle_for_long_names() {
        assert_eq!(truncate_middle("abcdef", 6), "abcdef");
        assert_eq!(truncate_middle("abcdefghijkl", 7), "abc…jkl");
    }

    #[test]
    fn maps_internal_events_to_readable_labels() {
        assert_eq!(
            event_label("response_item/function_call_output"),
            "tool output"
        );
        assert_eq!(event_label("event_msg/token_count"), "token updates");
        assert_eq!(event_label("unknown/raw"), "other");
    }
}
