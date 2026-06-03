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
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table, Wrap},
};

use crate::{
    cli::DashboardOptions,
    codex::{MeterSnapshot, RateWindow, SessionSummary, scan_codex_home},
    error::{AppError, AppResult},
    format,
};

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
            if should_quit(event) {
                break;
            }
        }
    }

    Ok(())
}

fn should_quit(event: Event) -> bool {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        }
        _ => false,
    }
}

fn draw(
    frame: &mut Frame<'_>,
    snapshot: Option<&MeterSnapshot>,
    error: Option<&str>,
    options: &DashboardOptions,
) {
    let area = frame.area();
    if area.width < 72 || area.height < 22 {
        draw_too_small(frame, area);
        return;
    }

    let shell = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::default().bg(Color::Rgb(10, 12, 18)))
        .title(Line::from(vec![
            Span::styled(
                " codex",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "-meter ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .title_alignment(Alignment::Left);
    frame.render_widget(shell, area);

    let inner = inset(area, 1);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(inner);

    draw_header(frame, rows[0], snapshot, error, options);
    draw_meters(frame, rows[1], snapshot);
    draw_body(frame, rows[2], snapshot);
    draw_footer(frame, rows[3], snapshot);
}

fn draw_too_small(frame: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new("codex-meter needs at least 72x22")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn draw_header(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: Option<&MeterSnapshot>,
    error: Option<&str>,
    options: &DashboardOptions,
) {
    let status = error
        .map(|message| format!("error: {message}"))
        .unwrap_or_else(|| {
            snapshot
                .map(|snapshot| {
                    format!(
                        "{} scanned / {} available | refresh {}s",
                        snapshot.scanned_files,
                        snapshot.available_session_files,
                        options.refresh.as_secs()
                    )
                })
                .unwrap_or_else(|| "scanning Codex logs".to_string())
        });

    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let model = latest
        .and_then(|session| session.model.as_deref())
        .unwrap_or("unknown model");
    let plan = snapshot
        .and_then(|snapshot| snapshot.current_rate_limits.as_ref())
        .and_then(|limits| limits.plan_type.as_deref())
        .unwrap_or("unknown plan");

    let lines = vec![
        Line::from(vec![
            Span::styled("model ", Style::default().fg(Color::DarkGray)),
            Span::styled(model, Style::default().fg(Color::LightCyan)),
            Span::raw("   "),
            Span::styled("plan ", Style::default().fg(Color::DarkGray)),
            Span::styled(plan, Style::default().fg(Color::LightGreen)),
        ]),
        Line::from(Span::styled(
            status,
            Style::default().fg(if error.is_some() {
                Color::Red
            } else {
                Color::Gray
            }),
        )),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_meters(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);

    let limits = snapshot.and_then(|snapshot| snapshot.current_rate_limits.as_ref());
    draw_window_gauge(
        frame,
        chunks[0],
        "primary 5h",
        limits.and_then(|limits| limits.primary.as_ref()),
        Color::LightGreen,
    );
    draw_window_gauge(
        frame,
        chunks[1],
        "secondary 7d",
        limits.and_then(|limits| limits.secondary.as_ref()),
        Color::LightMagenta,
    );
    draw_context_gauge(frame, chunks[2], snapshot);
}

fn draw_window_gauge(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    window: Option<&RateWindow>,
    color: Color,
) {
    let label = format::window_line(title, window);
    let gauge = Gauge::default()
        .block(panel(title))
        .gauge_style(Style::default().fg(color).bg(Color::Rgb(28, 32, 44)))
        .ratio(format::ratio(window.and_then(|window| window.used_percent)))
        .label(label);
    frame.render_widget(gauge, area);
}

fn draw_context_gauge(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
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

    let label = if context == 0 {
        "context --".to_string()
    } else {
        format!(
            "last turn {} / {}",
            format::tokens(last_total),
            format::tokens(context)
        )
    };

    let gauge = Gauge::default()
        .block(panel("context"))
        .gauge_style(
            Style::default()
                .fg(Color::LightBlue)
                .bg(Color::Rgb(28, 32, 44)),
        )
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
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

    let mut lines = vec![
        Line::from(vec![
            Span::styled("last turn ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format::tokens(last.total_tokens),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format::usage_parts(last).join("   ")),
        Line::raw(""),
        Line::from(vec![
            Span::styled("scanned total ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format::tokens(total.total_tokens),
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format::usage_parts(total).join("   ")),
        Line::raw(""),
    ];

    if let Some(session) = latest {
        lines.push(Line::from(vec![
            Span::styled("latest file ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                session
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unknown"),
                Style::default().fg(Color::Gray),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("updated ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format::age(session.modified_at),
                Style::default().fg(Color::Gray),
            ),
            Span::raw("   "),
            Span::styled("scan ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if session.tail_scanned { "tail" } else { "full" },
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("tokens"))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_activity_panel(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(4)])
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
        .block(panel("recent session totals"))
        .style(Style::default().fg(Color::Cyan))
        .data(&data);
    frame.render_widget(sparkline, rows[0]);

    let table_rows = event_rows(snapshot);
    let table = Table::new(
        table_rows,
        [Constraint::Percentage(70), Constraint::Percentage(30)],
    )
    .block(panel("events"))
    .header(
        Row::new([Cell::from("event"), Cell::from("count")])
            .style(Style::default().fg(Color::DarkGray)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(table, rows[1]);
}

fn event_rows(snapshot: Option<&MeterSnapshot>) -> Vec<Row<'static>> {
    let Some(snapshot) = snapshot else {
        return vec![Row::new([Cell::from("waiting"), Cell::from("--")])];
    };

    let mut events = snapshot
        .event_counts
        .iter()
        .map(|(event, count)| (event.clone(), *count))
        .collect::<Vec<_>>();
    events.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    events
        .into_iter()
        .take(8)
        .map(|(event, count)| {
            Row::new([
                Cell::from(event).style(Style::default().fg(Color::Gray)),
                Cell::from(count.to_string()).style(Style::default().fg(Color::LightGreen)),
            ])
        })
        .collect()
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, snapshot: Option<&MeterSnapshot>) {
    let latest = snapshot.and_then(|snapshot| snapshot.latest_session.as_ref());
    let line = Line::from(vec![
        Span::styled("q/esc ", Style::default().fg(Color::LightRed)),
        Span::styled("quit", Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled("malformed ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            snapshot
                .map(|snapshot| snapshot.malformed_lines.to_string())
                .unwrap_or_else(|| "--".to_string()),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("   "),
        Span::styled("tail files ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            snapshot
                .map(|snapshot| snapshot.tail_scanned_files.to_string())
                .unwrap_or_else(|| "--".to_string()),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("   "),
        Span::styled("scan ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            snapshot
                .map(|snapshot| format::age(snapshot.scanned_at))
                .unwrap_or_else(|| "--".to_string()),
            Style::default().fg(Color::Gray),
        ),
        Span::raw("   "),
        Span::styled("latest event ", Style::default().fg(Color::DarkGray)),
        Span::styled(latest_event(latest), Style::default().fg(Color::Gray)),
    ]);

    frame.render_widget(
        Paragraph::new(line).alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Color::DarkGray),
        ),
        area,
    );
}

fn latest_event(session: Option<&SessionSummary>) -> String {
    session
        .and_then(|session| session.last_event_at.clone())
        .unwrap_or_else(|| "--".to_string())
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(62, 70, 88)))
        .style(Style::default().bg(Color::Rgb(14, 17, 25)))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ))
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
