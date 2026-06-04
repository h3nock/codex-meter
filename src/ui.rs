use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    cli::DashboardOptions,
    codex::{CodexScanner, CostStatus, MeterSnapshot, estimate_codex_cost},
    cost_estimate::CostEstimateResult,
    error::{AppError, AppResult},
    render,
};

pub fn run(codex_home: PathBuf, options: DashboardOptions) -> AppResult<()> {
    let mut terminal = TerminalSession::enter()?;
    let mut scanner = CodexScanner::default();
    let mut snapshot = None;
    let mut error = None;
    let (cost_tx, cost_rx) = mpsc::channel();
    let (remote_tx, remote_rx) = mpsc::channel();
    let mut cost_indexing = false;
    let mut remote_scanning = false;
    let mut latest_cost = None;
    let mut last_scan = Instant::now() - options.refresh;

    terminal
        .terminal
        .draw(|frame| render::draw(frame, snapshot.as_ref(), error.as_deref(), &options))
        .map_err(|source| AppError::io("failed to draw terminal frame", source))?;

    loop {
        drain_cost_results(
            &cost_rx,
            &mut cost_indexing,
            &mut latest_cost,
            &mut snapshot,
        );
        drain_remote_results(
            &remote_rx,
            &mut remote_scanning,
            latest_cost.as_ref(),
            cost_indexing,
            &mut snapshot,
            &mut error,
        );

        if last_scan.elapsed() >= options.refresh {
            match scanner.scan_local_without_cost(&codex_home, options.max_files) {
                Ok(mut next) => {
                    apply_latest_cost(&mut next, latest_cost.as_ref(), cost_indexing);
                    if !cost_indexing {
                        start_cost_index(&codex_home, &cost_tx);
                        cost_indexing = true;
                        if latest_cost.is_none() {
                            next.cost_status = CostStatus::Indexing;
                        }
                    }
                    snapshot = Some(next);
                    error = None;
                    if !remote_scanning {
                        start_remote_scan(&codex_home, options.max_files, &remote_tx);
                        remote_scanning = true;
                    }
                }
                Err(next_error) => error = Some(next_error.to_string()),
            }
            last_scan = Instant::now();
        }

        terminal
            .terminal
            .draw(|frame| render::draw(frame, snapshot.as_ref(), error.as_deref(), &options))
            .map_err(|source| AppError::io("failed to draw terminal frame", source))?;

        if event::poll(Duration::from_millis(150))
            .map_err(|source| AppError::io("failed to poll terminal events", source))?
        {
            let event = event::read()
                .map_err(|source| AppError::io("failed to read terminal event", source))?;
            match input_action(event) {
                InputAction::Quit => break,
                InputAction::Refresh => {
                    last_scan = Instant::now() - options.refresh;
                    if !cost_indexing {
                        start_cost_index(&codex_home, &cost_tx);
                        cost_indexing = true;
                        if let Some(snapshot) = &mut snapshot
                            && latest_cost.is_none()
                        {
                            snapshot.cost_status = CostStatus::Indexing;
                        }
                    }
                    if !remote_scanning {
                        start_remote_scan(&codex_home, options.max_files, &remote_tx);
                        remote_scanning = true;
                    }
                }
                InputAction::None => {}
            }
        }
    }

    Ok(())
}

fn start_remote_scan(
    codex_home: &Path,
    max_files: usize,
    sender: &mpsc::Sender<RemoteScanMessage>,
) {
    let codex_home = codex_home.to_path_buf();
    let sender = sender.clone();
    thread::spawn(move || {
        let result = CodexScanner::default()
            .scan_without_cost(&codex_home, max_files)
            .map_err(|error| error.to_string());
        let _ = sender.send(RemoteScanMessage::Finished(result));
    });
}

fn start_cost_index(codex_home: &Path, sender: &mpsc::Sender<CostIndexMessage>) {
    let codex_home = codex_home.to_path_buf();
    let sender = sender.clone();
    thread::spawn(move || {
        let result = estimate_codex_cost(&codex_home).map_err(|error| error.to_string());
        let _ = sender.send(CostIndexMessage::Finished(result));
    });
}

fn drain_remote_results(
    receiver: &Receiver<RemoteScanMessage>,
    remote_scanning: &mut bool,
    latest_cost: Option<&CostEstimateResult>,
    cost_indexing: bool,
    snapshot: &mut Option<MeterSnapshot>,
    error: &mut Option<String>,
) {
    while let Ok(message) = receiver.try_recv() {
        match message {
            RemoteScanMessage::Finished(Ok(mut next)) => {
                *remote_scanning = false;
                apply_latest_cost(&mut next, latest_cost, cost_indexing);
                *snapshot = Some(next);
                *error = None;
            }
            RemoteScanMessage::Finished(Err(next_error)) => {
                *remote_scanning = false;
                if let Some(snapshot) = snapshot {
                    snapshot.data_warnings.push(next_error);
                } else {
                    *error = Some(next_error);
                }
            }
        }
    }
}

fn drain_cost_results(
    receiver: &Receiver<CostIndexMessage>,
    cost_indexing: &mut bool,
    latest_cost: &mut Option<CostEstimateResult>,
    snapshot: &mut Option<MeterSnapshot>,
) {
    while let Ok(message) = receiver.try_recv() {
        match message {
            CostIndexMessage::Finished(Ok(result)) => {
                *cost_indexing = false;
                *latest_cost = Some(result);
                if let Some(snapshot) = snapshot {
                    apply_latest_cost(snapshot, latest_cost.as_ref(), false);
                }
            }
            CostIndexMessage::Finished(Err(error)) => {
                *cost_indexing = false;
                if let Some(snapshot) = snapshot {
                    snapshot.data_warnings.push(error);
                    snapshot.cost_status = CostStatus::Ready;
                }
            }
        }
    }
}

fn apply_latest_cost(
    snapshot: &mut MeterSnapshot,
    latest_cost: Option<&CostEstimateResult>,
    cost_indexing: bool,
) {
    if let Some(cost) = latest_cost {
        snapshot.data_warnings.extend(cost.warnings.clone());
        if let Some(report) = cost.report.clone() {
            snapshot.profile.apply_cost_report(report);
        }
        snapshot.cost_status = CostStatus::Ready;
    } else if cost_indexing {
        snapshot.cost_status = CostStatus::Indexing;
    }
}

enum CostIndexMessage {
    Finished(Result<CostEstimateResult, String>),
}

enum RemoteScanMessage {
    Finished(Result<MeterSnapshot, String>),
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

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> AppResult<Self> {
        enable_raw_mode().map_err(|source| AppError::io("failed to enable raw mode", source))?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            Clear(ClearType::All)
        )
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
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

    use super::*;

    #[test]
    fn mouse_scroll_is_ignored_by_input_handler() {
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(input_action(event), InputAction::None);
    }
}
