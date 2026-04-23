# stop

Smart top. A Rust TUI that diagnoses why your computer is slow and names the process responsible. Instead of raw numbers like `htop`, it ranks subsystems (CPU, memory, disk, network) by impact, surfaces culprits with plain-English explanations, and lets you act on the guilty process with a single keypress: `k` to kill, `s` to suspend, `n` to renice, and more, without leaving the UI.

Source: https://github.com/adamatan/smart-top

## Install

Homebrew (macOS, Linux):

```bash
brew install adamatan/tap/smart-top
```

Cargo:

```bash
cargo install smart-top
```

One-liner installer (downloads the prebuilt binary for your platform):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/adamatan/smart-top/releases/latest/download/smart-top-installer.sh | sh
```

The crate is `smart-top`, the binary is `stop`. Naming on crates.io was taken; the command you type stays short.

## Features

- Ranked **Blameboard** showing who's consuming the most CPU / memory / disk, weighted by how stressed each subsystem is
- Colored **System Load** bars with trend sparklines per subsystem
- **Per-process drill-in** (Enter) with full command line and every sampled PID
- **Persistent action bar**: suspend, resume, kill, force-kill, interrupt, renice, lsof, and more, one keystroke, no confirmation modal
- Deltas (↑ / ↓) highlight what changed since the last refresh
- Adaptive refresh: 250 ms when critical, slower when idle
- Cross-platform: Linux and macOS

## Usage

```
# Continuous watch mode (default, press q to quit)
stop

# Navigation:
#   ↑ / ↓     inspect a process (its details appear below the blameboard)
#   Enter     expand the selected process to fullscreen
#   q         quit

# Action keys (apply to every PID in the selected group, no confirmation):
#   s   pause process (SIGSTOP)       r   resume (SIGCONT)
#   k   soft kill (SIGTERM)           K   hard kill (SIGKILL, no cleanup)
#   i   send Ctrl-C (SIGINT)          H   reload config (SIGHUP)
#   1   SIGUSR1 (app-defined)         2   SIGUSR2 (app-defined)
#   n   lower priority (renice +10)
#   l   list open files (lsof to /tmp file)
#   m   open Activity Monitor          (macOS)
#   p   free cached RAM (purge)        (macOS)

# One-shot snapshot (press any key to exit)
stop --once

# Faster refresh
stop --interval 1

# JSON output (no UI, just metrics + report)
stop --json
```

Acting on a process you don't own (kill, suspend, renice) requires the usual OS permissions. Run with `sudo` for system processes.

## Build from source

```bash
git clone https://github.com/adamatan/smart-top.git
cd smart-top
cargo build --release
# binary at target/release/stop
```

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `-1, --once` | off | One-shot snapshot instead of continuous refresh |
| `-i, --interval N` | 2 | Refresh interval in seconds |
| `-n, --top N` | 5 | Processes shown per category |
| `--json` | off | Output raw metrics + diagnosis as JSON |
| `--no-color` | off | Disable ANSI color |

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
