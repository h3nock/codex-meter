use std::{env, ffi::OsString, path::PathBuf, time::Duration};

use crate::error::{AppError, AppResult};

pub const HELP: &str = "\
codex-meter

Usage:
  codex-meter [--codex-home PATH] [--refresh SECONDS] [--max-files N]
  codex-meter --once [--codex-home PATH] [--max-files N]
  codex-meter alias status
  codex-meter alias create [--bin-dir PATH]

Options:
  --codex-home PATH   Codex home directory. Defaults to CODEX_HOME or ~/.codex.
  --refresh SECONDS   TUI refresh interval. Defaults to 60 seconds.
  --max-files N       Session files used for live quota/model metadata. Defaults to 8.
  --once              Print one plain-text snapshot and exit.
  -h, --help          Show this help.
  -V, --version       Show the version.

The short command name `cm` is not installed by default. Use
`codex-meter alias create` to create it only when it does not collide.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Dashboard(DashboardOptions),
    Alias(AliasCommand),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardOptions {
    pub codex_home: Option<PathBuf>,
    pub refresh: Duration,
    pub max_files: usize,
    pub once: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasCommand {
    Status,
    Create { bin_dir: Option<PathBuf> },
}

impl Default for DashboardOptions {
    fn default() -> Self {
        Self {
            codex_home: None,
            refresh: Duration::from_secs(60),
            max_files: 8,
            once: false,
        }
    }
}

impl DashboardOptions {
    pub fn resolve_codex_home(&self) -> AppResult<PathBuf> {
        if let Some(path) = &self.codex_home {
            return Ok(path.clone());
        }

        if let Some(path) = env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(path));
        }

        let home = env::var_os("HOME").ok_or_else(|| {
            AppError::Argument("HOME is not set; pass --codex-home explicitly".to_string())
        })?;
        Ok(PathBuf::from(home).join(".codex"))
    }
}

pub fn parse_args<I, T>(args: I) -> AppResult<Command>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();

    if args.is_empty() {
        return Ok(Command::Dashboard(DashboardOptions::default()));
    }

    let first = args[0].to_string_lossy();
    match first.as_ref() {
        "-h" | "--help" => return Ok(Command::Help),
        "-V" | "--version" => return Ok(Command::Version),
        "alias" => return parse_alias(&args[1..]),
        _ => {}
    }

    parse_dashboard(&args)
}

fn parse_dashboard(args: &[OsString]) -> AppResult<Command> {
    let mut options = DashboardOptions::default();
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].to_string_lossy();
        match arg.as_ref() {
            "--codex-home" => {
                index += 1;
                options.codex_home = Some(next_path(args, index, "--codex-home")?);
            }
            "--refresh" => {
                index += 1;
                let seconds = next_string(args, index, "--refresh")?;
                options.refresh = Duration::from_secs(parse_positive_u64(&seconds, "--refresh")?);
            }
            "--max-files" => {
                index += 1;
                let count = next_string(args, index, "--max-files")?;
                options.max_files = parse_positive_usize(&count, "--max-files")?;
            }
            "--once" => options.once = true,
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            value => {
                return Err(AppError::Argument(format!(
                    "unknown argument `{value}`; run `codex-meter --help`"
                )));
            }
        }
        index += 1;
    }

    Ok(Command::Dashboard(options))
}

fn parse_alias(args: &[OsString]) -> AppResult<Command> {
    let Some(action) = args.first() else {
        return Err(AppError::Argument(
            "missing alias action; use `codex-meter alias status` or `codex-meter alias create`"
                .to_string(),
        ));
    };

    match action.to_string_lossy().as_ref() {
        "status" => {
            if args.len() != 1 {
                return Err(AppError::Argument(
                    "`codex-meter alias status` does not accept extra arguments".to_string(),
                ));
            }
            Ok(Command::Alias(AliasCommand::Status))
        }
        "create" => {
            let mut bin_dir = None;
            let mut index = 1;
            while index < args.len() {
                match args[index].to_string_lossy().as_ref() {
                    "--bin-dir" => {
                        index += 1;
                        bin_dir = Some(next_path(args, index, "--bin-dir")?);
                    }
                    value => {
                        return Err(AppError::Argument(format!(
                            "unknown alias create argument `{value}`"
                        )));
                    }
                }
                index += 1;
            }
            Ok(Command::Alias(AliasCommand::Create { bin_dir }))
        }
        value => Err(AppError::Argument(format!(
            "unknown alias action `{value}`; use status or create"
        ))),
    }
}

fn next_path(args: &[OsString], index: usize, flag: &str) -> AppResult<PathBuf> {
    args.get(index)
        .map(PathBuf::from)
        .ok_or_else(|| AppError::Argument(format!("{flag} requires a path")))
}

fn next_string(args: &[OsString], index: usize, flag: &str) -> AppResult<String> {
    args.get(index)
        .map(|value| value.to_string_lossy().to_string())
        .ok_or_else(|| AppError::Argument(format!("{flag} requires a value")))
}

fn parse_positive_u64(value: &str, flag: &str) -> AppResult<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| AppError::Argument(format!("{flag} must be a positive integer")))?;
    if parsed == 0 {
        return Err(AppError::Argument(format!(
            "{flag} must be greater than zero"
        )));
    }
    Ok(parsed)
}

fn parse_positive_usize(value: &str, flag: &str) -> AppResult<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| AppError::Argument(format!("{flag} must be a positive integer")))?;
    if parsed == 0 {
        return Err(AppError::Argument(format!(
            "{flag} must be greater than zero"
        )));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dashboard_options() {
        let parsed = parse_args([
            "--once",
            "--codex-home",
            "/tmp/codex",
            "--refresh",
            "5",
            "--max-files",
            "10",
        ])
        .expect("valid args");

        assert_eq!(
            parsed,
            Command::Dashboard(DashboardOptions {
                codex_home: Some(PathBuf::from("/tmp/codex")),
                refresh: Duration::from_secs(5),
                max_files: 10,
                once: true,
            })
        );
    }

    #[test]
    fn rejects_zero_refresh() {
        let error = parse_args(["--refresh", "0"]).expect_err("zero refresh is invalid");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn default_refresh_is_one_minute() {
        let parsed = parse_args(std::iter::empty::<&str>()).expect("valid default args");
        let Command::Dashboard(options) = parsed else {
            panic!("expected dashboard command");
        };

        assert_eq!(options.refresh, Duration::from_secs(60));
    }

    #[test]
    fn parses_alias_create_bin_dir() {
        let parsed = parse_args(["alias", "create", "--bin-dir", "/tmp/bin"]).expect("valid args");
        assert_eq!(
            parsed,
            Command::Alias(AliasCommand::Create {
                bin_dir: Some(PathBuf::from("/tmp/bin")),
            })
        );
    }
}
