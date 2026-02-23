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

## Controls

- `q`: quit
- `?` or `esc`: toggle help
- `+` / `-`: adjust polling interval (100ms .. 10000ms)
- `x`: toggle lazy average mode
- `,` / `.`: decrease/increase lazy sample window (1..50)
- `c` / `m` / `p`: sort by CPU / memory / process name
- `←` / `→`: cycle sort column
- `r`: reverse sort direction
- `↑` / `↓`: move selection
- `PageUp` / `PageDown`: page selection
- `enter`: toggle extended PID details for selected row
- `f`: filter/search mode
- `k`: open kill confirmation for selected process name

## Notes

- macOS-only behavior is expected because it relies on `ps -caxo ...` output format.
- Sending `SIGKILL` may require sufficient permissions for target PIDs - run with `sudo` for max comfort.
