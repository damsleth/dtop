# dtop
*damsleths top - yet another process monitor*

It aggregates process CPU and RSS memory by executable name and now includes:
- Workspace split (`dtop`, `dtop-core`, `dtop-collector`, `dtop-ui`)
- Pluggable collectors (`mac-ps` and feature-gated `linux-procfs`)
- System overview header (CPU, MEM, SWAP, load, uptime)
- Process and network modes (`n`)
- Disk throughput columns (`i`) and delta/churn mode (`v`)
- Background sampler thread with timing telemetry
- Safer process actions (signal selection, graceful-then-force, optional typed confirmation)
- Snapshot export to `artifacts/` (`s` or `--snapshot`)
- Configurable keybindings and defaults via `~/.config/dtop/config.toml`

## Build

```bash
cargo build
```

## Run

```bash
cargo run
```

Collector backend override:

```bash
cargo run -- --collector mac-ps
```

Linux procfs collector (feature-gated):

```bash
cargo run -p dtop --features dtop-collector/linux-procfs -- --collector linux-procfs
```

Non-interactive snapshot:

```bash
cargo run -- --snapshot
```

## Config

Config file path:

```bash
~/.config/dtop/config.toml
```

Supported fields:
- `poll_interval_ms = 1000`
- `default_sort = "cpu"` (`cpu`, `mem`, `name`, `disk_read`, `disk_write`, `cpu_delta`, `mem_delta`)
- `require_typed_confirmation = true|false`
- `theme = "name"`
- `[visible_columns]` with `disk = true|false` and `graphs = true|false`
- `[keymap]` action override map (for example `sort_cpu = "c"`)

## Controls

- `q`: quit
- `?`: toggle help
- `+` / `-`: adjust polling interval (100ms .. 10000ms)
- `x`: toggle lazy median mode
- `g`: toggle graph columns
- `n`: toggle process/network mode
- `v`: toggle delta mode
- `i`: toggle disk columns
- `c` / `m` / `p`: sort CPU-or-total / MEM-or-RX / name
- `r` / `w`: sort disk read (or TX in network mode) / disk write
- `Ctrl+r`: reverse sort direction
- `↑` / `↓` / `PageUp` / `PageDown`: move selection
- `f`: search/filter (process mode)
- `k`: open process action modal
- `s`: save snapshot (`.json` + `.md`)
- kill modal: `1` = `SIGTERM`, `2` = `SIGINT`, `3` = `SIGKILL`, `g` toggles graceful flow

## Notes

- Default collector uses `ps`/`netstat`/`vm_stat`/`sysctl` on macOS.
- `linux-procfs` backend is available when built with `dtop-collector/linux-procfs`.
- Process action signals require sufficient permissions for target PIDs.
