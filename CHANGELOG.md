# Changelog

## 0.1.0 - 2026-04-19

Initial public release.

- Ranked Blameboard: processes sorted by impact on the most-stressed subsystem (CPU, memory, disk, network).
- Colored System Load bars with per-subsystem trend sparklines.
- Per-process drill-in with full command line and every sampled PID.
- Persistent action bar: `s` suspend, `r` resume, `k` SIGTERM, `K` SIGKILL, `i` SIGINT, `H` SIGHUP, `1`/`2` SIGUSR1/2, `n` renice +10, `l` lsof, `m` Activity Monitor (macOS), `p` purge cached RAM (macOS).
- Live permission probe per action row via `kill(pid, 0)`.
- Plain-language action banners ("Paused" rather than "SIGSTOP sent").
- Deltas (↑ / ↓) mark what changed since the last refresh.
- Adaptive refresh: 250 ms when critical, slower when idle.
- `--once` one-shot snapshot, `--interval`, `--top`, `--json`, `--no-color` flags.
- Cross-platform: Linux and macOS.
