# QA Report — dtop

Thorough code quality review of the two main crates: `dtop` (~2050 LoC) and `dtop-collector` (~1020 LoC).

---

## High Severity

### 1. `thread::sleep` on UI thread during kill — `dtop/src/main.rs:585–592`

`handle_kill_prompt_input` calls `thread::sleep(Duration::from_millis(300))` per PID when using graceful-then-force mode. For a process group with 10 PIDs, the UI freezes for 3 seconds. Kill should be dispatched asynchronously or batched.

### 2. `try_send` Full fallback blocks sampler thread — `dtop/src/main.rs:1962–1968`

When the `sync_channel(1)` is full, the code falls back to a blocking `send`, defeating the non-blocking design. If the UI stalls, the sampler thread stalls with it. The `Full` branch should drop the packet (or drain-and-replace), not block.

### 3. Duplicate bounded-deque logic — `dtop/src/main.rs:941–950`

`shrink_deque` is called, then `push_back`, then a `while` loop trims again — doing the same work twice. `push_bounded_sample` (line 1170) already exists and does this correctly. Replace the three-step pattern with two calls to `push_bounded_sample`.

---

## Medium Severity

### 4. God object: `App` has 40 fields — `dtop/src/main.rs:285–342`

13 `Option<f64>`/`Option<u64>` system-metric scalars, 3 history VecDeques, kill-prompt state, UI transient state, and persistent process-tracking maps are all mixed into one flat struct. Extract sub-structs:
- `SystemView` for the 13 system metrics
- `SystemHistory` for the VecDeques + previous totals
- `UiTransientState` for action_audit_line, last_snapshot_path, status_error, timing fields
- `KillPromptState` for kill_prompt + kill_prompt_error

### 5. Positional 6-tuple `GroupedProcess` — `dtop/src/main.rs:344`

```rust
type GroupedProcess = (f64, f64, usize, Vec<i32>, f64, f64);
```

Accessed by numeric index (`entry.0` through `entry.5`) throughout the grouping logic. Replace with a named struct with fields `cpu`, `mem_mb`, `count`, `pids`, `disk_read_bps`, `disk_write_bps`.

### 6. Network sort shares process descending flag — `dtop/src/main.rs:843`

`apply_interface_counters` uses `self.sort_state.descending` — the process view's flag. Reversing process sort also reverses network sort. Needs a separate `network_sort_descending` or a per-view sort state.

### 7. Hand-rolled JSON serialization (154 lines) — `dtop/src/main.rs:976–1129`

Manual `push_str` JSON building with duplicated trailing-comma logic for process rows (line 1046) and network rows (line 1063). Fragile and hard to extend — adding a field means finding the right `push_str` sites. Consider adding `serde_json` as an optional dependency, or at minimum extract the comma logic into a helper.

### 8. Key dispatch relies on `Debug` format stability — `dtop/src/main.rs:1932`

`key_to_string` uses `format!("{:?}-{:?}", modifiers, code)` to build hashmap keys. `Debug` output of crossterm types is not guaranteed stable across versions. A hand-written serializer that matches on actual variants would be safer.

### 9. Collection errors silently discarded — `dtop/src/main.rs:1948–1950`

`collector.collect_processes().unwrap_or_default()` and similar calls drop all errors. The user sees an empty table with no indication that collection failed. Add an `Option<String>` error field to `App` or `SamplerPacket` so the status bar can surface collection failures.

### 10. `netstat` spawned twice per poll cycle — `dtop-collector/src/lib.rs:118+137, 416–424`

`collect_network_totals` and `collect_network_interfaces` both spawn `netstat -ibn` and parse the same output. Share a single invocation.

### 11. `ps` spawned twice per poll cycle — `dtop-collector/src/lib.rs:93+249`

`collect_processes` runs `ps -caxo ...` for per-process data, then `collect_cpu_total_pct` (called from `collect_system_stats`) runs `ps -A -o %cpu=` separately. Total CPU can be computed by summing `ProcessRecord.cpu` from the already-collected processes.

### 12. `/proc/stat` read twice with inconsistent deltas (Linux) — `dtop-collector/src/lib.rs:566+648`

`collect_linux_procfs_processes` and `collect_linux_system_stats` each call `read_linux_total_and_idle_jiffies()`. The two reads see different snapshots, making per-process and total CPU deltas inconsistent. Read once per tick and pass down.

### 13. `parse_linux_rss_mb` — `?` in loop exits function, not iteration — `dtop-collector/src/lib.rs:745–752`

```rust
for line in status.lines() {
    let raw = line.strip_prefix("VmRSS:")?.trim();  // ? returns None from function
    ...
}
```

