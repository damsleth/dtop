use std::cmp::max;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dtop_collector::{
    Collector, CollectorBackend, InterfaceCounter, ProcessRecord, SystemStats, build_collector,
};
use dtop_core::{
    FilterState, MetricHistory, ProcEntry, ProcSnapshotEntry, SortBy, SortState, median,
};
use dtop_ui::{
    HelpModalData, KillModalData, NetworkRowData, NetworkSortBy, NetworkTableData,
    ProcessRowData, ProcessTableData, StatusLineData, SystemHeaderData,
    draw_network_table, draw_process_table, format_bytes_per_second,
    is_ctrl_c_quit, is_search_input_char, render_help_modal, render_kill_modal,
    render_status_line, render_system_overview_header,
};
use libc::{SIGINT, SIGKILL, SIGTERM, kill};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphMode {
    Off,
    List,
}

impl GraphMode {
    fn cycle(self) -> Self {
        match self {
            GraphMode::Off => GraphMode::List,
            GraphMode::List => GraphMode::Off,
        }
    }

    fn show_list(self) -> bool {
        matches!(self, GraphMode::List)
    }

    fn label(self) -> &'static str {
        match self {
            GraphMode::Off => "Off",
            GraphMode::List => "List",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    Process,
    Network,
}

impl ViewMode {
    fn toggle(self) -> Self {
        match self {
            ViewMode::Process => ViewMode::Network,
            ViewMode::Network => ViewMode::Process,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ViewMode::Process => "Process",
            ViewMode::Network => "Network",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignalChoice {
    Term,
    Int,
    Kill,
}

impl SignalChoice {
    fn as_raw(self) -> i32 {
        match self {
            SignalChoice::Term => SIGTERM,
            SignalChoice::Int => SIGINT,
            SignalChoice::Kill => SIGKILL,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SignalChoice::Term => "SIGTERM",
            SignalChoice::Int => "SIGINT",
            SignalChoice::Kill => "SIGKILL",
        }
    }
}

#[derive(Clone, Debug)]
struct NetworkRow {
    name: String,
    rx_bps: f64,
    tx_bps: f64,
    total_bps: f64,
}

#[derive(Clone, Debug)]
struct KillPrompt {
    command: String,
    pids: Vec<i32>,
    signal: SignalChoice,
    graceful_then_force: bool,
    typed_confirmation: String,
    debug_summary: Option<String>,
}

#[derive(Clone, Debug)]
struct RateTotalsSample {
    read_total: u64,
    write_total: u64,
    captured_at: Instant,
}

#[derive(Clone, Debug)]
struct NetTotalsSample {
    rx_total: u64,
    tx_total: u64,
    captured_at: Instant,
}

#[derive(Clone, Debug)]
struct SamplerPacket {
    captured_at: Instant,
    processes: Vec<ProcessRecord>,
    system: Option<SystemStats>,
    interfaces: Vec<InterfaceCounter>,
    poll_ms: f64,
    parse_ms: f64,
    collection_error: Option<String>,
}

trait ProcessController: Send {
    fn send_signal(&self, pid: i32, signal: i32) -> io::Result<()>;
    fn is_alive(&self, pid: i32) -> bool;
}

struct LibcProcessController;

impl ProcessController for LibcProcessController {
    fn send_signal(&self, pid: i32, signal: i32) -> io::Result<()> {
        let rc = unsafe { kill(pid, signal) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn is_alive(&self, pid: i32) -> bool {
        let rc = unsafe { kill(pid, 0) };
        rc == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Action {
    Quit,
    ToggleHelp,
    PollFaster,
    PollSlower,
    ToggleLazy,
    ToggleGraph,
    ToggleViewMode,
    ToggleDeltaMode,
    ToggleDiskColumns,
    SortCpu,
    SortMem,
    SortName,
    SortDiskRead,
    SortDiskWrite,
    ReverseSort,
    MoveDown,
    MoveUp,
    PageDown,
    PageUp,
    Search,
    Kill,
    ToggleKillDebug,
    Snapshot,
}

impl Action {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "quit" => Some(Action::Quit),
            "help" => Some(Action::ToggleHelp),
            "poll_faster" => Some(Action::PollFaster),
            "poll_slower" => Some(Action::PollSlower),
            "toggle_lazy" => Some(Action::ToggleLazy),
            "toggle_graph" => Some(Action::ToggleGraph),
            "toggle_view_mode" => Some(Action::ToggleViewMode),
            "toggle_delta_mode" => Some(Action::ToggleDeltaMode),
            "toggle_disk_columns" => Some(Action::ToggleDiskColumns),
            "sort_cpu" => Some(Action::SortCpu),
            "sort_mem" => Some(Action::SortMem),
            "sort_name" => Some(Action::SortName),
            "sort_disk_read" => Some(Action::SortDiskRead),
            "sort_disk_write" => Some(Action::SortDiskWrite),
            "reverse_sort" => Some(Action::ReverseSort),
            "move_down" => Some(Action::MoveDown),
            "move_up" => Some(Action::MoveUp),
            "page_down" => Some(Action::PageDown),
            "page_up" => Some(Action::PageUp),
            "search" => Some(Action::Search),
            "kill" => Some(Action::Kill),
            "toggle_kill_debug" => Some(Action::ToggleKillDebug),
            "snapshot" => Some(Action::Snapshot),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct KeyRegistry {
    by_key: HashMap<String, Action>,
    help_rows: Vec<(String, String)>,
}

impl KeyRegistry {
    fn resolve(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        self.by_key.get(&key_to_string(code, modifiers)).copied()
    }
}

#[derive(Debug, Clone)]
struct AppConfig {
    poll_interval_ms: u64,
    default_sort: SortBy,
    show_disk_columns: bool,
    graph_mode: GraphMode,
    require_typed_confirmation: bool,
    key_overrides: HashMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            default_sort: SortBy::Cpu,
            show_disk_columns: false,
            graph_mode: GraphMode::Off,
            require_typed_confirmation: false,
            key_overrides: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CliArgs {
    collector_override: Option<String>,
    snapshot_mode: bool,
}

#[derive(Clone, Debug, Default)]
struct SystemView {
    cpu_total_pct: Option<f64>,
    mem_used_mb: Option<f64>,
    mem_total_mb: Option<f64>,
    swap_used_mb: Option<f64>,
    swap_total_mb: Option<f64>,
    load_avg_1: Option<f64>,
    load_avg_5: Option<f64>,
    load_avg_15: Option<f64>,
    uptime_seconds: Option<u64>,
    net_rx_bps: Option<f64>,
    net_tx_bps: Option<f64>,
    disk_read_bps: Option<f64>,
    disk_write_bps: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct SystemHistory {
    cpu: VecDeque<f64>,
    mem: VecDeque<f64>,
    net: VecDeque<f64>,
    previous_net_totals: Option<NetTotalsSample>,
    previous_disk_totals: Option<RateTotalsSample>,
}

struct App {
    poll_interval_ms: u64,
    poll_interval_shared: Arc<AtomicU64>,
    sort_state: SortState,
    network_sort_by: NetworkSortBy,
    network_sort_descending: bool,
    view_mode: ViewMode,
    lazy_mode: bool,
    delta_mode: bool,
    graph_mode: GraphMode,
    show_disk_columns: bool,
    lazy_window: usize,
    show_help: bool,
    debug_kill: bool,
    require_typed_confirmation: bool,
    collector_backend: CollectorBackend,
    key_registry: KeyRegistry,
    current_processes: Vec<ProcEntry>,
    filtered_indices: Vec<usize>,
    selected_index: Option<usize>,
    filter_state: FilterState,
    process_snapshot: Vec<ProcSnapshotEntry>,
    metric_history: HashMap<String, MetricHistory>,
    pids_by_command: HashMap<String, Vec<i32>>,
    previous_command_totals: HashMap<String, (f64, f64)>,
    recent_exited_commands: Vec<String>,
    previous_pid_disk_totals: HashMap<i32, (u64, u64)>,
    previous_sample_captured_at: Option<Instant>,
    network_rows: Vec<NetworkRow>,
    previous_iface_totals: HashMap<String, (u64, u64, Instant)>,
    system: SystemView,
    system_history: SystemHistory,
    kill_prompt: Option<KillPrompt>,
    kill_prompt_error: Option<String>,
    process_controller: Box<dyn ProcessController>,
    action_audit_line: Option<String>,
    last_poll_ms: f64,
    last_parse_ms: f64,
    last_render_ms: f64,
    last_snapshot_path: Option<String>,
    status_error: Option<String>,
}

struct GroupedProcess {
    cpu: f64,
    mem_mb: f64,
    count: usize,
    pids: Vec<i32>,
    disk_read_bps: f64,
    disk_write_bps: f64,
}

impl App {
    fn new(
        cfg: AppConfig,
        backend: CollectorBackend,
        key_registry: KeyRegistry,
        process_controller: Box<dyn ProcessController>,
        poll_interval_shared: Arc<AtomicU64>,
    ) -> Self {
        let sort_state = SortState {
            by: cfg.default_sort,
            descending: true,
        };

        let this = Self {
            poll_interval_ms: cfg.poll_interval_ms,
            poll_interval_shared,
            sort_state,
            network_sort_by: NetworkSortBy::Total,
            network_sort_descending: true,
            view_mode: ViewMode::Process,
            lazy_mode: true,
            delta_mode: false,
            graph_mode: cfg.graph_mode,
            show_disk_columns: cfg.show_disk_columns,
            lazy_window: 20,
            show_help: false,
            debug_kill: false,
            require_typed_confirmation: cfg.require_typed_confirmation,
            collector_backend: backend,
            key_registry,
            current_processes: Vec::new(),
            filtered_indices: Vec::new(),
            selected_index: None,
            filter_state: FilterState::default(),
            process_snapshot: Vec::new(),
            metric_history: HashMap::new(),
            pids_by_command: HashMap::new(),
            previous_command_totals: HashMap::new(),
            recent_exited_commands: Vec::new(),
            previous_pid_disk_totals: HashMap::new(),
            previous_sample_captured_at: None,
            network_rows: Vec::new(),
            previous_iface_totals: HashMap::new(),
            system: SystemView::default(),
            system_history: SystemHistory::default(),
            kill_prompt: None,
            kill_prompt_error: None,
            process_controller,
            action_audit_line: None,
            last_poll_ms: 0.0,
            last_parse_ms: 0.0,
            last_render_ms: 0.0,
            last_snapshot_path: None,
            status_error: None,
        };
        this.poll_interval_shared
            .store(this.poll_interval_ms, Ordering::Relaxed);
        this
    }

    fn set_poll_interval(&mut self, value: u64) {
        self.poll_interval_ms = value.clamp(100, 10_000);
        self.poll_interval_shared
            .store(self.poll_interval_ms, Ordering::Relaxed);
    }

    fn update_filter_and_selection(&mut self) {
        self.filtered_indices.clear();
        let len = match self.view_mode {
            ViewMode::Process => {
                let query = self.filter_state.search_query.to_lowercase();
                for (idx, row) in self.current_processes.iter().enumerate() {
                    if query.is_empty() || row.name.to_lowercase().contains(&query) {
                        self.filtered_indices.push(idx);
                    }
                }
                self.filtered_indices.len()
            }
            ViewMode::Network => {
                self.filtered_indices
                    .extend(0..self.network_rows.len());
                self.filtered_indices.len()
            }
        };

        if len == 0 {
            self.selected_index = None;
        } else if let Some(current) = self.selected_index {
            self.selected_index = Some(current.min(len - 1));
        } else {
            self.selected_index = Some(0);
        }
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize, page_mode: bool) {
        let len = self.filtered_indices.len();
        if len == 0 {
            return;
        }
        let step = if page_mode {
            max(1, visible_rows) as isize
        } else {
            delta.abs()
        };
        let direction = if delta >= 0 { 1 } else { -1 };
        let change = step * direction;
        let new_idx = if let Some(current) = self.selected_index {
            (current as isize + change).clamp(0, (len - 1) as isize) as usize
        } else if direction >= 0 {
            0
        } else {
            len - 1
        };
        self.selected_index = Some(new_idx);
    }

    fn selected_process(&self) -> Option<&ProcEntry> {
        let selected = self.selected_index?;
        let idx = *self.filtered_indices.get(selected)?;
        self.current_processes.get(idx)
    }

    fn get_pids_for_command(&self, command: &str) -> Vec<i32> {
        if let Some(pids) = self.pids_by_command.get(command) {
            return pids.clone();
        }
        self.process_snapshot
            .iter()
            .filter(|proc| proc.name == command)
            .map(|proc| proc.pid)
            .collect()
    }

    fn open_kill_prompt_for_selected(&mut self) {
        if self.view_mode != ViewMode::Process {
            return;
        }
        let Some(selected) = self.selected_process() else {
            return;
        };
        let command = selected.name.clone();
        let pids = self.get_pids_for_command(&command);
        let debug_summary = if self.debug_kill {
            Some(format!(
                "debug: selected='{}' resolved_pids={} require_typed={}",
                command,
                pids.len(),
                self.require_typed_confirmation
            ))
        } else {
            None
        };
        self.kill_prompt = Some(KillPrompt {
            command,
            pids: pids.clone(),
            signal: SignalChoice::Kill,
            graceful_then_force: false,
            typed_confirmation: String::new(),
            debug_summary,
        });
        self.kill_prompt_error = if pids.is_empty() {
            Some("No PIDs found".to_string())
        } else {
            None
        };
    }

    fn record_audit(&mut self, action: &str, result: &str) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.action_audit_line = Some(format!("{ts}: {action} -> {result}"));
    }

    fn handle_kill_prompt_input(&mut self, key: KeyCode) {
        let Some(prompt) = self.kill_prompt.as_mut() else {
            return;
        };

        match key {
            KeyCode::Esc => {
                self.kill_prompt = None;
                self.kill_prompt_error = None;
            }
            KeyCode::Char('1') => prompt.signal = SignalChoice::Term,
            KeyCode::Char('2') => prompt.signal = SignalChoice::Int,
            KeyCode::Char('3') => prompt.signal = SignalChoice::Kill,
            KeyCode::Char('g') | KeyCode::Char('G') => {
                prompt.graceful_then_force = !prompt.graceful_then_force;
            }
            KeyCode::Backspace => {
                if self.require_typed_confirmation {
                    prompt.typed_confirmation.pop();
                }
            }
            KeyCode::Char(c) => {
                if self.require_typed_confirmation && c.is_ascii_alphabetic() {
                    prompt.typed_confirmation.push(c.to_ascii_uppercase());
                }
            }
            KeyCode::Enter => {
                if prompt.pids.is_empty() {
                    self.kill_prompt_error = Some("No PIDs found".to_string());
                    return;
                }
                if self.require_typed_confirmation && prompt.typed_confirmation != "KILL" {
                    self.kill_prompt_error = Some("Type KILL to confirm".to_string());
                    return;
                }

                let signal = prompt.signal;
                let graceful_then_force = prompt.graceful_then_force;
                let pids = prompt.pids.clone();
                let command = prompt.command.clone();

                let mut errors = Vec::new();
                for pid in &pids {
                    if let Err(err) = self.process_controller.send_signal(*pid, signal.as_raw()) {
                        errors.push(err.to_string());
                    }
                }

                if graceful_then_force && signal != SignalChoice::Kill {
                    thread::sleep(Duration::from_millis(300));
                    for pid in &pids {
                        if self.process_controller.is_alive(*pid)
                            && let Err(err) =
                                self.process_controller.send_signal(*pid, SIGKILL)
                        {
                            errors.push(err.to_string());
                        }
                    }
                }

                if let Some(err) = errors.first() {
                    self.kill_prompt_error = Some(err.clone());
                    self.record_audit(
                        &format!("{} {}", signal.label(), command),
                        &format!("error: {err}"),
                    );
                } else {
                    self.kill_prompt = None;
                    self.kill_prompt_error = None;
                    self.record_audit(&format!("{} {}", signal.label(), command), "ok");
                }
            }
            _ => {}
        }
    }

    fn handle_search_input(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.filter_state.search_mode = false;
                self.filter_state.search_query.clear();
            }
            KeyCode::Enter => {
                self.filter_state.search_mode = false;
            }
            KeyCode::Backspace => {
                self.filter_state.search_query.pop();
            }
            KeyCode::Char(c) => {
                if is_search_input_char(c) {
                    self.filter_state.search_query.push(c);
                }
            }
            KeyCode::Down => self.move_selection(1, 1, false),
            KeyCode::Up => self.move_selection(-1, 1, false),
            _ => {}
        }
    }

    fn handle_action(&mut self, action: Action, visible_rows: usize) -> bool {
        match action {
            Action::Quit => return true,
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::PollFaster => self.set_poll_interval(self.poll_interval_ms.saturating_sub(100)),
            Action::PollSlower => self.set_poll_interval(self.poll_interval_ms + 100),
            Action::ToggleLazy => self.lazy_mode = !self.lazy_mode,
            Action::ToggleGraph => self.graph_mode = self.graph_mode.cycle(),
            Action::ToggleViewMode => {
                self.view_mode = self.view_mode.toggle();
                self.filter_state.search_mode = false;
                self.filter_state.search_query.clear();
            }
            Action::ToggleDeltaMode => self.delta_mode = !self.delta_mode,
            Action::ToggleDiskColumns => self.show_disk_columns = !self.show_disk_columns,
            Action::SortCpu => {
                if self.view_mode == ViewMode::Network {
                    self.network_sort_by = NetworkSortBy::Total;
                } else if self.delta_mode {
                    self.sort_state.by = SortBy::CpuDelta;
                } else {
                    self.sort_state.by = SortBy::Cpu;
                }
            }
            Action::SortMem => {
                if self.view_mode == ViewMode::Network {
                    self.network_sort_by = NetworkSortBy::Rx;
                } else if self.delta_mode {
                    self.sort_state.by = SortBy::MemDelta;
                } else {
                    self.sort_state.by = SortBy::Mem;
                }
            }
            Action::SortName => {
                if self.view_mode == ViewMode::Network {
                    self.network_sort_by = NetworkSortBy::Name;
                } else {
                    self.sort_state.by = SortBy::Name;
                }
            }
            Action::SortDiskRead => {
                if self.view_mode == ViewMode::Network {
                    self.network_sort_by = NetworkSortBy::Tx;
                } else {
                    self.sort_state.by = SortBy::DiskRead;
                }
            }
            Action::SortDiskWrite => {
                self.sort_state.by = SortBy::DiskWrite;
            }
            Action::ReverseSort => {
                if self.view_mode == ViewMode::Network {
                    self.network_sort_descending = !self.network_sort_descending;
                } else {
                    self.sort_state.descending = !self.sort_state.descending;
                }
            }
            Action::MoveDown => self.move_selection(1, visible_rows, false),
            Action::MoveUp => self.move_selection(-1, visible_rows, false),
            Action::PageDown => self.move_selection(1, visible_rows, true),
            Action::PageUp => self.move_selection(-1, visible_rows, true),
            Action::Search => {
                if self.view_mode == ViewMode::Process {
                    self.filter_state.search_mode = true;
                }
            }
            Action::Kill => self.open_kill_prompt_for_selected(),
            Action::ToggleKillDebug => self.debug_kill = !self.debug_kill,
            Action::Snapshot => match self.save_snapshot_document() {
                Ok(path) => {
                    self.last_snapshot_path = Some(path.clone());
                    self.record_audit("snapshot", &path);
                    self.status_error = None;
                }
                Err(err) => {
                    self.status_error = Some(err.to_string());
                    self.record_audit("snapshot", &format!("error: {err}"));
                }
            },
        }
        false
    }

    fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers, visible_rows: usize) -> bool {
        if self.kill_prompt.is_some() {
            self.handle_kill_prompt_input(key);
            return false;
        }

        if self.filter_state.search_mode {
            self.handle_search_input(key);
            return false;
        }

        if let Some(action) = self.key_registry.resolve(key, modifiers) {
            return self.handle_action(action, visible_rows);
        }

        false
    }

    fn apply_packet(&mut self, packet: SamplerPacket) {
        self.last_poll_ms = packet.poll_ms;
        self.last_parse_ms = packet.parse_ms;
        self.status_error = packet.collection_error;

        if let Some(system) = packet.system {
            self.apply_system_stats(packet.captured_at, system);
        }

        self.apply_interface_counters(packet.captured_at, &packet.interfaces);
        self.apply_process_snapshot(packet.captured_at, packet.processes);
        self.update_filter_and_selection();
    }

    fn apply_system_stats(&mut self, captured_at: Instant, stats: SystemStats) {
        self.system.cpu_total_pct = stats.cpu_total_pct;
        self.system.mem_used_mb = stats.mem_used_mb;
        self.system.mem_total_mb = stats.mem_total_mb;
        self.system.swap_used_mb = stats.swap_used_mb;
        self.system.swap_total_mb = stats.swap_total_mb;
        self.system.load_avg_1 = stats.load_avg_1;
        self.system.load_avg_5 = stats.load_avg_5;
        self.system.load_avg_15 = stats.load_avg_15;
        self.system.uptime_seconds = stats.uptime_seconds;

        if let Some(v) = stats.cpu_total_pct {
            push_bounded_sample(&mut self.system_history.cpu, v, self.lazy_window);
        }
        if let (Some(used), Some(total)) = (stats.mem_used_mb, stats.mem_total_mb)
            && total > 0.0
        {
            push_bounded_sample(
                &mut self.system_history.mem,
                (used / total) * 100.0,
                self.lazy_window,
            );
        }

        if let (Some(rx_total), Some(tx_total)) =
            (stats.net_rx_bytes_total, stats.net_tx_bytes_total)
        {
            if let Some(prev) = self.system_history.previous_net_totals.clone() {
                let elapsed = captured_at.duration_since(prev.captured_at).as_secs_f64();
                if elapsed > 0.0 {
                    let rx_bps = rx_total.saturating_sub(prev.rx_total) as f64 / elapsed;
                    let tx_bps = tx_total.saturating_sub(prev.tx_total) as f64 / elapsed;
                    self.system.net_rx_bps = Some(rx_bps);
                    self.system.net_tx_bps = Some(tx_bps);
                    push_bounded_sample(
                        &mut self.system_history.net,
                        rx_bps + tx_bps,
                        self.lazy_window,
                    );
                }
            }
            self.system_history.previous_net_totals = Some(NetTotalsSample {
                rx_total,
                tx_total,
                captured_at,
            });
        }

        if let (Some(read_total), Some(write_total)) =
            (stats.disk_read_bytes_total, stats.disk_write_bytes_total)
        {
            if let Some(prev) = self.system_history.previous_disk_totals.clone() {
                let elapsed = captured_at.duration_since(prev.captured_at).as_secs_f64();
                if elapsed > 0.0 {
                    self.system.disk_read_bps =
                        Some(read_total.saturating_sub(prev.read_total) as f64 / elapsed);
                    self.system.disk_write_bps =
                        Some(write_total.saturating_sub(prev.write_total) as f64 / elapsed);
                }
            }
            self.system_history.previous_disk_totals = Some(RateTotalsSample {
                read_total,
                write_total,
                captured_at,
            });
        }
    }

    fn apply_interface_counters(&mut self, captured_at: Instant, interfaces: &[InterfaceCounter]) {
        let mut rows = Vec::new();
        for iface in interfaces {
            let (mut rx_bps, mut tx_bps) = (0.0, 0.0);
            if let Some((prev_rx, prev_tx, prev_at)) =
                self.previous_iface_totals.get(&iface.name).copied()
            {
                let elapsed = captured_at.duration_since(prev_at).as_secs_f64();
                if elapsed > 0.0 {
                    rx_bps = iface.rx_bytes_total.saturating_sub(prev_rx) as f64 / elapsed;
                    tx_bps = iface.tx_bytes_total.saturating_sub(prev_tx) as f64 / elapsed;
                }
            }
            rows.push(NetworkRow {
                name: iface.name.clone(),
                rx_bps,
                tx_bps,
                total_bps: rx_bps + tx_bps,
            });
            self.previous_iface_totals.insert(
                iface.name.clone(),
                (iface.rx_bytes_total, iface.tx_bytes_total, captured_at),
            );
        }

        match self.network_sort_by {
            NetworkSortBy::Rx => rows.sort_by(|a, b| a.rx_bps.total_cmp(&b.rx_bps)),
            NetworkSortBy::Tx => rows.sort_by(|a, b| a.tx_bps.total_cmp(&b.tx_bps)),
            NetworkSortBy::Total => rows.sort_by(|a, b| a.total_bps.total_cmp(&b.total_bps)),
            NetworkSortBy::Name => rows.sort_by_key(|r| r.name.to_lowercase()),
        }
        if self.network_sort_descending {
            rows.reverse();
        }
        self.network_rows = rows;
    }

    fn apply_process_snapshot(&mut self, captured_at: Instant, rows: Vec<ProcessRecord>) {
        let elapsed = self
            .previous_sample_captured_at
            .map(|prev| captured_at.duration_since(prev).as_secs_f64())
            .filter(|v| *v > 0.0)
            .unwrap_or(0.0);
        self.previous_sample_captured_at = Some(captured_at);

        let mut grouped: HashMap<String, GroupedProcess> = HashMap::new();
        let mut snapshot = Vec::with_capacity(rows.len());
        let mut present_commands = HashSet::new();
        let mut next_pid_disk_totals = HashMap::new();

        for process in rows {
            present_commands.insert(process.name.clone());

            let mut disk_read_bps = 0.0;
            let mut disk_write_bps = 0.0;
            if elapsed > 0.0
                && let (Some(read_total), Some(write_total)) = (
                    process.disk_read_bytes_total,
                    process.disk_write_bytes_total,
                )
            {
                if let Some((prev_read, prev_write)) =
                    self.previous_pid_disk_totals.get(&process.pid)
                {
                    disk_read_bps = read_total.saturating_sub(*prev_read) as f64 / elapsed;
                    disk_write_bps = write_total.saturating_sub(*prev_write) as f64 / elapsed;
                }
                next_pid_disk_totals.insert(process.pid, (read_total, write_total));
            }

            let entry = grouped
                .entry(process.name.clone())
                .or_insert(GroupedProcess {
                    cpu: 0.0,
                    mem_mb: 0.0,
                    count: 0,
                    pids: Vec::new(),
                    disk_read_bps: 0.0,
                    disk_write_bps: 0.0,
                });
            entry.cpu += process.cpu;
            entry.mem_mb += process.mem_mb;
            entry.count += 1;
            entry.pids.push(process.pid);
            entry.disk_read_bps += disk_read_bps;
            entry.disk_write_bps += disk_write_bps;

            snapshot.push(ProcSnapshotEntry {
                pid: process.pid,
                ppid: process.ppid,
                name: process.name,
                cpu: process.cpu,
                mem_mb: process.mem_mb,
                disk_read_bytes_total: process.disk_read_bytes_total,
                disk_write_bytes_total: process.disk_write_bytes_total,
            });
        }

        self.previous_pid_disk_totals = next_pid_disk_totals;
        self.process_snapshot = snapshot;

        let mut next_previous = HashMap::new();
        let mut exited = Vec::new();

        for command in self.previous_command_totals.keys() {
            if !present_commands.contains(command) {
                exited.push(command.clone());
            }
        }
        exited.sort();
        self.recent_exited_commands = exited.into_iter().take(5).collect();

        let mut proc_list = Vec::new();
        let mut refreshed_pids = HashMap::new();

        for (name, group) in grouped {
            refreshed_pids.insert(name.clone(), group.pids);
            let (prev_cpu, prev_mem) = self
                .previous_command_totals
                .get(&name)
                .copied()
                .unwrap_or((0.0, 0.0));
            next_previous.insert(name.clone(), (group.cpu, group.mem_mb));

            let is_new = !self.previous_command_totals.contains_key(&name);
            let cpu_delta = group.cpu - prev_cpu;
            let mem_delta_mb = group.mem_mb - prev_mem;

            let history =
                self.metric_history
                    .entry(name.clone())
                    .or_insert_with(|| MetricHistory {
                        cpu: VecDeque::with_capacity(self.lazy_window),
                        mem: VecDeque::with_capacity(self.lazy_window),
                    });
            push_bounded_sample(&mut history.cpu, group.cpu, self.lazy_window);
            push_bounded_sample(&mut history.mem, group.mem_mb, self.lazy_window);

            proc_list.push(ProcEntry {
                name,
                cpu: group.cpu,
                mem_mb: group.mem_mb,
                disk_read_bps: group.disk_read_bps,
                disk_write_bps: group.disk_write_bps,
                cpu_delta,
                mem_delta_mb,
                is_new,
                count: group.count,
                cpu_lazy: median(&history.cpu).unwrap_or(group.cpu),
                mem_lazy: median(&history.mem).unwrap_or(group.mem_mb),
            });
        }

        self.metric_history
            .retain(|name, _| next_previous.contains_key(name));
        self.previous_command_totals = next_previous;
        self.pids_by_command = refreshed_pids;

        sort_processes(&mut proc_list, &self.sort_state, self.lazy_mode);
        self.current_processes = proc_list;
    }

    fn save_snapshot_document(&self) -> io::Result<String> {
        let artifacts = PathBuf::from("artifacts");
        fs::create_dir_all(&artifacts)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        let epoch_ms = now.as_millis() as u64;
        let stamp = now.as_secs();

        let process_rows = self.current_processes.iter().take(25).collect::<Vec<_>>();
        let network_rows = self.network_rows.iter().take(25).collect::<Vec<_>>();

        let json_path = artifacts.join(format!("snapshot-{stamp}.json"));
        let mut json = String::new();
        json.push_str("{\n");
        json.push_str("  \"schema_version\": 1,\n");
        json.push_str(&format!("  \"generated_at_epoch_ms\": {epoch_ms},\n"));
        json.push_str(&format!(
            "  \"mode\": \"{}\",\n",
            json_escape(self.view_mode.label())
        ));
        json.push_str(&format!(
            "  \"sort\": \"{}\",\n",
            json_escape(self.sort_state.by.label())
        ));
        json.push_str(&format!(
            "  \"descending\": {},\n",
            if self.sort_state.descending {
                "true"
            } else {
                "false"
            }
        ));
        json.push_str(&format!(
            "  \"poll_interval_ms\": {},\n",
            self.poll_interval_ms
        ));
        json.push_str(&format!(
            "  \"filter_query\": \"{}\",\n",
            json_escape(&self.filter_state.search_query)
        ));
        json.push_str("  \"top_n\": 25,\n");

        let mut proc_arr = JsonArray::new("    ");
        for row in &process_rows {
            proc_arr.push_object(&[
                ("name", JsonValue::Str(row.name.clone())),
                ("cpu", JsonValue::Float(row.cpu)),
                ("mem_mb", JsonValue::Float(row.mem_mb)),
                ("cpu_delta", JsonValue::Float(row.cpu_delta)),
                ("mem_delta_mb", JsonValue::Float(row.mem_delta_mb)),
                ("disk_read_bps", JsonValue::Float(row.disk_read_bps)),
                ("disk_write_bps", JsonValue::Float(row.disk_write_bps)),
                ("is_new", JsonValue::Bool(row.is_new)),
            ]);
        }
        json.push_str(&format!("  \"process_rows\": {},\n", proc_arr.finish()));

        let mut net_arr = JsonArray::new("    ");
        for row in &network_rows {
            net_arr.push_object(&[
                ("name", JsonValue::Str(row.name.clone())),
                ("rx_bps", JsonValue::Float(row.rx_bps)),
                ("tx_bps", JsonValue::Float(row.tx_bps)),
                ("total_bps", JsonValue::Float(row.total_bps)),
            ]);
        }
        json.push_str(&format!("  \"network_rows\": {}\n", net_arr.finish()));
        json.push_str("}\n");
        fs::write(&json_path, json)?;

        let mut md = String::new();
        md.push_str(&format!("# dtop snapshot {stamp}\n\n"));
        md.push_str(&format!("- mode: {}\n", self.view_mode.label()));
        md.push_str(&format!(
            "- sort: {} ({})\n",
            self.sort_state.by.label(),
            if self.sort_state.descending {
                "desc"
            } else {
                "asc"
            }
        ));
        md.push_str(&format!("- poll interval: {}ms\n", self.poll_interval_ms));
        md.push_str(&format!(
            "- filter: `{}`\n\n",
            self.filter_state.search_query
        ));

        if !process_rows.is_empty() {
            md.push_str("## Processes\n\n");
            md.push_str("| Name | CPU | MEM MB | CPU Δ | MEM Δ MB | Read/s | Write/s | New |\n");
            md.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | :---: |\n");
            for row in &process_rows {
                md.push_str(&format!(
                    "| {} | {:.2} | {:.2} | {:+.2} | {:+.2} | {} | {} | {} |\n",
                    row.name,
                    row.cpu,
                    row.mem_mb,
                    row.cpu_delta,
                    row.mem_delta_mb,
                    format_bytes_per_second(row.disk_read_bps),
                    format_bytes_per_second(row.disk_write_bps),
                    if row.is_new { "yes" } else { "" }
                ));
            }
            md.push('\n');
        }

        if !network_rows.is_empty() {
            md.push_str("## Network\n\n");
            md.push_str("| Interface | RX/s | TX/s | Total/s |\n");
            md.push_str("| --- | ---: | ---: | ---: |\n");
            for row in &network_rows {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    row.name,
                    format_bytes_per_second(row.rx_bps),
                    format_bytes_per_second(row.tx_bps),
                    format_bytes_per_second(row.total_bps),
                ));
            }
        }

        let md_path = artifacts.join(format!("snapshot-{stamp}.md"));
        fs::write(&md_path, md)?;

        Ok(json_path.display().to_string())
    }
}

fn sort_processes(processes: &mut [ProcEntry], sort_state: &SortState, lazy_mode: bool) {
    match sort_state.by {
        SortBy::Cpu => {
            if lazy_mode {
                processes.sort_by(|a, b| a.cpu_lazy.total_cmp(&b.cpu_lazy));
            } else {
                processes.sort_by(|a, b| a.cpu.total_cmp(&b.cpu));
            }
        }
        SortBy::Mem => {
            if lazy_mode {
                processes.sort_by(|a, b| a.mem_lazy.total_cmp(&b.mem_lazy));
            } else {
                processes.sort_by(|a, b| a.mem_mb.total_cmp(&b.mem_mb));
            }
        }
        SortBy::Name => {
            processes.sort_by_key(|p| p.name.to_lowercase());
        }
        SortBy::DiskRead => {
            processes.sort_by(|a, b| a.disk_read_bps.total_cmp(&b.disk_read_bps));
        }
        SortBy::DiskWrite => {
            processes.sort_by(|a, b| a.disk_write_bps.total_cmp(&b.disk_write_bps));
        }
        SortBy::CpuDelta => {
            processes.sort_by(|a, b| a.cpu_delta.total_cmp(&b.cpu_delta));
        }
        SortBy::MemDelta => {
            processes.sort_by(|a, b| a.mem_delta_mb.total_cmp(&b.mem_delta_mb));
        }
    }

    if sort_state.descending {
        processes.reverse();
    }
}

fn push_bounded_sample(history: &mut VecDeque<f64>, value: f64, max_len: usize) {
    history.push_back(value);
    while history.len() > max_len {
        history.pop_front();
    }
}

fn draw_ui(f: &mut ratatui::Frame<'_>, app: &App) -> usize {
    let area = f.area();
    let mut y = area.y;

    if app.view_mode == ViewMode::Process
        && (app.filter_state.search_mode || !app.filter_state.search_query.is_empty())
    {
        let mut query = app.filter_state.search_query.clone();
        if app.filter_state.search_mode {
            query.push('_');
        }
        let search_area = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let search_line = Line::from(vec![
            Span::styled(
                "Search: ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(query),
        ]);
        f.render_widget(Paragraph::new(search_line), search_area);
        y = y.saturating_add(1);
    }

    let header_area = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    let sys = &app.system;
    let sys_data = SystemHeaderData {
        cpu_total_pct: sys.cpu_total_pct.unwrap_or(0.0),
        mem_used_mb: sys.mem_used_mb,
        mem_total_mb: sys.mem_total_mb,
        swap_used_mb: sys.swap_used_mb,
        swap_total_mb: sys.swap_total_mb,
        load_avg_1: sys.load_avg_1,
        load_avg_5: sys.load_avg_5,
        load_avg_15: sys.load_avg_15,
        uptime_seconds: sys.uptime_seconds,
    };
    render_system_overview_header(f, &sys_data, header_area);
    y = y.saturating_add(1);

    let status_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };

    let content_height = area
        .height
        .saturating_sub(y.saturating_sub(area.y))
        .saturating_sub(1);
    let content_area = Rect {
        x: area.x,
        y,
        width: area.width,
        height: content_height,
    };

    let visible_rows = {
        if content_area.width == 0 || content_area.height == 0 {
            0
        } else {
            let vis = content_area.height.saturating_sub(1) as usize;
            let start_index = app.selected_index.unwrap_or(0).saturating_sub(vis / 2);
            match app.view_mode {
                ViewMode::Process => {
                    let rows: Vec<ProcessRowData> = app.current_processes.iter().map(|p| {
                        let (cpu_hist, mem_hist) = app.metric_history.get(&p.name)
                            .map(|h| (h.cpu.clone(), h.mem.clone()))
                            .unwrap_or_default();
                        ProcessRowData {
                            name: p.name.clone(),
                            cpu: p.cpu,
                            cpu_lazy: p.cpu_lazy,
                            cpu_delta: p.cpu_delta,
                            mem_mb: p.mem_mb,
                            mem_lazy: p.mem_lazy,
                            mem_delta_mb: p.mem_delta_mb,
                            disk_read_bps: p.disk_read_bps,
                            disk_write_bps: p.disk_write_bps,
                            is_new: p.is_new,
                            count: p.count,
                            cpu_history: cpu_hist,
                            mem_history: mem_hist,
                        }
                    }).collect();
                    let data = ProcessTableData {
                        rows: &rows,
                        filtered_indices: &app.filtered_indices,
                        selected_index: app.selected_index,
                        sort_by: app.sort_state.by,
                        delta_mode: app.delta_mode,
                        lazy_mode: app.lazy_mode,
                        show_disk_columns: app.show_disk_columns,
                        graph_enabled: app.graph_mode.show_list(),
                    };
                    draw_process_table(f, &data, content_area, start_index, vis)
                }
                ViewMode::Network => {
                    let rows: Vec<NetworkRowData> = app.network_rows.iter().map(|n| {
                        NetworkRowData {
                            name: n.name.clone(),
                            rx_bps: n.rx_bps,
                            tx_bps: n.tx_bps,
                            total_bps: n.total_bps,
                        }
                    }).collect();
                    let data = NetworkTableData {
                        rows: &rows,
                        filtered_indices: &app.filtered_indices,
                        selected_index: app.selected_index,
                        sort_by: app.network_sort_by,
                    };
                    draw_network_table(f, &data, content_area, start_index, vis)
                }
            }
        }
    };

    let status_data = StatusLineData {
        view_label: app.view_mode.label(),
        sort_label: app.sort_state.by.label(),
        sort_descending: app.sort_state.descending,
        poll_interval_ms: app.poll_interval_ms,
        last_poll_ms: app.last_poll_ms,
        last_parse_ms: app.last_parse_ms,
        last_render_ms: app.last_render_ms,
        recent_exited_commands: &app.recent_exited_commands,
        action_audit_line: app.action_audit_line.as_deref(),
        last_snapshot_path: app.last_snapshot_path.as_deref(),
        status_error: app.status_error.as_deref(),
        is_network_view: app.view_mode == ViewMode::Network,
    };
    render_status_line(f, &status_data, status_area);

    if app.show_help {
        let help_data = HelpModalData {
            poll_interval_ms: app.poll_interval_ms,
            mode_label: app.view_mode.label().to_string(),
            sort_label: app.sort_state.by.label().to_string(),
            lazy_window: app.lazy_window,
            kill_debug_enabled: app.debug_kill,
            backend_label: app.collector_backend.as_str().to_string(),
            graph_mode: app.graph_mode.label().to_string(),
            keybindings: app.key_registry.help_rows.clone(),
        };
        render_help_modal(f, &help_data);
    }

    if let Some(prompt) = app.kill_prompt.as_ref() {
        let kill_data = KillModalData {
            command: &prompt.command,
            pids: &prompt.pids,
            signal_label: prompt.signal.label(),
            graceful_then_force: prompt.graceful_then_force,
            require_typed_confirmation: app.require_typed_confirmation,
            typed_confirmation: &prompt.typed_confirmation,
            debug_summary: prompt.debug_summary.as_deref(),
            error: app.kill_prompt_error.as_deref(),
        };
        render_kill_modal(f, &kill_data);
    }

    visible_rows
}



struct JsonArray {
    buf: String,
    indent: &'static str,
    count: usize,
}

impl JsonArray {
    fn new(indent: &'static str) -> Self {
        Self {
            buf: String::from("[\n"),
            indent,
            count: 0,
        }
    }

    fn push_object(&mut self, fields: &[(&str, JsonValue)]) {
        if self.count > 0 {
            self.buf.push_str(",\n");
        }
        self.buf.push_str(self.indent);
        self.buf.push_str("{\n");
        for (i, (key, val)) in fields.iter().enumerate() {
            self.buf.push_str(self.indent);
            self.buf.push_str("  ");
            self.buf
                .push_str(&format!("\"{}\": {}", key, val.render()));
            if i + 1 < fields.len() {
                self.buf.push(',');
            }
            self.buf.push('\n');
        }
        self.buf.push_str(self.indent);
        self.buf.push('}');
        self.count += 1;
    }

    fn finish(mut self) -> String {
        self.buf.push('\n');
        self.buf
            .push_str(self.indent.strip_suffix("  ").unwrap_or(""));
        self.buf.push(']');
        self.buf
    }
}

enum JsonValue {
    Str(String),
    Float(f64),
    Bool(bool),
}

impl JsonValue {
    fn render(&self) -> String {
        match self {
            JsonValue::Str(s) => format!("\"{}\"", json_escape(s)),
            JsonValue::Float(v) => format!("{v:.6}"),
            JsonValue::Bool(b) => b.to_string(),
        }
    }
}

fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for ch in raw.chars() {
        match ch {
            '\"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

fn parse_args(args: &[String]) -> Result<CliArgs, String> {
    let mut out = CliArgs::default();

    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--snapshot" => out.snapshot_mode = true,
            "--collector" => {
                let Some(value) = iter.next() else {
                    return Err("--collector expects a value".to_string());
                };
                out.collector_override = Some(value.clone());
            }
            _ if arg.starts_with("--collector=") => {
                let value = arg.trim_start_matches("--collector=");
                if value.is_empty() {
                    return Err("--collector expects a value".to_string());
                }
                out.collector_override = Some(value.to_string());
            }
            _ => {}
        }
    }

    Ok(out)
}

fn load_config() -> Result<AppConfig, String> {
    let mut cfg = AppConfig::default();

    let Some(home) = std::env::var_os("HOME") else {
        return Ok(cfg);
    };
    let path = PathBuf::from(home).join(".config/dtop/config.toml");
    if !path.exists() {
        return Ok(cfg);
    }

    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let mut section = String::new();
    for (line_no, original) in raw.lines().enumerate() {
        let line = original.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            continue;
        }

        let Some((key_raw, value_raw)) = line.split_once('=') else {
            return Err(format!(
                "invalid config {}:{}: expected key=value",
                path.display(),
                line_no + 1
            ));
        };
        let key = key_raw.trim();
        let value = value_raw.trim();

        match section.as_str() {
            "" => match key {
                "poll_interval_ms" => {
                    let parsed = parse_u64(value).ok_or_else(|| {
                        format!(
                            "invalid config {}:{}: poll_interval_ms must be integer",
                            path.display(),
                            line_no + 1
                        )
                    })?;
                    cfg.poll_interval_ms = parsed.clamp(100, 10_000);
                }
                "default_sort" => {
                    let raw_sort = parse_string_value(value).ok_or_else(|| {
                        format!(
                            "invalid config {}:{}: default_sort must be string",
                            path.display(),
                            line_no + 1
                        )
                    })?;
                    cfg.default_sort = parse_sort(&raw_sort).ok_or_else(|| {
                        format!(
                            "invalid config {}:{}: unsupported default_sort '{}'",
                            path.display(),
                            line_no + 1,
                            raw_sort
                        )
                    })?;
                }
                "require_typed_confirmation" => {
                    cfg.require_typed_confirmation = parse_bool(value).ok_or_else(|| {
                        format!(
                            "invalid config {}:{}: require_typed_confirmation must be true/false",
                            path.display(),
                            line_no + 1
                        )
                    })?;
                }
                _ => {}
            },
            "visible_columns" => match key {
                "disk" => {
                    cfg.show_disk_columns = parse_bool(value).ok_or_else(|| {
                        format!(
                            "invalid config {}:{}: visible_columns.disk must be true/false",
                            path.display(),
                            line_no + 1
                        )
                    })?;
                }
                "graphs" => {
                    let enabled = parse_bool(value).ok_or_else(|| {
                        format!(
                            "invalid config {}:{}: visible_columns.graphs must be true/false",
                            path.display(),
                            line_no + 1
                        )
                    })?;
                    cfg.graph_mode = if enabled {
                        GraphMode::List
                    } else {
                        GraphMode::Off
                    };
                }
                _ => {}
            },
            "keymap" => {
                let key_name = key.trim_matches(|c| c == '"' || c == '\'').to_string();
                let Some(key_spec) = parse_string_value(value) else {
                    return Err(format!(
                        "invalid config {}:{}: keymap values must be strings",
                        path.display(),
                        line_no + 1
                    ));
                };
                cfg.key_overrides.insert(key_name, key_spec);
            }
            _ => {}
        }
    }

    Ok(cfg)
}

fn parse_string_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('\"') && trimmed.ends_with('\"') && trimmed.len() >= 2 {
        return Some(trimmed[1..trimmed.len() - 1].to_string());
    }
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        return Some(trimmed[1..trimmed.len() - 1].to_string());
    }
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_u64(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

fn parse_sort(value: &str) -> Option<SortBy> {
    match value {
        "cpu" => Some(SortBy::Cpu),
        "mem" => Some(SortBy::Mem),
        "name" => Some(SortBy::Name),
        "disk_read" => Some(SortBy::DiskRead),
        "disk_write" => Some(SortBy::DiskWrite),
        "cpu_delta" => Some(SortBy::CpuDelta),
        "mem_delta" => Some(SortBy::MemDelta),
        _ => None,
    }
}

fn build_key_registry(overrides: &HashMap<String, String>) -> Result<KeyRegistry, String> {
    let mut defaults = vec![
        (Action::Quit, "q", "Quit"),
        (Action::ToggleHelp, "?", "Toggle help"),
        (Action::PollFaster, "+", "Poll faster"),
        (Action::PollSlower, "-", "Poll slower"),
        (Action::ToggleLazy, "x", "Toggle lazy median"),
        (Action::ToggleGraph, "g", "Toggle graph columns"),
        (Action::ToggleViewMode, "n", "Toggle Process/Network"),
        (Action::ToggleDeltaMode, "v", "Toggle delta mode"),
        (Action::ToggleDiskColumns, "i", "Toggle disk columns"),
        (Action::SortCpu, "c", "Sort CPU or Total net"),
        (Action::SortMem, "m", "Sort MEM or RX net"),
        (Action::SortName, "p", "Sort name"),
        (Action::SortDiskRead, "r", "Sort disk read or TX net"),
        (Action::SortDiskWrite, "w", "Sort disk write"),
        (Action::ReverseSort, "ctrl+r", "Reverse sort"),
        (Action::MoveDown, "down", "Move selection down"),
        (Action::MoveUp, "up", "Move selection up"),
        (Action::PageDown, "pagedown", "Page down"),
        (Action::PageUp, "pageup", "Page up"),
        (Action::Search, "f", "Search/filter"),
        (Action::Kill, "k", "Kill action"),
        (Action::ToggleKillDebug, "d", "Toggle kill debug"),
        (Action::Snapshot, "s", "Write snapshot"),
    ];

    for (action_name, key_spec) in overrides {
        let Some(action) = Action::from_name(action_name) else {
            return Err(format!("unknown keymap action '{action_name}'"));
        };

        if let Some(existing) = defaults.iter_mut().find(|entry| entry.0 == action) {
            existing.1 = key_spec;
        } else {
            defaults.push((action, key_spec, "custom"));
        }
    }

    let mut by_key: HashMap<String, Action> = HashMap::new();
    let mut help_rows = Vec::new();

    for (action, key_spec, desc) in defaults {
        let (code, mods, display) = parse_key_spec(key_spec)?;
        let signature = key_to_string(code, mods);
        if let Some(previous) = by_key.insert(signature.clone(), action) {
            return Err(format!(
                "key conflict on '{display}': {:?} and {:?}",
                previous, action
            ));
        }
        help_rows.push((display, desc.to_string()));
    }

    Ok(KeyRegistry { by_key, help_rows })
}

fn parse_key_spec(raw: &str) -> Result<(KeyCode, KeyModifiers, String), String> {
    let lowered = raw.trim().to_lowercase();
    if lowered.is_empty() {
        return Err("empty key spec".to_string());
    }

    let mut mods = KeyModifiers::NONE;
    let mut key_part = lowered.as_str();
    if let Some(rest) = key_part.strip_prefix("ctrl+") {
        mods |= KeyModifiers::CONTROL;
        key_part = rest;
    }

    let code = match key_part {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "comma" => KeyCode::Char(','),
        "dot" => KeyCode::Char('.'),
        "question" => KeyCode::Char('?'),
        one if one.chars().count() == 1 => {
            let ch = one.chars().next().expect("char exists");
            KeyCode::Char(ch)
        }
        _ => return Err(format!("unsupported key spec '{raw}'")),
    };

    Ok((code, mods, raw.to_string()))
}

fn key_to_string(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut parts = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push_str("ctrl+");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push_str("alt+");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push_str("shift+");
    }
    match code {
        KeyCode::Char(c) => parts.push(c),
        KeyCode::Up => parts.push_str("up"),
        KeyCode::Down => parts.push_str("down"),
        KeyCode::Left => parts.push_str("left"),
        KeyCode::Right => parts.push_str("right"),
        KeyCode::PageUp => parts.push_str("pageup"),
        KeyCode::PageDown => parts.push_str("pagedown"),
        KeyCode::Enter => parts.push_str("enter"),
        KeyCode::Esc => parts.push_str("esc"),
        _ => parts.push_str(&format!("{code:?}")),
    }
    parts
}

fn start_sampler_thread(
    collector: Box<dyn Collector>,
    poll_interval: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
) -> Receiver<SamplerPacket> {
    let (tx, rx): (SyncSender<SamplerPacket>, Receiver<SamplerPacket>) = sync_channel(1);

    thread::spawn(move || {
        while !stop_flag.load(Ordering::Relaxed) {
            let loop_start = Instant::now();

            let collect_start = Instant::now();
            let mut collection_error = None;
            let processes = collector.collect_processes().unwrap_or_else(|e| {
                collection_error = Some(format!("process collection: {e}"));
                Vec::new()
            });
            let system = collector.collect_system_stats().ok().flatten();
            let interfaces = collector.collect_network_interfaces().unwrap_or_else(|e| {
                if collection_error.is_none() {
                    collection_error = Some(format!("network collection: {e}"));
                }
                Vec::new()
            });
            let poll_ms = collect_start.elapsed().as_secs_f64() * 1000.0;

            let packet = SamplerPacket {
                captured_at: Instant::now(),
                processes,
                system,
                interfaces,
                poll_ms,
                parse_ms: 0.0,
                collection_error,
            };

            match tx.try_send(packet) {
                Ok(_) => {}
                Err(TrySendError::Full(_)) => {} // drop stale packet
                Err(TrySendError::Disconnected(_)) => break,
            }

            let interval = Duration::from_millis(poll_interval.load(Ordering::Relaxed).max(100));
            let elapsed = loop_start.elapsed();
            if elapsed < interval {
                thread::sleep(interval - elapsed);
            }
        }
    });

    rx
}

fn run_snapshot_once(
    cfg: AppConfig,
    collector_backend: CollectorBackend,
    collector: Box<dyn Collector>,
) -> Result<(), Box<dyn std::error::Error>> {
    let key_registry = build_key_registry(&cfg.key_overrides).map_err(io::Error::other)?;
    let poll_interval_shared = Arc::new(AtomicU64::new(cfg.poll_interval_ms));
    let mut app = App::new(
        cfg,
        collector_backend,
        key_registry,
        Box::new(LibcProcessController),
        poll_interval_shared,
    );

    let start = Instant::now();
    let processes = collector.collect_processes().unwrap_or_default();
    let system = collector.collect_system_stats().ok().flatten();
    let interfaces = collector.collect_network_interfaces().unwrap_or_default();
    let packet = SamplerPacket {
        captured_at: Instant::now(),
        processes,
        system,
        interfaces,
        poll_ms: start.elapsed().as_secs_f64() * 1000.0,
        parse_ms: 0.0,
        collection_error: None,
    };
    app.apply_packet(packet);

    let path = app.save_snapshot_document()?;
    let _ = writeln!(io::stdout(), "Snapshot written: {path}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let cli = parse_args(&args).map_err(io::Error::other)?;
    let cfg = load_config().map_err(io::Error::other)?;
    let (collector_backend, collector) =
        build_collector(cli.collector_override.as_deref()).map_err(io::Error::other)?;

    if cli.snapshot_mode {
        return run_snapshot_once(cfg, collector_backend, collector);
    }

    let key_registry = build_key_registry(&cfg.key_overrides).map_err(io::Error::other)?;
    let poll_interval_shared = Arc::new(AtomicU64::new(cfg.poll_interval_ms));

    let mut app = App::new(
        cfg,
        collector_backend,
        key_registry,
        Box::new(LibcProcessController),
        poll_interval_shared.clone(),
    );

    let stop_flag = Arc::new(AtomicBool::new(false));
    let rx = start_sampler_thread(collector, poll_interval_shared, stop_flag.clone());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(20);
    let mut should_quit = false;

    while !should_quit {
        let mut latest_packet = None;
        while let Ok(packet) = rx.try_recv() {
            latest_packet = Some(packet);
        }
        if let Some(packet) = latest_packet {
            app.apply_packet(packet);
        }

        let render_start = Instant::now();
        let mut visible_rows = 0usize;
        terminal.draw(|f| {
            visible_rows = draw_ui(f, &app);
        })?;
        app.last_render_ms = render_start.elapsed().as_secs_f64() * 1000.0;

        if event::poll(tick_rate)?
            && let Event::Key(key_event) = event::read()?
            && key_event.kind == KeyEventKind::Press
        {
            if is_ctrl_c_quit(key_event.code, key_event.modifiers) {
                should_quit = true;
                continue;
            }
            should_quit = app.handle_key(key_event.code, key_event.modifiers, visible_rows);
        }
    }

    stop_flag.store(true, Ordering::Relaxed);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Action, App, AppConfig, CliArgs, CollectorBackend, KeyRegistry, SamplerPacket,
        build_key_registry, format_bytes_per_second, key_to_string, parse_args,
    };
    use crate::ProcessController;
    use crossterm::event::{KeyCode, KeyModifiers};
    use dtop_collector::ProcessRecord;
    use dtop_core::SortBy;
    use std::collections::HashMap;
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    struct MockProcessController;

    impl ProcessController for MockProcessController {
        fn send_signal(&self, _pid: i32, _signal: i32) -> io::Result<()> {
            Ok(())
        }

        fn is_alive(&self, _pid: i32) -> bool {
            false
        }
    }

    fn build_test_app() -> App {
        let cfg = AppConfig {
            poll_interval_ms: 1000,
            default_sort: SortBy::Cpu,
            show_disk_columns: true,
            graph_mode: super::GraphMode::Off,
            require_typed_confirmation: true,
            key_overrides: HashMap::new(),
        };
        let key_registry = KeyRegistry {
            by_key: HashMap::new(),
            help_rows: Vec::new(),
        };

        App::new(
            cfg,
            CollectorBackend::MacPs,
            key_registry,
            Box::new(MockProcessController),
            Arc::new(AtomicU64::new(1000)),
        )
    }

    #[test]
    fn parses_cli_args_snapshot_and_collector() {
        let args = vec![
            "dtop".to_string(),
            "--snapshot".to_string(),
            "--collector=mac-ps".to_string(),
        ];
        let parsed = parse_args(&args).expect("args should parse");
        assert!(parsed.snapshot_mode);
        assert_eq!(parsed.collector_override.as_deref(), Some("mac-ps"));
    }

    #[test]
    fn formats_byte_rates_human_readable() {
        assert_eq!(format_bytes_per_second(120.0), "120 B/s");
        assert_eq!(format_bytes_per_second(2048.0), "2.0 KB/s");
    }

    #[test]
    fn snapshot_document_write_sets_path() {
        let mut app = build_test_app();
        app.current_processes.push(dtop_core::ProcEntry {
            name: "node".to_string(),
            cpu: 10.0,
            mem_mb: 100.0,
            disk_read_bps: 1024.0,
            disk_write_bps: 2048.0,
            cpu_delta: 2.0,
            mem_delta_mb: 3.0,
            is_new: true,
            count: 1,
            cpu_lazy: 10.0,
            mem_lazy: 100.0,
        });
        let path = app
            .save_snapshot_document()
            .expect("snapshot should be written");
        assert!(path.ends_with(".json"));
    }

    #[test]
    fn key_registry_detects_conflicts() {
        let mut overrides = HashMap::new();
        overrides.insert("sort_cpu".to_string(), "m".to_string());
        let err = build_key_registry(&overrides).expect_err("should reject conflict");
        assert!(err.contains("key conflict"));
    }

    #[test]
    fn applies_deltas_and_marks_new_processes() {
        let mut app = build_test_app();
        let first = SamplerPacket {
            captured_at: Instant::now(),
            processes: vec![ProcessRecord {
                pid: 10,
                ppid: 1,
                name: "node".to_string(),
                cpu: 10.0,
                mem_mb: 100.0,
                disk_read_bytes_total: Some(1000),
                disk_write_bytes_total: Some(2000),
            }],
            system: None,
            interfaces: Vec::new(),
            poll_ms: 1.0,
            parse_ms: 0.0,
            collection_error: None,
        };
        app.apply_packet(first);
        assert_eq!(app.current_processes.len(), 1);
        assert!(app.current_processes[0].is_new);

        let second = SamplerPacket {
            captured_at: Instant::now() + std::time::Duration::from_secs(1),
            processes: vec![ProcessRecord {
                pid: 10,
                ppid: 1,
                name: "node".to_string(),
                cpu: 15.0,
                mem_mb: 104.0,
                disk_read_bytes_total: Some(2000),
                disk_write_bytes_total: Some(3000),
            }],
            system: None,
            interfaces: Vec::new(),
            poll_ms: 1.0,
            parse_ms: 0.0,
            collection_error: None,
        };
        app.apply_packet(second);

        assert_eq!(app.current_processes[0].cpu_delta, 5.0);
        assert_eq!(app.current_processes[0].mem_delta_mb, 4.0);
        assert!(!app.current_processes[0].is_new);
    }

    #[test]
    fn key_to_string_is_stable() {
        assert_eq!(key_to_string(KeyCode::Char('q'), KeyModifiers::NONE), "q");
        assert_eq!(
            key_to_string(KeyCode::Char('r'), KeyModifiers::CONTROL),
            "ctrl+r"
        );
        assert_eq!(key_to_string(KeyCode::Up, KeyModifiers::NONE), "up");
        assert_eq!(key_to_string(KeyCode::PageDown, KeyModifiers::NONE), "pagedown");
    }

    #[test]
    fn parse_args_defaults() {
        let args = vec!["dtop".to_string()];
        let parsed = parse_args(&args).expect("args should parse");
        assert_eq!(parsed.snapshot_mode, CliArgs::default().snapshot_mode);
    }

    #[test]
    fn action_mapping_supports_known_names() {
        assert_eq!(Action::from_name("quit"), Some(Action::Quit));
        assert_eq!(Action::from_name("unknown"), None);
    }
}
