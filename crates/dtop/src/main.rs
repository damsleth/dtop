use std::cmp::{Ordering, max, min};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dtop_collector::{Collector, CollectorBackend, build_collector};
use libc::{SIGKILL, kill};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use dtop_core::{
    DetailEntry, FilterState, MetricHistory, ProcEntry, ProcSnapshotEntry, SortBy, SortState,
    TreeRow, median, shrink_deque,
};
use dtop_ui::{
    HelpModalData, KillModalData, is_ctrl_c_quit, is_search_input_char, render_help_modal,
    render_kill_modal, sort_header_style, sparkline, truncate_to_width,
};

#[derive(Clone, Debug)]
struct KillPrompt {
    command: String,
    pids: Vec<i32>,
    debug_summary: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphMode {
    Off,
    List,
    Detail,
    Both,
}

impl GraphMode {
    fn cycle(self) -> Self {
        match self {
            GraphMode::Off => GraphMode::List,
            GraphMode::List => GraphMode::Detail,
            GraphMode::Detail => GraphMode::Both,
            GraphMode::Both => GraphMode::Off,
        }
    }

    fn show_list(self) -> bool {
        matches!(self, GraphMode::List | GraphMode::Both)
    }

    fn show_detail(self) -> bool {
        matches!(self, GraphMode::Detail | GraphMode::Both)
    }

