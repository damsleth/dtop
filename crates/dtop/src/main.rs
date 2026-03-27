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
    HelpModalData, KillModalData, ThresholdLevel, format_uptime_short, is_ctrl_c_quit,
    is_search_input_char, render_help_modal, render_kill_modal, sort_header_style, sparkline,
    threshold_color, threshold_level, truncate_to_width,
};
use libc::{SIGINT, SIGKILL, SIGTERM, kill};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};

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
enum NetworkSortBy {
    Rx,
    Tx,
    Total,
    Name,
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
    theme: Option<String>,
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
            theme: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CliArgs {
    collector_override: Option<String>,
    snapshot_mode: bool,
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
    system_cpu_total_pct: Option<f64>,
    system_mem_used_mb: Option<f64>,
    system_mem_total_mb: Option<f64>,
    system_swap_used_mb: Option<f64>,
    system_swap_total_mb: Option<f64>,
    system_load_avg_1: Option<f64>,
    system_load_avg_5: Option<f64>,
    system_load_avg_15: Option<f64>,
    system_uptime_seconds: Option<u64>,
    system_net_rx_bps: Option<f64>,
    system_net_tx_bps: Option<f64>,
    system_disk_read_bps: Option<f64>,
    system_disk_write_bps: Option<f64>,
    previous_system_net_totals: Option<NetTotalsSample>,
    previous_system_disk_totals: Option<RateTotalsSample>,
    system_cpu_history: VecDeque<f64>,
    system_mem_history: VecDeque<f64>,
    system_net_history: VecDeque<f64>,
    kill_prompt: Option<KillPrompt>,
    kill_prompt_error: Option<String>,
    process_controller: Box<dyn ProcessController>,
    action_audit_line: Option<String>,
    last_poll_ms: f64,
    last_parse_ms: f64,
    last_render_ms: f64,
    last_snapshot_path: Option<String>,
    status_error: Option<String>,
    theme_name: Option<String>,
}

type GroupedProcess = (f64, f64, usize, Vec<i32>, f64, f64);

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
            system_cpu_total_pct: None,
            system_mem_used_mb: None,
            system_mem_total_mb: None,
            system_swap_used_mb: None,
            system_swap_total_mb: None,
            system_load_avg_1: None,
            system_load_avg_5: None,
            system_load_avg_15: None,
            system_uptime_seconds: None,
            system_net_rx_bps: None,
            system_net_tx_bps: None,
            system_disk_read_bps: None,
            system_disk_write_bps: None,
            previous_system_net_totals: None,
            previous_system_disk_totals: None,
            system_cpu_history: VecDeque::new(),
            system_mem_history: VecDeque::new(),
            system_net_history: VecDeque::new(),
            kill_prompt: None,
            kill_prompt_error: None,
            process_controller,
            action_audit_line: None,
            last_poll_ms: 0.0,
            last_parse_ms: 0.0,
            last_render_ms: 0.0,
            last_snapshot_path: None,
            status_error: None,
            theme_name: cfg.theme,
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
                        continue;
                    }

                    if graceful_then_force && signal != SignalChoice::Kill {
                        thread::sleep(Duration::from_millis(300));
                        if self.process_controller.is_alive(*pid)
                            && let Err(err) = self.process_controller.send_signal(*pid, SIGKILL)
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

        if let Some(system) = packet.system {
            self.apply_system_stats(packet.captured_at, system);
        }

        self.apply_interface_counters(packet.captured_at, &packet.interfaces);
        self.apply_process_snapshot(packet.captured_at, packet.processes);
        self.update_filter_and_selection();
    }

    fn apply_system_stats(&mut self, captured_at: Instant, stats: SystemStats) {
        self.system_cpu_total_pct = stats.cpu_total_pct;
        self.system_mem_used_mb = stats.mem_used_mb;
        self.system_mem_total_mb = stats.mem_total_mb;
        self.system_swap_used_mb = stats.swap_used_mb;
        self.system_swap_total_mb = stats.swap_total_mb;
        self.system_load_avg_1 = stats.load_avg_1;
        self.system_load_avg_5 = stats.load_avg_5;
        self.system_load_avg_15 = stats.load_avg_15;
        self.system_uptime_seconds = stats.uptime_seconds;

        if let Some(v) = stats.cpu_total_pct {
            push_bounded_sample(&mut self.system_cpu_history, v, self.lazy_window);
        }
        if let (Some(used), Some(total)) = (stats.mem_used_mb, stats.mem_total_mb)
            && total > 0.0
        {
            push_bounded_sample(
                &mut self.system_mem_history,
                (used / total) * 100.0,
                self.lazy_window,
            );
        }

        if let (Some(rx_total), Some(tx_total)) =
            (stats.net_rx_bytes_total, stats.net_tx_bytes_total)
        {
            if let Some(prev) = self.previous_system_net_totals.clone() {
                let elapsed = captured_at.duration_since(prev.captured_at).as_secs_f64();
                if elapsed > 0.0 {
                    let rx_bps = rx_total.saturating_sub(prev.rx_total) as f64 / elapsed;
                    let tx_bps = tx_total.saturating_sub(prev.tx_total) as f64 / elapsed;
                    self.system_net_rx_bps = Some(rx_bps);
                    self.system_net_tx_bps = Some(tx_bps);
                    push_bounded_sample(
                        &mut self.system_net_history,
                        rx_bps + tx_bps,
                        self.lazy_window,
                    );
                }
            }
            self.previous_system_net_totals = Some(NetTotalsSample {
                rx_total,
                tx_total,
                captured_at,
            });
        }

        if let (Some(read_total), Some(write_total)) =
            (stats.disk_read_bytes_total, stats.disk_write_bytes_total)
        {
            if let Some(prev) = self.previous_system_disk_totals.clone() {
                let elapsed = captured_at.duration_since(prev.captured_at).as_secs_f64();
                if elapsed > 0.0 {
                    self.system_disk_read_bps =
                        Some(read_total.saturating_sub(prev.read_total) as f64 / elapsed);
                    self.system_disk_write_bps =
                        Some(write_total.saturating_sub(prev.write_total) as f64 / elapsed);
                }
            }
            self.previous_system_disk_totals = Some(RateTotalsSample {
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
            NetworkSortBy::Name => {
                rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }
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

            let entry =
                grouped
                    .entry(process.name.clone())
                    .or_insert((0.0, 0.0, 0, Vec::new(), 0.0, 0.0));
            entry.0 += process.cpu;
            entry.1 += process.mem_mb;
            entry.2 += 1;
            entry.3.push(process.pid);
            entry.4 += disk_read_bps;
            entry.5 += disk_write_bps;

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

        for (name, (cpu, mem_mb, count, pids, disk_read_bps, disk_write_bps)) in grouped {
            refreshed_pids.insert(name.clone(), pids);
            let (prev_cpu, prev_mem) = self
                .previous_command_totals
                .get(&name)
                .copied()
                .unwrap_or((0.0, 0.0));
            next_previous.insert(name.clone(), (cpu, mem_mb));

            let is_new = !self.previous_command_totals.contains_key(&name);
            let cpu_delta = cpu - prev_cpu;
            let mem_delta_mb = mem_mb - prev_mem;

            let history =
                self.metric_history
                    .entry(name.clone())
                    .or_insert_with(|| MetricHistory {
                        cpu: VecDeque::with_capacity(self.lazy_window),
                        mem: VecDeque::with_capacity(self.lazy_window),
                    });
            push_bounded_sample(&mut history.cpu, cpu, self.lazy_window);
            push_bounded_sample(&mut history.mem, mem_mb, self.lazy_window);

            proc_list.push(ProcEntry {
                name,
                cpu,
                mem_mb,
                disk_read_bps,
                disk_write_bps,
                cpu_delta,
                mem_delta_mb,
                is_new,
                count,
                cpu_lazy: median(&history.cpu).unwrap_or(cpu),
                mem_lazy: median(&history.mem).unwrap_or(mem_mb),
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
        json.push_str("  \"process_rows\": [\n");
        for (idx, row) in process_rows.iter().enumerate() {
            json.push_str("    {\n");
            json.push_str(&format!(
                "      \"name\": \"{}\",\n",
                json_escape(&row.name)
            ));
            json.push_str(&format!("      \"cpu\": {:.6},\n", row.cpu));
            json.push_str(&format!("      \"mem_mb\": {:.6},\n", row.mem_mb));
            json.push_str(&format!("      \"cpu_delta\": {:.6},\n", row.cpu_delta));
            json.push_str(&format!(
                "      \"mem_delta_mb\": {:.6},\n",
                row.mem_delta_mb
            ));
            json.push_str(&format!(
                "      \"disk_read_bps\": {:.6},\n",
                row.disk_read_bps
            ));
            json.push_str(&format!(
                "      \"disk_write_bps\": {:.6},\n",
                row.disk_write_bps
            ));
            json.push_str(&format!(
                "      \"is_new\": {}\n",
                if row.is_new { "true" } else { "false" }
            ));
            json.push_str("    }");
            if idx + 1 != process_rows.len() {
                json.push(',');
            }
            json.push('\n');
        }
        json.push_str("  ],\n");
        json.push_str("  \"network_rows\": [\n");
        for (idx, row) in network_rows.iter().enumerate() {
            json.push_str("    {\n");
            json.push_str(&format!(
                "      \"name\": \"{}\",\n",
                json_escape(&row.name)
            ));
            json.push_str(&format!("      \"rx_bps\": {:.6},\n", row.rx_bps));
            json.push_str(&format!("      \"tx_bps\": {:.6},\n", row.tx_bps));
            json.push_str(&format!("      \"total_bps\": {:.6}\n", row.total_bps));
            json.push_str("    }");
            if idx + 1 != network_rows.len() {
                json.push(',');
            }
            json.push('\n');
        }
        json.push_str("  ]\n");
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
            processes.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
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
    render_system_overview_header(f, app, header_area);
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

    let visible_rows = draw_table(f, app, content_area);
    render_status_line(f, app, status_area);

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

fn render_system_overview_header(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let cpu_value = app.system_cpu_total_pct.unwrap_or(0.0);
    let mem_pct = match (app.system_mem_used_mb, app.system_mem_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => (used / total) * 100.0,
        _ => 0.0,
    };
    let swap_pct = match (app.system_swap_used_mb, app.system_swap_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => (used / total) * 100.0,
        _ => 0.0,
    };

    let cpu_color = threshold_color(threshold_level(cpu_value, 70.0, 90.0));
    let mem_color = threshold_color(threshold_level(mem_pct, 75.0, 90.0));
    let swap_color = threshold_color(threshold_level(swap_pct, 80.0, 95.0));

    let load = match (
        app.system_load_avg_1,
        app.system_load_avg_5,
        app.system_load_avg_15,
    ) {
        (Some(a), Some(b), Some(c)) => format!("{a:.2}/{b:.2}/{c:.2}"),
        _ => "n/a".to_string(),
    };
    let uptime = app
        .system_uptime_seconds
        .map(format_uptime_short)
        .unwrap_or_else(|| "n/a".to_string());

    let compact = area.width < 120;
    let mut spans = Vec::new();

    if compact {
        spans.push(Span::raw("SYS "));
        spans.push(Span::styled(
            format!("CPU {:.0}% ", cpu_value),
            Style::default().fg(cpu_color),
        ));
        spans.push(Span::styled(
            format!("MEM {:.0}% ", mem_pct),
            Style::default().fg(mem_color),
        ));
        spans.push(Span::styled(
            format!("SWP {:.0}% ", swap_pct),
            Style::default().fg(swap_color),
        ));
        spans.push(Span::raw(format!("L {load} U {uptime}")));
    } else {
        spans.push(Span::raw("System "));
        spans.push(Span::styled(
            format!("CPU {:>5.1}%  ", cpu_value),
            Style::default().fg(cpu_color),
        ));
        spans.push(Span::styled(
            format!("MEM {:>5.1}%  ", mem_pct),
            Style::default().fg(mem_color),
        ));
        spans.push(Span::styled(
            format!("SWAP {:>5.1}%  ", swap_pct),
            Style::default().fg(swap_color),
        ));
        spans.push(Span::raw(format!("LOAD {load}  UPTIME {uptime}")));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_table(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }

    let visible_rows = area.height.saturating_sub(1) as usize;
    let start_index = app
        .selected_index
        .unwrap_or(0)
        .saturating_sub(visible_rows / 2);

    match app.view_mode {
        ViewMode::Process => draw_process_table(f, app, area, start_index, visible_rows),
        ViewMode::Network => draw_network_table(f, app, area, start_index, visible_rows),
    }
}

fn draw_process_table(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    start_index: usize,
    visible_rows: usize,
) -> usize {
    let name_width = if app.show_disk_columns {
        area.width.saturating_sub(46)
    } else {
        area.width.saturating_sub(30)
    };
    let graph_width: u16 = 8;
    let graph_enabled = app.graph_mode.show_list() && area.width > 80;

    let mut header_cells = vec![
        Cell::from(Span::styled(
            truncate_to_width("PROCESS", name_width as usize),
            sort_header_style(app.sort_state.by == SortBy::Name),
        )),
        Cell::from(Span::styled(
            if app.delta_mode { "CPU Δ" } else { "CPU%" },
            sort_header_style(matches!(app.sort_state.by, SortBy::Cpu | SortBy::CpuDelta)),
        )),
        Cell::from(Span::styled(
            if app.delta_mode { "MEM Δ" } else { "MEM(MB)" },
            sort_header_style(matches!(app.sort_state.by, SortBy::Mem | SortBy::MemDelta)),
        )),
    ];

    let mut constraints = vec![
        Constraint::Length(name_width.max(1)),
        Constraint::Length(10),
        Constraint::Length(10),
    ];

    if app.show_disk_columns {
        header_cells.push(Cell::from(Span::styled(
            "READ/s",
            sort_header_style(app.sort_state.by == SortBy::DiskRead),
        )));
        header_cells.push(Cell::from(Span::styled(
            "WRITE/s",
            sort_header_style(app.sort_state.by == SortBy::DiskWrite),
        )));
        constraints.push(Constraint::Length(10));
        constraints.push(Constraint::Length(10));
    }

    if graph_enabled {
        header_cells.push(Cell::from("CPU ▄"));
        header_cells.push(Cell::from("MEM ▄"));
        constraints.push(Constraint::Length(graph_width));
        constraints.push(Constraint::Length(graph_width));
    }

    let header = Row::new(header_cells).height(1);
    let mut rows = Vec::new();

    for row_i in 0..visible_rows {
        let list_index = start_index + row_i;
        if list_index >= app.filtered_indices.len() {
            break;
        }
        let real_idx = app.filtered_indices[list_index];
        let Some(proc) = app.current_processes.get(real_idx) else {
            continue;
        };

        let mut display_name = proc.name.clone();
        if proc.count > 1 {
            display_name.push_str(&format!(" ({})", proc.count));
        }
        if proc.is_new {
            display_name = format!("+ {display_name}");
        }

        let cpu_display = if app.delta_mode {
            format!("{:+.2}", proc.cpu_delta)
        } else if app.lazy_mode {
            format!("{:.2}", proc.cpu_lazy)
        } else {
            format!("{:.2}", proc.cpu)
        };

        let mem_display = if app.delta_mode {
            format!("{:+.2}", proc.mem_delta_mb)
        } else if app.lazy_mode {
            format!("{:.2}", proc.mem_lazy)
        } else {
            format!("{:.2}", proc.mem_mb)
        };

        let mut style = Style::default();
        let alert_value = if app.delta_mode {
            proc.cpu_delta.abs()
        } else if app.lazy_mode {
            proc.cpu_lazy
        } else {
            proc.cpu
        };
        style = style.fg(match threshold_level(alert_value, 20.0, 50.0) {
            ThresholdLevel::Normal => Color::Reset,
            ThresholdLevel::Warning => Color::Yellow,
            ThresholdLevel::Critical => Color::Red,
        });

        if app.selected_index == Some(list_index) {
            style = style.add_modifier(Modifier::REVERSED);
        }

        let mut cells = vec![
            Cell::from(truncate_to_width(&display_name, name_width as usize)),
            Cell::from(cpu_display),
            Cell::from(mem_display),
        ];

        if app.show_disk_columns {
            cells.push(Cell::from(format_bytes_per_second(proc.disk_read_bps)));
            cells.push(Cell::from(format_bytes_per_second(proc.disk_write_bps)));
        }

        if graph_enabled {
            if let Some(history) = app.metric_history.get(&proc.name) {
                cells.push(Cell::from(sparkline(&history.cpu, graph_width as usize)));
                cells.push(Cell::from(sparkline(&history.mem, graph_width as usize)));
            } else {
                cells.push(Cell::from(" ".repeat(graph_width as usize)));
                cells.push(Cell::from(" ".repeat(graph_width as usize)));
            }
        }

        rows.push(Row::new(cells).style(style));
    }

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1);
    f.render_widget(table, area);

    visible_rows
}

fn draw_network_table(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    start_index: usize,
    visible_rows: usize,
) -> usize {
    let name_width = area.width.saturating_sub(36);
    let header = Row::new(vec![
        Cell::from(Span::styled(
            truncate_to_width("INTERFACE", name_width as usize),
            sort_header_style(app.network_sort_by == NetworkSortBy::Name),
        )),
        Cell::from(Span::styled(
            "RX/s",
            sort_header_style(app.network_sort_by == NetworkSortBy::Rx),
        )),
        Cell::from(Span::styled(
            "TX/s",
            sort_header_style(app.network_sort_by == NetworkSortBy::Tx),
        )),
        Cell::from(Span::styled(
            "TOTAL/s",
            sort_header_style(app.network_sort_by == NetworkSortBy::Total),
        )),
    ])
    .height(1);

    let mut rows = Vec::new();
    for row_i in 0..visible_rows {
        let list_index = start_index + row_i;
        if list_index >= app.filtered_indices.len() {
            break;
        }
        let real_idx = app.filtered_indices[list_index];
        let Some(net) = app.network_rows.get(real_idx) else {
            continue;
        };

        let mut style = Style::default();
        if app.selected_index == Some(list_index) {
            style = style.add_modifier(Modifier::REVERSED);
        }

        rows.push(
            Row::new(vec![
                Cell::from(truncate_to_width(&net.name, name_width as usize)),
                Cell::from(format_bytes_per_second(net.rx_bps)),
                Cell::from(format_bytes_per_second(net.tx_bps)),
                Cell::from(format_bytes_per_second(net.total_bps)),
            ])
            .style(style),
        );
    }

    let table = Table::new(
        rows,
        vec![
            Constraint::Length(name_width.max(1)),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .column_spacing(1);
    f.render_widget(table, area);

    visible_rows
}

fn render_status_line(f: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let mut status = format!(
        "{} | Sort={}{} | Poll={}ms | Timing P {:.1}ms Parse {:.1}ms Render {:.1}ms",
        app.view_mode.label(),
        app.sort_state.by.label(),
        if app.sort_state.descending {
            "↓"
        } else {
            "↑"
        },
        app.poll_interval_ms,
        app.last_poll_ms,
        app.last_parse_ms,
        app.last_render_ms,
    );

    if !app.recent_exited_commands.is_empty() {
        status.push_str(&format!(
            " | Exited: {}",
            app.recent_exited_commands.join(", ")
        ));
    }
    if let Some(line) = app.action_audit_line.as_ref() {
        status.push_str(&format!(" | Audit: {line}"));
    }
    if let Some(path) = app.last_snapshot_path.as_ref() {
        status.push_str(&format!(" | Snapshot: {path}"));
    }
    if let Some(err) = app.status_error.as_ref() {
        status.push_str(&format!(" | Error: {err}"));
    }
    if app.view_mode == ViewMode::Network {
        status.push_str(" | Interface-level throughput view");
    }
    if let Some(theme) = app.theme_name.as_ref() {
        status.push_str(&format!(" | Theme: {theme}"));
    }

    f.render_widget(
        Paragraph::new(truncate_to_width(&status, area.width as usize)),
        area,
    );
}

fn format_bytes_per_second(bytes_per_second: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if bytes_per_second >= GB {
        format!("{:.2} GB/s", bytes_per_second / GB)
    } else if bytes_per_second >= MB {
        format!("{:.2} MB/s", bytes_per_second / MB)
    } else if bytes_per_second >= KB {
        format!("{:.1} KB/s", bytes_per_second / KB)
    } else {
        format!("{:.0} B/s", bytes_per_second)
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
                "theme" => {
                    cfg.theme = parse_string_value(value);
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
                let Some(key_name) = parse_string_value(key) else {
                    continue;
                };
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
    format!("{:?}-{:?}", modifiers, code)
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
            let processes = collector.collect_processes().unwrap_or_default();
            let system = collector.collect_system_stats().ok().flatten();
            let interfaces = collector.collect_network_interfaces().unwrap_or_default();
            let poll_ms = collect_start.elapsed().as_secs_f64() * 1000.0;

            let packet = SamplerPacket {
                captured_at: Instant::now(),
                processes,
                system,
                interfaces,
                poll_ms,
                parse_ms: 0.0,
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
            theme: None,
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
        };
        app.apply_packet(second);

        assert_eq!(app.current_processes[0].cpu_delta, 5.0);
        assert_eq!(app.current_processes[0].mem_delta_mb, 4.0);
        assert!(!app.current_processes[0].is_new);
    }

    #[test]
    fn key_to_string_is_stable() {
        let key = key_to_string(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(key.contains("Char"));
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
