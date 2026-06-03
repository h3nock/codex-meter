mod alias;
mod cli;
mod codex;
mod error;
mod format;
mod ui;

use std::process;

use cli::{AliasCommand, Command};
use codex::scan_codex_home;
use error::{AppError, AppResult};

fn main() {
    if let Err(error) = run() {
        eprintln!("codex-meter: {error}");
        process::exit(1);
    }
}

fn run() -> AppResult<()> {
    match cli::parse_args(std::env::args().skip(1))? {
        Command::Dashboard(options) => {
            let codex_home = options.resolve_codex_home()?;

            if options.once {
                let snapshot = scan_codex_home(&codex_home, options.max_files)?;
                println!("{}", format::plain_summary(&snapshot));
                return Ok(());
            }

            ui::run(codex_home, options)
        }
        Command::Alias(AliasCommand::Status) => alias::print_status(),
        Command::Alias(AliasCommand::Install { bin_dir }) => alias::install(bin_dir),
        Command::Help => {
            println!("{}", cli::HELP);
            Ok(())
        }
        Command::Version => {
            println!("codex-meter {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

impl From<AppError> for process::ExitCode {
    fn from(_: AppError) -> Self {
        process::ExitCode::FAILURE
    }
}