If the first line doesn't start with `VmRSS:`, the function returns `None` immediately — it never reaches subsequent lines. Works by accident because `VmRSS:` appears early in `/proc/{pid}/status`, but structurally broken. Use `if let Some(raw) = line.strip_prefix(...)` instead.

---

## Low Severity

### 14. `dtop-collector` is a single 1020-line file — `dtop-collector/src/lib.rs`

macOS collector, Linux collector, shared types, trait definition, and all parsers in one file. Natural split: `lib.rs` (trait + types + selection), `mac.rs`, `linux.rs`.

### 15. Dead code: `TreeRow` and `DetailEntry` — `dtop-core/src/lib.rs:18–45`

Defined in `dtop-core` but never imported or used anywhere in the workspace.

### 16. Dead code: `parse_linux_stat_ppid` — `dtop-collector/src/lib.rs:716–718`

Never called. `parse_linux_stat` is used directly. Masked by feature flags on default macOS build.

### 17. No-op rebinding — `dtop/src/main.rs:1943`

`let collector = collector;` — does nothing. Remove.

### 18. `theme` config parsed but never applied — `dtop/src/main.rs:262, 1603`

`theme` is read from config and shown in the status bar but never affects rendering. Either implement themes or remove the dead config path so users aren't misled.

### 19. Rendering lives in the binary, not `dtop-ui` — `dtop/src/main.rs`

`draw_process_table`, `draw_network_table`, `render_system_overview_header`, `render_status_line` are all in `main.rs`. The `dtop-ui` crate only has modals and helpers. The stated separation of concerns isn't enforced.

### 20. `draw_ui` takes `&mut App` but never mutates it — `dtop/src/main.rs:2059`

`draw_ui` should take `&App`. The `&mut` requirement is leftover/unnecessary.

### 21. Unnecessary `.collect::<Vec<usize>>()` — `dtop/src/main.rs:440–443`

`.extend((0..self.network_rows.len()).collect::<Vec<usize>>())` allocates a temporary Vec. `.extend(0..self.network_rows.len())` is equivalent and allocation-free.

### 22. `sort_processes` takes `SortState` by value — `dtop/src/main.rs:1132`

Only reads two fields. Should take `&SortState`.

### 23. Column indices recomputed per row in netstat parser — `dtop-collector/src/lib.rs:509–514`

Header column positions for `Name`, `Ibytes`, `Obytes` are looked up with `.position()` on every data row. Compute once after parsing the header line.

### 24. Magic field indices without comments — `dtop-collector/src/lib.rs:730–742, 875–878`

Linux `/proc/stat` fields 1/11/12 and `/proc/diskstats` fields 5/9 used by raw index. Add constants or comments referencing the kernel docs.

### 25. Backend name strings duplicated 3 times — `dtop-collector/src/lib.rs:214–223`

`"mac-ps"` and `"linux-procfs"` repeated in `as_str`, `from_name`, and `available_backends`. Use `as_str` in the other two locations.

### 26. Error handling inconsistency between backends — `dtop-collector/src/lib.rs:139 vs 173`

macOS `collect_network_interfaces` returns `Ok(Vec::new())` on failure; Linux propagates the error. Callers can't rely on `Err` uniformly.

### 27. Hand-rolled TOML parser (125 lines) — `dtop/src/main.rs:1671–1796`

No support for inline tables, arrays, multi-line strings, dotted keys, or escape sequences. Fine for the current narrow config schema but will break if it grows. The `keymap` section parsing (lines 1778–1790) has a dead `parse_string_value(key)` check that has no effect.

### 28. Shared Linux collector state double-write — `dtop-collector/src/lib.rs:158, 638, 671`

Both `collect_linux_procfs_processes` and `collect_linux_system_stats` update `last_total_jiffies` in the shared `Mutex<LinuxCollectorState>`. If called sequentially on the same tick, the second overwrites the first's delta reference.

---

## Recommended Refactoring Priority

1. **Extract sub-structs from `App`** (#4, #5) — biggest readability win
2. **Fix sampler channel semantics** (#2) — correctness bug
3. **Eliminate duplicate subprocess spawns** (#10, #11, #12) — ~3 fewer processes per tick
4. **Fix `parse_linux_rss_mb`** (#13) — latent correctness bug
5. **Move rendering to `dtop-ui`** (#19) — cuts `main.rs` by ~500 lines
6. **Replace hand-rolled JSON** (#7) — maintenance hazard
7. **Remove dead code** (#15, #16, #17) — quick cleanup
8. **Split `dtop-collector` into modules** (#14) — prep for multi-platform growth
