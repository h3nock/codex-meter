# codex-meter

A lightweight btop-style terminal meter for Codex.

`codex-meter` reads local Codex session JSONL files from `~/.codex` and renders the Codex quota windows, reset timing, latest turn size, and recent turn sizes in a terminal dashboard. It does not depend on CodexBar and does not spawn the Codex CLI during normal refreshes.

Large Codex session logs can be gigabytes. To stay responsive, the default scan reads the 8 most recent session files and only a bounded tail window from large files.

## Install

From this repository:

```sh
cargo install --path .
```

Run the dashboard:

```sh
codex-meter
```

Print one snapshot and exit:

```sh
codex-meter --once
```

Use a custom Codex home:

```sh
codex-meter --codex-home ~/.codex --max-files 8 --refresh 2
```

## Short alias

The canonical command is `codex-meter`.

The shorter `cm` command is opt-in because short executable names collide with third-party tools. Check the current PATH:

```sh
codex-meter alias status
```

Install `cm` only when it is available:

```sh
codex-meter alias install
```

The installer refuses to overwrite another command. By default it creates the alias next to the running `codex-meter` executable. To place it somewhere else:

```sh
codex-meter alias install --bin-dir ~/.local/bin
```

## What It Reads

`codex-meter` scans recent files under:

- `~/.codex/sessions`
- `~/.codex/archived_sessions` for counts only

It parses metadata fields such as token usage, rate limits, model, provider, and timestamps. It does not parse or display prompt/response message text.

The dashboard presents Codex rate limits as user-facing windows:

- `5h session left` for the short rolling Codex window
- `weekly left` for remaining 7-day Codex quota

## Development

```sh
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build --release
```

## License

MIT