    fn label(self) -> &'static str {
        match self {
            GraphMode::Off => "Off",
            GraphMode::List => "List",
            GraphMode::Detail => "Detail",
            GraphMode::Both => "Both",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct NetTotalsSample {
    rx_total: u64,
    tx_total: u64,
    captured_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DetailTarget {
    Command(String),
    TreePid(i32),
}

#[derive(Clone, Debug)]
struct DetailPanel {
    target: DetailTarget,
    title: String,
    summary: String,
    rows: Vec<DetailEntry>,
    trend_key: Option<String>,
}

struct App {
    poll_interval_ms: u64,
    sort_state: SortState,
    lazy_mode: bool,
    graph_mode: GraphMode,
    show_cpu_panel: bool,
    show_mem_panel: bool,
    show_net_panel: bool,
    lazy_window: usize,
    debug_kill: bool,
    last_update: Instant,
    show_help: bool,
    current_processes: Vec<ProcEntry>,
    filtered_indices: Vec<usize>,
    selected_index: Option<usize>,
    filter_state: FilterState,
    detail_targets: Vec<DetailTarget>,
    process_snapshot: Vec<ProcSnapshotEntry>,
    tree_rows: Vec<TreeRow>,
    tree_mode: bool,
    expanded_pids: HashSet<i32>,
    previous_scroll_anchor: Option<usize>,
    metric_history: HashMap<String, MetricHistory>,
    pids_by_command: HashMap<String, Vec<i32>>,
    system_cpu_total_pct: Option<f64>,
    system_mem_used_mb: Option<f64>,
    system_mem_total_mb: Option<f64>,
    system_net_rx_bps: Option<f64>,
    system_net_tx_bps: Option<f64>,
    system_cpu_history: VecDeque<f64>,
    system_mem_history: VecDeque<f64>,
    system_net_history: VecDeque<f64>,
    previous_net_totals: Option<NetTotalsSample>,
    collector_backend: CollectorBackend,
    collector: Box<dyn Collector>,
    kill_prompt: Option<KillPrompt>,
    kill_prompt_error: Option<String>,
}

impl App {
    fn new(collector_backend: CollectorBackend, collector: Box<dyn Collector>) -> Self {
        let now = Instant::now();
        Self {
            poll_interval_ms: 1000,
            sort_state: SortState::default(),
            lazy_mode: true,
            graph_mode: GraphMode::Off,
            show_cpu_panel: false,
            show_mem_panel: false,
            show_net_panel: false,
            lazy_window: 10,
            debug_kill: false,
            last_update: now - Duration::from_millis(1000),
            show_help: false,
            current_processes: Vec::new(),
            filtered_indices: Vec::new(),
            selected_index: None,
            filter_state: FilterState::default(),
            detail_targets: Vec::new(),
            process_snapshot: Vec::new(),
            tree_rows: Vec::new(),
            tree_mode: false,
            expanded_pids: HashSet::new(),
            previous_scroll_anchor: None,
            metric_history: HashMap::new(),
            pids_by_command: HashMap::new(),
            system_cpu_total_pct: None,
            system_mem_used_mb: None,
            system_mem_total_mb: None,
            system_net_rx_bps: None,
            system_net_tx_bps: None,
            system_cpu_history: VecDeque::new(),
            system_mem_history: VecDeque::new(),
            system_net_history: VecDeque::new(),
            previous_net_totals: None,
            collector_backend,
            collector,
            kill_prompt: None,
            kill_prompt_error: None,
        }
    }

    fn set_lazy_window(&mut self, size: usize) {
        let bounded = size.clamp(1, 50);
        if bounded == self.lazy_window {
            return;
        }
        self.lazy_window = bounded;
        for history in self.metric_history.values_mut() {
            shrink_deque(&mut history.cpu, bounded);
            shrink_deque(&mut history.mem, bounded);
        }
        shrink_deque(&mut self.system_cpu_history, bounded);
        shrink_deque(&mut self.system_mem_history, bounded);
        shrink_deque(&mut self.system_net_history, bounded);
    }

    fn change_sort(&mut self, sort: SortBy) {
        if self.sort_state.by != sort {
            self.sort_state.by = sort;
            if self.tree_mode {
                self.rebuild_tree_rows();
            }
            self.last_update = Instant::now() - Duration::from_millis(self.poll_interval_ms);
        }
    }

    fn maybe_update_processes(&mut self) {
        if self.last_update.elapsed() >= Duration::from_millis(self.poll_interval_ms) {
            self.current_processes = self.get_processes();
            self.prune_detail_targets();
            self.update_system_stats();
            if self.tree_mode {
                self.rebuild_tree_rows();
            }
            self.last_update = Instant::now();
        }
    }

    fn get_processes(&mut self) -> Vec<ProcEntry> {
        let Ok(processes) = self.collector.collect_processes() else {
            self.pids_by_command.clear();
            self.process_snapshot.clear();
            return Vec::new();
        };

        let mut grouped: HashMap<String, (f64, f64, usize, Vec<i32>)> = HashMap::new();
        let mut snapshot = Vec::with_capacity(processes.len());
        for process in processes {
            let entry = grouped
                .entry(process.name.clone())
                .or_insert((0.0, 0.0, 0, Vec::new()));
            entry.0 += process.cpu;
            entry.1 += process.mem_mb;
            entry.2 += 1;
            entry.3.push(process.pid);
            snapshot.push(ProcSnapshotEntry {
                pid: process.pid,
                ppid: process.ppid,
                name: process.name,
                cpu: process.cpu,
                mem_mb: process.mem_mb,
            });
        }
        self.process_snapshot = snapshot;

        let mut refreshed_pids_by_command: HashMap<String, Vec<i32>> = HashMap::new();
        let mut proc_list: Vec<ProcEntry> = grouped
            .into_iter()
            .map(|(name, (cpu, mem_mb, count, pids))| {
                refreshed_pids_by_command.insert(name.clone(), pids);
                ProcEntry {
                    name,
                    cpu,
                    mem_mb,
                    count,
                    cpu_lazy: cpu,
                    mem_lazy: mem_mb,
                }
            })
            .collect();
        self.pids_by_command = refreshed_pids_by_command;

        let mut present: HashMap<String, ()> = HashMap::new();
        for proc in &mut proc_list {
            present.insert(proc.name.clone(), ());
            let history = self
                .metric_history
                .entry(proc.name.clone())
                .or_insert_with(|| MetricHistory {
                    cpu: VecDeque::with_capacity(self.lazy_window),
                    mem: VecDeque::with_capacity(self.lazy_window),
                });
            shrink_deque(&mut history.cpu, self.lazy_window);
            shrink_deque(&mut history.mem, self.lazy_window);
            history.cpu.push_back(proc.cpu);
            history.mem.push_back(proc.mem_mb);
            while history.cpu.len() > self.lazy_window {
                history.cpu.pop_front();
            }
            while history.mem.len() > self.lazy_window {
                history.mem.pop_front();
            }
            proc.cpu_lazy = median(&history.cpu).unwrap_or(proc.cpu);
            proc.mem_lazy = median(&history.mem).unwrap_or(proc.mem_mb);
        }

        self.metric_history
            .retain(|name, _| present.contains_key(name));

        match self.sort_state.by {
            SortBy::Cpu => {
                if self.lazy_mode {
                    proc_list.sort_by(|a, b| a.cpu_lazy.total_cmp(&b.cpu_lazy));
                } else {
                    proc_list.sort_by(|a, b| a.cpu.total_cmp(&b.cpu));
                }
            }
            SortBy::Mem => {
                if self.lazy_mode {
                    proc_list.sort_by(|a, b| a.mem_lazy.total_cmp(&b.mem_lazy));
                } else {
                    proc_list.sort_by(|a, b| a.mem_mb.total_cmp(&b.mem_mb));
                }
            }
            SortBy::Name => {
                proc_list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            }
        }
        if self.sort_state.descending {
            proc_list.reverse();
        }
        proc_list
    }

    fn update_system_stats(&mut self) {
        let Ok(stats) = self.collector.collect_system_stats() else {
            return;
        };
        let Some(stats) = stats else {
            return;
        };

        self.system_cpu_total_pct = stats.cpu_total_pct;
        self.system_mem_used_mb = stats.mem_used_mb;
        self.system_mem_total_mb = stats.mem_total_mb;

        if let Some(cpu) = stats.cpu_total_pct {
            push_bounded_sample(&mut self.system_cpu_history, cpu, self.lazy_window);
        }
        if let (Some(used), Some(total)) = (stats.mem_used_mb, stats.mem_total_mb)
            && total > 0.0
        {
            let used_pct = (used / total) * 100.0;
            push_bounded_sample(&mut self.system_mem_history, used_pct, self.lazy_window);
        }

        let now = Instant::now();
        if let (Some(rx_total), Some(tx_total)) = (stats.net_rx_bytes_total, stats.net_tx_bytes_total)
        {
            if let Some(previous) = self.previous_net_totals {
                let elapsed = now.duration_since(previous.captured_at).as_secs_f64();
                if elapsed > 0.0 {
                    let rx_bps = (rx_total.saturating_sub(previous.rx_total) as f64) / elapsed;
                    let tx_bps = (tx_total.saturating_sub(previous.tx_total) as f64) / elapsed;
                    self.system_net_rx_bps = Some(rx_bps);
                    self.system_net_tx_bps = Some(tx_bps);
                    push_bounded_sample(
                        &mut self.system_net_history,
                        rx_bps + tx_bps,
                        self.lazy_window,
                    );
                }
            }
            self.previous_net_totals = Some(NetTotalsSample {
                rx_total,
                tx_total,
                captured_at: now,
            });
        }
    }

    fn update_filter_and_selection(&mut self) {
        if self.tree_mode {
            if self.tree_rows.is_empty() {
                self.selected_index = None;
                self.previous_scroll_anchor = None;
            } else if let Some(idx) = self.selected_index {
                self.selected_index = Some(idx.min(self.tree_rows.len() - 1));
            } else {
                self.selected_index = Some(0);
            }
            return;
        }

        let query = self.filter_state.search_query.to_lowercase();
        self.filtered_indices.clear();
        for (idx, proc) in self.current_processes.iter().enumerate() {
            if query.is_empty() || proc.name.to_lowercase().contains(&query) {
                self.filtered_indices.push(idx);
            }
        }

        if self.filtered_indices.is_empty() {
            self.selected_index = None;
            self.previous_scroll_anchor = None;
            return;
        }

        if let Some(idx) = self.selected_index {
            self.selected_index = Some(idx.min(self.filtered_indices.len() - 1));
        }
    }

    fn selected_process(&self) -> Option<&ProcEntry> {
        let pos = self.selected_index?;
        let real_idx = *self.filtered_indices.get(pos)?;
        self.current_processes.get(real_idx)
    }

    fn move_selection(&mut self, delta: isize, visible_rows: usize, page_mode: bool) {
        let len = if self.tree_mode {
            self.tree_rows.len()
        } else {
            self.filtered_indices.len()
        };
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

    fn clear_search_and_selection(&mut self) {
        self.filter_state.search_mode = false;
        self.filter_state.search_query.clear();
        self.selected_index = None;
        self.previous_scroll_anchor = None;
    }

    fn selected_detail_target(&self) -> Option<DetailTarget> {
        if self.tree_mode {
            let idx = self.selected_index?;
            let row = self.tree_rows.get(idx)?;
            Some(DetailTarget::TreePid(row.pid))
        } else {
            let proc = self.selected_process()?;
            Some(DetailTarget::Command(proc.name.clone()))
        }
    }

    fn is_detail_target_open(&self, target: &DetailTarget) -> bool {
        self.detail_targets.iter().any(|existing| existing == target)
    }

    fn toggle_detail_target(&mut self, target: DetailTarget) {
        if let Some(pos) = self.detail_targets.iter().position(|existing| *existing == target) {
            self.detail_targets.remove(pos);
        } else {
            self.detail_targets.push(target);
        }
    }

    fn toggle_extended_selected(&mut self) {
        if let Some(target) = self.selected_detail_target() {
            self.toggle_detail_target(target);
        }
    }

    fn is_command_detail_open(&self, command: &str) -> bool {
        self.detail_targets
            .iter()
            .any(|target| matches!(target, DetailTarget::Command(name) if name == command))
    }

    fn build_detail_panels(&self) -> Vec<DetailPanel> {
        let mut panels = Vec::new();
        for target in &self.detail_targets {
            let panel = match target {
                DetailTarget::Command(command) => self.build_command_detail_panel(command),
                DetailTarget::TreePid(pid) => self.build_tree_detail_panel(*pid),
            };
            if let Some(panel) = panel {
                panels.push(panel);
            }
        }
        panels
    }

    fn prune_detail_targets(&mut self) {
        let live_pids: HashSet<i32> = self.process_snapshot.iter().map(|proc| proc.pid).collect();
        let live_commands: HashSet<&str> = self
            .process_snapshot
            .iter()
            .map(|proc| proc.name.as_str())
            .collect();
        self.detail_targets.retain(|target| match target {
            DetailTarget::Command(command) => live_commands.contains(command.as_str()),
            DetailTarget::TreePid(pid) => live_pids.contains(pid),
        });
    }

    fn build_command_detail_panel(&self, command: &str) -> Option<DetailPanel> {
        let mut rows: Vec<DetailEntry> = self
            .process_snapshot
            .iter()
            .filter(|proc| proc.name == command)
            .map(|proc| DetailEntry {
                pid: proc.pid,
                cpu: proc.cpu,
                mem_mb: proc.mem_mb,
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let total_cpu = rows.iter().map(|r| r.cpu).sum::<f64>();
        let total_mem = rows.iter().map(|r| r.mem_mb).sum::<f64>();
        Some(DetailPanel {
            target: DetailTarget::Command(command.to_string()),
            title: format!("Command: {command}"),
            summary: format!(
                "{} PIDs  Total CPU {:>6.2}%  Total MEM {:>8.2} MB",
                rows.len(),
                total_cpu,
                total_mem
            ),
            rows,
            trend_key: Some(command.to_string()),
        })
    }

    fn build_tree_detail_panel(&self, root_pid: i32) -> Option<DetailPanel> {
        let by_pid: HashMap<i32, &ProcSnapshotEntry> =
            self.process_snapshot.iter().map(|proc| (proc.pid, proc)).collect();
        let root = by_pid.get(&root_pid)?;

        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for proc in &self.process_snapshot {
            children.entry(proc.ppid).or_default().push(proc.pid);
        }

        let mut stack = vec![root_pid];
        let mut visited = HashSet::new();
        let mut rows = Vec::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            if let Some(proc) = by_pid.get(&pid) {
                rows.push(DetailEntry {
                    pid: proc.pid,
                    cpu: proc.cpu,
                    mem_mb: proc.mem_mb,
                });
                if let Some(child_ids) = children.get(&pid) {
                    for child in child_ids {
                        stack.push(*child);
                    }
                }
            }
        }
        rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let total_cpu = rows.iter().map(|r| r.cpu).sum::<f64>();
        let total_mem = rows.iter().map(|r| r.mem_mb).sum::<f64>();
        Some(DetailPanel {
            target: DetailTarget::TreePid(root_pid),
            title: format!("Tree: {} ({root_pid})", root.name),
            summary: format!(
                "{} processes  Total CPU {:>6.2}%  Total MEM {:>8.2} MB",
                rows.len(),
                total_cpu,
                total_mem
            ),
            rows,
            trend_key: None,
        })
    }

    fn get_pids_for_command(&self, command: &str) -> Vec<i32> {
        if command.is_empty() {
            return Vec::new();
        }
        if let Some(pids) = self.pids_by_command.get(command)
            && !pids.is_empty()
        {
            return pids.clone();
        }
        self.process_snapshot
            .iter()
            .filter(|proc| proc.name == command)
            .map(|proc| proc.pid)
            .collect()
    }

    fn handle_key(&mut self, key: KeyCode, _modifiers: KeyModifiers, visible_rows: usize) -> bool {
        if self.kill_prompt.is_some() {
            return self.handle_kill_prompt_input(key);
        }

        if self.filter_state.search_mode {
            return self.handle_search_input(key);
        }

        match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => return true,
            KeyCode::Esc | KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.poll_interval_ms = min(10_000, self.poll_interval_ms + 100);
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                self.poll_interval_ms = max(100, self.poll_interval_ms.saturating_sub(100));
            }
            KeyCode::Char('x') | KeyCode::Char('X') => {
                self.lazy_mode = !self.lazy_mode;
            }
            KeyCode::Char('g') | KeyCode::Char('G') => {
                self.graph_mode = self.graph_mode.cycle();
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                self.tree_mode = !self.tree_mode;
                self.previous_scroll_anchor = None;
                if self.tree_mode {
                    self.filter_state.search_mode = false;
                    self.filter_state.search_query.clear();
                    self.rebuild_tree_rows();
                    self.selected_index = if self.tree_rows.is_empty() { None } else { Some(0) };
                } else {
                    self.selected_index = None;
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.sort_state.descending = !self.sort_state.descending;
                if self.tree_mode {
                    self.rebuild_tree_rows();
                }
                self.last_update = Instant::now() - Duration::from_millis(self.poll_interval_ms);
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                if self.tree_mode {
                    return false;
                }
                if let Some(proc) = self.selected_process() {
                    let command = proc.name.clone();
                    let pids = self.get_pids_for_command(&command);
                    let debug_summary = if self.debug_kill {
                        Some(format!(
                            "debug: selected='{}' resolved_pids={}",
                            command,
                            pids.len()
                        ))
                    } else {
                        None
                    };
                    self.kill_prompt = Some(KillPrompt {
                        command,
                        pids: pids.clone(),
                        debug_summary,
                    });
                    self.kill_prompt_error = if pids.is_empty() {
                        Some("No PIDs found".to_string())
                    } else {
                        None
                    };
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.debug_kill = !self.debug_kill;
            }
            KeyCode::Char('1') => {
                self.show_cpu_panel = !self.show_cpu_panel;
            }
            KeyCode::Char('2') => {
                self.show_mem_panel = !self.show_mem_panel;
            }
            KeyCode::Char('3') => {
                self.show_net_panel = !self.show_net_panel;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => self.change_sort(SortBy::Cpu),
            KeyCode::Char('m') | KeyCode::Char('M') => self.change_sort(SortBy::Mem),
            KeyCode::Char('p') | KeyCode::Char('P') => self.change_sort(SortBy::Name),
            KeyCode::Left => {
                let next = match self.sort_state.by {
                    SortBy::Cpu => SortBy::Name,
                    SortBy::Mem => SortBy::Cpu,
                    SortBy::Name => SortBy::Mem,
                };
                self.change_sort(next);
            }
            KeyCode::Right => {
                let next = match self.sort_state.by {
                    SortBy::Cpu => SortBy::Mem,
                    SortBy::Mem => SortBy::Name,
                    SortBy::Name => SortBy::Cpu,
                };
                self.change_sort(next);
            }
            KeyCode::Char('.') => self.set_lazy_window(self.lazy_window + 1),
            KeyCode::Char(',') => self.set_lazy_window(self.lazy_window.saturating_sub(1)),
            KeyCode::PageDown => self.move_selection(1, visible_rows, true),
            KeyCode::PageUp => self.move_selection(-1, visible_rows, true),
            KeyCode::Down => self.move_selection(1, visible_rows, false),
            KeyCode::Up => self.move_selection(-1, visible_rows, false),
            KeyCode::Enter => self.toggle_extended_selected(),
            KeyCode::Char(' ') => {
                if self.tree_mode {
                    self.toggle_tree_selected();
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.filter_state.search_mode = true;
            }
            _ => {}
        }
        false
    }

    fn handle_search_input(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Esc => self.clear_search_and_selection(),
            KeyCode::Enter => {
                self.filter_state.search_mode = false;
            }
            KeyCode::Backspace => {
                self.filter_state.search_query.pop();
                self.selected_index = None;
                self.previous_scroll_anchor = None;
            }
            KeyCode::Char(c) => {
                if is_search_input_char(c) {
                    self.filter_state.search_query.push(c);
                    self.selected_index = None;
                    self.previous_scroll_anchor = None;
                }
            }
            KeyCode::Down => self.move_selection(1, 1, false),
            KeyCode::Up => self.move_selection(-1, 1, false),
            _ => {}
        }
        false
    }

    fn handle_kill_prompt_input(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Esc => {
                self.kill_prompt = None;
                self.kill_prompt_error = None;
            }
            KeyCode::Enter => {
                if let Some(prompt) = self.kill_prompt.as_ref() {
                    let mut errors = Vec::new();
                    for pid in &prompt.pids {
                        let rc = unsafe { kill(*pid, SIGKILL) };
                        if rc != 0 {
                            let err = io::Error::last_os_error();
                            errors.push(err.to_string());
                        }
                    }
                    if let Some(first) = errors.first() {
                        self.kill_prompt_error = Some(first.clone());
                    } else {
                        self.kill_prompt = None;
                        self.kill_prompt_error = None;
                        self.last_update =
                            Instant::now() - Duration::from_millis(self.poll_interval_ms);
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn rebuild_tree_rows(&mut self) {
        let existing: HashSet<i32> = self.process_snapshot.iter().map(|p| p.pid).collect();
        self.expanded_pids.retain(|pid| existing.contains(pid));

        let mut by_pid: HashMap<i32, &ProcSnapshotEntry> = HashMap::new();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        for proc in &self.process_snapshot {
            by_pid.insert(proc.pid, proc);
            children.entry(proc.ppid).or_default().push(proc.pid);
        }

        for child_list in children.values_mut() {
            child_list.sort_by(|a, b| self.compare_snapshot_pids(*a, *b, &by_pid));
        }

        let mut roots = children.get(&1).cloned().unwrap_or_default();
        for proc in &self.process_snapshot {
            if proc.pid == 1 {
                continue;
            }
            if proc.ppid != 1 && !by_pid.contains_key(&proc.ppid) {
                roots.push(proc.pid);
            }
        }
        roots.sort();
        roots.dedup();
        roots.sort_by(|a, b| self.compare_snapshot_pids(*a, *b, &by_pid));

        let selected_pid = self
            .selected_index
            .and_then(|idx| self.tree_rows.get(idx).map(|row| row.pid));

        let mut rows = Vec::new();
        let mut visited = HashSet::new();
        for pid in roots {
            self.append_tree_row(pid, 0, &by_pid, &children, &mut visited, &mut rows);
        }
        self.tree_rows = rows;

        if self.tree_rows.is_empty() {
            self.selected_index = None;
        } else if let Some(pid) = selected_pid {
            self.selected_index = self.tree_rows.iter().position(|row| row.pid == pid).or(Some(0));
        } else {
            self.selected_index = Some(0);
        }
    }

    fn compare_snapshot_pids(
        &self,
        a_pid: i32,
        b_pid: i32,
        by_pid: &HashMap<i32, &ProcSnapshotEntry>,
    ) -> Ordering {
        let a = by_pid.get(&a_pid);
        let b = by_pid.get(&b_pid);
        let cmp = match (a, b) {
            (Some(a), Some(b)) => {
                let name_cmp = a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.pid.cmp(&b.pid));
                match self.sort_state.by {
                    SortBy::Cpu => a
                        .cpu
                        .total_cmp(&b.cpu)
                        .then_with(|| a.mem_mb.total_cmp(&b.mem_mb))
                        .then(name_cmp),
                    SortBy::Mem => a
                        .mem_mb
                        .total_cmp(&b.mem_mb)
                        .then_with(|| a.cpu.total_cmp(&b.cpu))
                        .then(name_cmp),
                    SortBy::Name => name_cmp,
                }
            }
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };

        let cmp = if self.sort_state.descending { cmp.reverse() } else { cmp };
        cmp.then_with(|| a_pid.cmp(&b_pid))
    }

    fn append_tree_row(
        &self,
        pid: i32,
        depth: usize,
        by_pid: &HashMap<i32, &ProcSnapshotEntry>,
        children: &HashMap<i32, Vec<i32>>,
        visited: &mut HashSet<i32>,
        rows: &mut Vec<TreeRow>,
    ) {
        if visited.contains(&pid) {
            return;
        }
        let Some(proc) = by_pid.get(&pid) else {
            return;
        };
        visited.insert(pid);

        let child_ids = children.get(&pid).cloned().unwrap_or_default();
        let has_children = !child_ids.is_empty();
        let expanded = self.expanded_pids.contains(&pid);
        rows.push(TreeRow {
            pid,
            name: proc.name.clone(),
            cpu: proc.cpu,
            mem_mb: proc.mem_mb,
            depth,
            has_children,
            expanded,
        });

        if has_children && expanded {
            for child_pid in child_ids {
                self.append_tree_row(child_pid, depth + 1, by_pid, children, visited, rows);
            }
        }
    }

    fn toggle_tree_selected(&mut self) {
        let Some(idx) = self.selected_index else {
            return;
        };
        let Some(row) = self.tree_rows.get(idx) else {
            return;
        };
        if !row.has_children {
            return;
        }
        let pid = row.pid;
        if self.expanded_pids.contains(&pid) {
            self.expanded_pids.remove(&pid);
        } else {
            self.expanded_pids.insert(pid);
        }
        self.rebuild_tree_rows();
    }
}

fn push_bounded_sample(history: &mut VecDeque<f64>, value: f64, max_len: usize) {
    history.push_back(value);
    while history.len() > max_len {
        history.pop_front();
    }
}

fn parse_collector_override(args: &[String]) -> Result<Option<String>, String> {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--collector" {
            let Some(value) = iter.next() else {
                return Err("--collector expects a backend value".to_string());
            };
            return Ok(Some(value.to_string()));
        }
        if let Some(value) = arg.strip_prefix("--collector=") {
            if value.is_empty() {
                return Err("--collector expects a backend value".to_string());
            }
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let collector_override = parse_collector_override(&args)
        .map_err(io::Error::other)?;
    let (collector_backend, collector) =
        build_collector(collector_override.as_deref()).map_err(io::Error::other)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(collector_backend, collector);
    let tick_rate = Duration::from_millis(10);
    let mut should_quit = false;

    while !should_quit {
        app.maybe_update_processes();
        app.update_filter_and_selection();

        let mut visible_rows = 0usize;
        terminal.draw(|f| {
            visible_rows = draw_ui(f, &mut app);
        })?;

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

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw_ui(f: &mut ratatui::Frame<'_>, app: &mut App) -> usize {
    let area = f.area();
    let mut lines_before_header = 0;
    let search_visible = !app.tree_mode && (app.filter_state.search_mode || !app.filter_state.search_query.is_empty());

    if search_visible {
        let mut query = app.filter_state.search_query.clone();
        if app.filter_state.search_mode {
            query.push('_');
        }
        let line = Line::from(vec![
            Span::styled(
                "Search: ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(query),
        ]);
        let search_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(line), search_area);
        lines_before_header = 1;
    }

    let mut content_area = Rect {
        x: area.x,
        y: area.y + lines_before_header,
        width: area.width,
        height: area.height.saturating_sub(lines_before_header),
    };

    let panel_count = [app.show_cpu_panel, app.show_mem_panel, app.show_net_panel]
        .into_iter()
        .filter(|enabled| *enabled)
        .count() as u16;
    if panel_count > 0 && content_area.height > 9 {
        let panel_height = min(7, content_area.height.saturating_sub(3));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(panel_height), Constraint::Min(1)])
            .split(content_area);
        render_system_panels(f, app, chunks[0], panel_count);
        content_area = chunks[1];
    }

    let value_width: u16 = if app.lazy_mode { 12 } else { 10 };
    let graph_width: u16 = 8;
    let list_graphs_enabled = app.graph_mode.show_list();
    let detail_graphs_enabled = app.graph_mode.show_detail();
    let graph_columns_visible = !app.tree_mode
        && list_graphs_enabled
        && content_area.width > (value_width * 2 + graph_width * 2 + 6);
    let min_name = 1u16;
    let fixed_width = value_width * 2 + 2 + if graph_columns_visible { graph_width * 2 + 2 } else { 0 };
    let name_col_width = if content_area.width > fixed_width {
        content_area.width - fixed_width
    } else {
        min_name
    };

    let list_top = content_area.y;
    let available_height = content_area.height;
    let detail_panels = app.build_detail_panels();
    let detail_active = !detail_panels.is_empty() && available_height > 1;
    let mut detail_height = 0u16;
    if detail_active {
        let per_panel = if detail_graphs_enabled { 7u16 } else { 5u16 };
        let requested = max(5, (detail_panels.len() as u16).saturating_mul(per_panel)) as u16;
        let max_height = available_height.saturating_sub(2);
        detail_height = min(requested, max_height);
    }

    let visible_rows = if detail_height > 0 {
        available_height.saturating_sub(detail_height + 1)
    } else {
        available_height
    }
    .saturating_sub(1) as usize;

    let selectable_len = if app.tree_mode {
        app.tree_rows.len()
    } else {
        app.filtered_indices.len()
    };
    let selected_exists = selectable_len > 0 && app.selected_index.is_some();
    let mut start_index = 0usize;
    if selected_exists && visible_rows > 0 {
        let mut anchor = app.previous_scroll_anchor.unwrap_or(0);
        let selected = app.selected_index.unwrap_or(0);
        if selected < anchor {
            anchor = selected;
        } else if selected >= anchor + visible_rows {
            anchor = selected + 1 - visible_rows;
        }
        let max_start = selectable_len.saturating_sub(visible_rows);
        anchor = anchor.min(max_start);
        app.previous_scroll_anchor = Some(anchor);
        start_index = anchor;
    } else if visible_rows > 0 {
        app.previous_scroll_anchor = Some(0);
    }

    let direction_icon = if app.sort_state.descending { "↓" } else { "↑" };
    let (mut name_header, mut cpu_header, mut mem_header) = if app.tree_mode {
        (
            "PROCESS TREE".to_string(),
            "CPU%".to_string(),
            "MEM(MB)".to_string(),
        )
    } else {
        (
            "PROCESS NAME".to_string(),
            "CPU%".to_string(),
            "MEM(MB)".to_string(),
        )
    };
    if !app.tree_mode {
        match app.sort_state.by {
            SortBy::Name => name_header = format!("{direction_icon} {name_header}"),
            SortBy::Cpu => cpu_header = format!("{direction_icon} {cpu_header}"),
            SortBy::Mem => mem_header = format!("{direction_icon} {mem_header}"),
        }
        if app.lazy_mode {
            cpu_header.push_str(&format!(" x̄({})", app.lazy_window));
            mem_header.push_str(&format!(" x̄({})", app.lazy_window));
        }
    }

    let mut header_cells = vec![
        Cell::from(Span::styled(
            truncate_to_width(&name_header, name_col_width as usize),
            sort_header_style(app.sort_state.by == SortBy::Name),
        )),
        Cell::from(Span::styled(
            truncate_to_width(&cpu_header, value_width as usize),
            sort_header_style(app.sort_state.by == SortBy::Cpu),
        )),
        Cell::from(Span::styled(
            truncate_to_width(&mem_header, value_width as usize),
            sort_header_style(app.sort_state.by == SortBy::Mem),
        )),
    ];
    if graph_columns_visible {
        header_cells.push(Cell::from(Span::styled(
            "CPU ▄",
            sort_header_style(false),
        )));
        header_cells.push(Cell::from(Span::styled(
            "MEM ▄",
            sort_header_style(false),
        )));
    }
    let header = Row::new(header_cells).height(1);

    let mut rows = Vec::with_capacity(visible_rows);
    for row_i in 0..visible_rows {
        let list_index = start_index + row_i;
        if app.tree_mode {
            if list_index >= app.tree_rows.len() {
                break;
            }
            if let Some(proc) = app.tree_rows.get(list_index) {
                let marker = if proc.has_children {
                    if proc.expanded {
                        "▾ "
                    } else {
                        "▸ "
                    }
                } else {
                    "  "
                };
                let indent = "  ".repeat(proc.depth);
                let display_name = format!("{indent}{marker}{} ({})", proc.name, proc.pid);

                let mut style = Style::default();
                if proc.cpu > 50.0 {
                    style = style.fg(Color::Red);
                } else if proc.cpu > 20.0 {
                    style = style.fg(Color::Yellow);
                }
                if app.selected_index == Some(list_index) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if app.is_detail_target_open(&DetailTarget::TreePid(proc.pid)) {
                    style = style.add_modifier(Modifier::BOLD);
                }

                rows.push(
                    Row::new(vec![
                        Cell::from(truncate_to_width(&display_name, name_col_width as usize)),
                        Cell::from(format!("{:>width$.2}", proc.cpu, width = value_width as usize)),
                        Cell::from(format!("{:>width$.2}", proc.mem_mb, width = value_width as usize)),
                    ])
                    .style(style),
                );
            }
        } else {
            if list_index >= app.filtered_indices.len() {
                break;
            }
            let real_index = app.filtered_indices[list_index];
            if let Some(proc) = app.current_processes.get(real_index) {
                let mut display_name = proc.name.clone();
                if proc.count > 1 {
                    display_name.push_str(&format!(" ({})", proc.count));
                }
                let cpu = if app.lazy_mode {
                    proc.cpu_lazy
                } else {
                    proc.cpu
                };
                let mem = if app.lazy_mode {
                    proc.mem_lazy
                } else {
                    proc.mem_mb
                };

                let mut style = Style::default();
                if cpu > 50.0 {
                    style = style.fg(Color::Red);
                } else if cpu > 20.0 {
                    style = style.fg(Color::Yellow);
                }

                if app.selected_index == Some(list_index)
                    || app.is_command_detail_open(&proc.name)
                {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let mut cells = vec![
                    Cell::from(truncate_to_width(&display_name, name_col_width as usize)),
                    Cell::from(format!("{:>width$.2}", cpu, width = value_width as usize)),
                    Cell::from(format!("{:>width$.2}", mem, width = value_width as usize)),
                ];
                if graph_columns_visible {
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
        }
    }

    let table_height = visible_rows as u16 + 1;
    let table_area = Rect {
        x: content_area.x,
        y: list_top,
        width: content_area.width,
        height: min(table_height, content_area.height),
    };

    let mut constraints = vec![
        Constraint::Length(name_col_width),
        Constraint::Length(value_width),
        Constraint::Length(value_width),
    ];
    if graph_columns_visible {
        constraints.push(Constraint::Length(graph_width));
        constraints.push(Constraint::Length(graph_width));
    }

    let table = Table::new(rows, constraints)
    .header(header)
    .column_spacing(1);
    f.render_widget(table, table_area);

    if detail_height > 0 {
        let separator_y = list_top + table_height;
        if separator_y < content_area.y + content_area.height {
            let sep = Paragraph::new("─".repeat(content_area.width as usize));
            let sep_area = Rect {
                x: content_area.x,
                y: separator_y,
                width: content_area.width,
                height: 1,
            };
            f.render_widget(sep, sep_area);
        }

        let detail_area = Rect {
            x: content_area.x,
            y: separator_y + 1,
            width: content_area.width,
            height: detail_height,
        };
        render_detail_panels(f, app, detail_area, &detail_panels, detail_graphs_enabled);
    }

    if app.show_help {
        let mut sort_label = app.sort_state.by.label().to_string();
        if app.lazy_mode {
            sort_label.push_str(" average (x̄)");
        }
        let help_data = HelpModalData {
            poll_interval_ms: app.poll_interval_ms,
            mode_label: if app.tree_mode { "Tree" } else { "List" }.to_string(),
            backend_label: app.collector_backend.as_str().to_string(),
            sort_label,
            lazy_window: app.lazy_window,
            kill_debug_enabled: app.debug_kill,
            graph_mode: app.graph_mode.label().to_string(),
        };
        render_help_modal(f, &help_data);
    }
    if let Some(prompt) = app.kill_prompt.as_ref() {
        let kill_data = KillModalData {
            command: &prompt.command,
            pids: &prompt.pids,
            debug_summary: prompt.debug_summary.as_deref(),
            error: app.kill_prompt_error.as_deref(),
        };
        render_kill_modal(f, &kill_data);
    }

    visible_rows
}

fn render_detail_panels(
    f: &mut ratatui::Frame<'_>,
    app: &App,
    area: Rect,
    panels: &[DetailPanel],
    detail_graphs_enabled: bool,
) {
    if panels.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let max_lines = area.height as usize;

    for (i, panel) in panels.iter().enumerate() {
        if lines.len() >= max_lines {
            break;
        }
        if i > 0
            && lines.len() < max_lines
        {
            lines.push(Line::from(String::new()));
        }

        let label = match panel.target {
            DetailTarget::Command(_) => "PID Group",
            DetailTarget::TreePid(_) => "Tree",
        };
        if lines.len() < max_lines {
            lines.push(Line::from(Span::styled(
                format!("{label}: {}", panel.title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        if lines.len() < max_lines {
            lines.push(Line::from(panel.summary.clone()));
        }

        if detail_graphs_enabled
            && let Some(key) = panel.trend_key.as_ref()
        {
            let graph_width = area.width.saturating_sub(14) as usize;
            let (cpu_graph, mem_graph) = app
                .metric_history
                .get(key)
                .map(|history| {
                    (
                        sparkline(&history.cpu, graph_width),
                        sparkline(&history.mem, graph_width),
                    )
                })
                .unwrap_or_else(|| (" ".repeat(graph_width), " ".repeat(graph_width)));
            if lines.len() < max_lines {
                lines.push(Line::from(format!("CPU trend: {cpu_graph}")));
            }
            if lines.len() < max_lines {
                lines.push(Line::from(format!("MEM trend: {mem_graph}")));
            }
        }

        if lines.len() < max_lines {
            lines.push(Line::from(Span::styled(
                "PID       CPU%      MEM(MB)",
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
        for detail in panel.rows.iter().take(5) {
            if lines.len() >= max_lines {
                break;
            }
            lines.push(Line::from(format!(
                "{:>6}   {:>6.2}     {:>7.2}",
                detail.pid, detail.cpu, detail.mem_mb
            )));
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn render_system_panels(f: &mut ratatui::Frame<'_>, app: &App, area: Rect, panel_count: u16) {
    if panel_count == 0 || area.width == 0 || area.height == 0 {
        return;
    }

    let constraints = vec![Constraint::Percentage(100 / panel_count); panel_count as usize];
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut next_idx = 0usize;
    if app.show_cpu_panel {
        render_cpu_panel(f, chunks[next_idx], app);
        next_idx += 1;
    }
    if app.show_mem_panel {
        render_mem_panel(f, chunks[next_idx], app);
        next_idx += 1;
    }
    if app.show_net_panel {
        render_net_panel(f, chunks[next_idx], app);
    }
}

fn render_cpu_panel(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title("CPU");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let value = app
        .system_cpu_total_pct
        .map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| "n/a".to_string());
    let graph_width = inner.width.saturating_sub(2) as usize;
    let graph = sparkline(&app.system_cpu_history, graph_width);
    let lines = vec![
        Line::from(format!("Total: {value}")),
        Line::from(graph),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_mem_panel(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title("MEM");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let summary = match (app.system_mem_used_mb, app.system_mem_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => {
            format!("Used: {:.1}/{:.1} GB ({:.1}%)", used / 1024.0, total / 1024.0, (used / total) * 100.0)
        }
        _ => "Used: n/a".to_string(),
    };
    let graph_width = inner.width.saturating_sub(2) as usize;
    let graph = sparkline(&app.system_mem_history, graph_width);
    let lines = vec![
        Line::from(summary),
        Line::from(graph),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_net_panel(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title("NET");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rx = app
        .system_net_rx_bps
        .map(format_bytes_per_second)
        .unwrap_or_else(|| "n/a".to_string());
    let tx = app
        .system_net_tx_bps
        .map(format_bytes_per_second)
        .unwrap_or_else(|| "n/a".to_string());
    let total = match (app.system_net_rx_bps, app.system_net_tx_bps) {
        (Some(rx), Some(tx)) => format_bytes_per_second(rx + tx),
        _ => "n/a".to_string(),
    };
    let graph_width = inner.width.saturating_sub(2) as usize;
    let graph = sparkline(&app.system_net_history, graph_width);
    let lines = vec![
        Line::from(format!("RX: {rx}  TX: {tx}")),
        Line::from(format!("Total: {total}")),
        Line::from(graph),
    ];
    f.render_widget(Paragraph::new(lines), inner);
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

#[cfg(test)]
mod tests {
    use super::{App, CollectorBackend, DetailTarget, SortBy};
    use crossterm::event::{KeyCode, KeyModifiers};
    use dtop_collector::{Collector, ProcessRecord};
    use std::io;

    struct MockCollector {
        rows: Vec<ProcessRecord>,
    }

    impl Collector for MockCollector {
        fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>> {
            Ok(self.rows.clone())
        }
    }

    fn fixture_rows() -> Vec<ProcessRecord> {
        include_str!("../tests/fixtures/mock_collector_snapshot.csv")
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(',');
                let pid = parts.next()?.parse::<i32>().ok()?;
                let ppid = parts.next()?.parse::<i32>().ok()?;
                let cpu = parts.next()?.parse::<f64>().ok()?;
                let mem_mb = parts.next()?.parse::<f64>().ok()?;
                let name = parts.next()?.to_string();
                Some(ProcessRecord {
                    pid,
                    ppid,
                    name,
                    cpu,
                    mem_mb,
                })
            })
            .collect()
    }

    #[test]
    fn groups_collector_fixture_rows_by_command() {
        let collector = Box::new(MockCollector {
            rows: fixture_rows(),
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);
        app.lazy_mode = false;

        let rows = app.get_processes();
        assert_eq!(rows.len(), 2);

        let node = rows
            .iter()
            .find(|row| row.name == "node")
            .expect("node command should exist");
        assert_eq!(node.count, 2);
        assert_eq!(node.cpu, 17.0);
        assert_eq!(node.mem_mb, 150.0);

        assert_eq!(app.process_snapshot.len(), 3);
        assert_eq!(app.pids_by_command.get("node"), Some(&vec![101, 102]));
        assert_eq!(app.pids_by_command.get("bash"), Some(&vec![201]));
    }

    #[test]
    fn builds_command_detail_panel_from_fixture() {
        let collector = Box::new(MockCollector {
            rows: fixture_rows(),
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);
        app.get_processes();

        let panel = app
            .build_command_detail_panel("node")
            .expect("expected command panel");
        assert_eq!(panel.rows.len(), 2);
        assert_eq!(panel.rows[0].pid, 101);
        assert_eq!(panel.rows[1].pid, 102);
        assert!(panel.summary.contains("Total CPU"));
    }

    #[test]
    fn builds_tree_detail_panel_with_subtree_aggregate() {
        let collector = Box::new(MockCollector {
            rows: vec![
                ProcessRecord {
                    pid: 1,
                    ppid: 0,
                    name: "launchd".to_string(),
                    cpu: 1.0,
                    mem_mb: 100.0,
                },
                ProcessRecord {
                    pid: 10,
                    ppid: 1,
                    name: "alpha".to_string(),
                    cpu: 10.0,
                    mem_mb: 10.0,
                },
                ProcessRecord {
                    pid: 11,
                    ppid: 10,
                    name: "child".to_string(),
                    cpu: 2.0,
                    mem_mb: 1.0,
                },
            ],
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);
        app.get_processes();

        let panel = app
            .build_tree_detail_panel(10)
            .expect("expected tree panel");
        assert_eq!(panel.rows.len(), 2);
        assert!(panel.summary.contains("2 processes"));
        assert!(panel.summary.contains("12.00"));
    }

    #[test]
    fn tree_view_sorts_roots_and_siblings_by_active_sort() {
        let collector = Box::new(MockCollector {
            rows: vec![
                ProcessRecord {
                    pid: 10,
                    ppid: 1,
                    name: "alpha".to_string(),
                    cpu: 10.0,
                    mem_mb: 10.0,
                },
                ProcessRecord {
                    pid: 20,
                    ppid: 1,
                    name: "bravo".to_string(),
                    cpu: 30.0,
                    mem_mb: 20.0,
                },
                ProcessRecord {
                    pid: 11,
                    ppid: 10,
                    name: "alpha-child-1".to_string(),
                    cpu: 2.0,
                    mem_mb: 1.0,
                },
                ProcessRecord {
                    pid: 12,
                    ppid: 10,
                    name: "alpha-child-2".to_string(),
                    cpu: 5.0,
                    mem_mb: 1.0,
                },
            ],
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);
        app.get_processes();
        app.tree_mode = true;
        app.expanded_pids.insert(10);

        app.sort_state.by = SortBy::Cpu;
        app.sort_state.descending = true;
        app.rebuild_tree_rows();
        let descending: Vec<i32> = app.tree_rows.iter().map(|r| r.pid).collect();
        assert_eq!(descending, vec![20, 10, 12, 11]);

        app.sort_state.descending = false;
        app.rebuild_tree_rows();
        let ascending: Vec<i32> = app.tree_rows.iter().map(|r| r.pid).collect();
        assert_eq!(ascending, vec![10, 11, 12, 20]);
    }

    #[test]
    fn space_toggles_expansion_in_tree_view() {
        let collector = Box::new(MockCollector {
            rows: vec![
                ProcessRecord {
                    pid: 10,
                    ppid: 1,
                    name: "alpha".to_string(),
                    cpu: 10.0,
                    mem_mb: 10.0,
                },
                ProcessRecord {
                    pid: 11,
                    ppid: 10,
                    name: "alpha-child".to_string(),
                    cpu: 2.0,
                    mem_mb: 1.0,
                },
            ],
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);
        app.get_processes();
        app.tree_mode = true;
        app.rebuild_tree_rows();
        app.selected_index = Some(0);

        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 10);
        assert!(app.expanded_pids.contains(&10));
        assert_eq!(app.tree_rows.len(), 2);

        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE, 10);
        assert!(!app.expanded_pids.contains(&10));
        assert_eq!(app.tree_rows.len(), 1);
    }

    #[test]
    fn enter_toggles_multiple_detail_targets_in_both_modes() {
        let collector = Box::new(MockCollector {
            rows: vec![
                ProcessRecord {
                    pid: 1,
                    ppid: 0,
                    name: "launchd".to_string(),
                    cpu: 1.0,
                    mem_mb: 100.0,
                },
                ProcessRecord {
                    pid: 10,
                    ppid: 1,
                    name: "node".to_string(),
                    cpu: 10.0,
                    mem_mb: 10.0,
                },
                ProcessRecord {
                    pid: 11,
                    ppid: 1,
                    name: "bash".to_string(),
                    cpu: 5.0,
                    mem_mb: 5.0,
                },
                ProcessRecord {
                    pid: 12,
                    ppid: 10,
                    name: "child".to_string(),
                    cpu: 2.0,
                    mem_mb: 1.0,
                },
            ],
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);
        app.current_processes = app.get_processes();
        app.update_filter_and_selection();
        app.selected_index = Some(0);

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(app.detail_targets.len(), 1);
        assert!(matches!(app.detail_targets[0], DetailTarget::Command(_)));

        app.selected_index = Some(1);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(app.detail_targets.len(), 2);

        app.tree_mode = true;
        app.rebuild_tree_rows();
        app.selected_index = Some(0);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert!(
            app.detail_targets
                .iter()
                .any(|target| matches!(target, DetailTarget::TreePid(_)))
        );
    }
}
