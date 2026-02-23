# dtop
*damsleths top - yet another process monitor - macOS only for now*  

It aggregates process CPU and RSS memory by executable name (matching `ps -c`) and supports:
- Sort by CPU / memory / name
- Reverse sort direction
- Lazy median averaging window (`x̄` mode)
- Live filtering
- Extended per-PID details
- Kill confirmation (`SIGKILL`) for selected process group

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

## Controls

- `q`: quit
- `?` or `esc`: toggle help
- `+` / `-`: adjust polling interval (100ms .. 10000ms)
- `x`: toggle lazy average mode
- `g`: cycle graph mode (`Off` -> `List` -> `Detail` -> `Both`)
- `1` / `2` / `3`: toggle CPU / MEM / NET panels
- `,` / `.`: decrease/increase lazy sample window (1..50)
- `c` / `m` / `p`: sort by CPU / memory / process name
- `←` / `→`: cycle sort column
- `r`: reverse sort direction
- `↑` / `↓`: move selection
- `PageUp` / `PageDown`: page selection
- `space`: expand/collapse selected tree node
- `enter`: toggle/pin extended info for selected row/tree (multiple panels supported)
- `f`: filter/search mode
- `k`: open kill confirmation for selected process name

## Notes

- Default collector uses `ps -caxo ...` output and works on macOS.
- `linux-procfs` backend is available when built with `dtop-collector/linux-procfs`.
- Sending `SIGKILL` may require sufficient permissions for target PIDs - run with `sudo` for max comfort.
