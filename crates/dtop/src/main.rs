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
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
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

struct App {
    poll_interval_ms: u64,
    sort_state: SortState,
    lazy_mode: bool,
    graph_mode: GraphMode,
    lazy_window: usize,
    debug_kill: bool,
    last_update: Instant,
    show_help: bool,
    current_processes: Vec<ProcEntry>,
    filtered_indices: Vec<usize>,
    selected_index: Option<usize>,
    show_extended: bool,
    filter_state: FilterState,
    detail_info: Vec<DetailEntry>,
    detail_command: Option<String>,
    detail_last_update: Instant,
    process_snapshot: Vec<ProcSnapshotEntry>,
    tree_rows: Vec<TreeRow>,
    tree_mode: bool,
    expanded_pids: HashSet<i32>,
    previous_scroll_anchor: Option<usize>,
    selected_name: Option<String>,
    metric_history: HashMap<String, MetricHistory>,
    pids_by_command: HashMap<String, Vec<i32>>,
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
            lazy_window: 10,
            debug_kill: false,
            last_update: now - Duration::from_millis(1000),
            show_help: false,
            current_processes: Vec::new(),
            filtered_indices: Vec::new(),
            selected_index: None,
            show_extended: false,
            filter_state: FilterState::default(),
            detail_info: Vec::new(),
            detail_command: None,
            detail_last_update: now - Duration::from_millis(1000),
            process_snapshot: Vec::new(),
            tree_rows: Vec::new(),
            tree_mode: false,
            expanded_pids: HashSet::new(),
            previous_scroll_anchor: None,
            selected_name: None,
            metric_history: HashMap::new(),
            pids_by_command: HashMap::new(),
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
            self.selected_name = None;
            self.show_extended = false;
            self.previous_scroll_anchor = None;
            self.detail_info.clear();
            self.detail_command = None;
            return;
        }

        if self.show_extended && let Some(selected_name) = self.selected_name.as_ref() {
            if let Some(new_pos) = self.filtered_indices.iter().position(|&i| {
                self.current_processes
                    .get(i)
                    .map(|p| &p.name == selected_name)
                    .unwrap_or(false)
            }) {
                self.selected_index = Some(new_pos);
            } else {
                self.show_extended = false;
                self.selected_name = None;
                self.selected_index = None;
                self.detail_info.clear();
                self.detail_command = None;
            }
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
        if !self.tree_mode && self.show_extended {
            if let Some(proc) = self.selected_process() {
                let selected_name = proc.name.clone();
                self.selected_name = Some(selected_name.clone());
                self.update_process_details(&selected_name);
            }
        } else {
            self.selected_name = None;
        }
    }

    fn clear_search_and_selection(&mut self) {
        self.filter_state.search_mode = false;
        self.filter_state.search_query.clear();
        self.selected_index = None;
        self.show_extended = false;
        self.previous_scroll_anchor = None;
        self.detail_info.clear();
        self.detail_command = None;
        self.selected_name = None;
    }

    fn update_process_details(&mut self, command: &str) {
        if command.is_empty() {
            self.detail_info.clear();
            self.detail_command = None;
            return;
        }
        if self.detail_command.as_deref() == Some(command)
            && self.detail_last_update.elapsed() < Duration::from_millis(self.poll_interval_ms)
        {
            return;
        }

        let Ok(processes) = self.collector.collect_processes() else {
            self.detail_info.clear();
            self.detail_command = Some(command.to_string());
            self.detail_last_update = Instant::now();
            return;
        };

        let mut details = Vec::new();
        for process in processes {
            if process.name != command {
                continue;
            }
            details.push(DetailEntry {
                pid: process.pid,
                cpu: process.cpu,
                mem_mb: process.mem_mb,
            });
        }
        details.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        self.detail_info = details;
        self.detail_command = Some(command.to_string());
        self.detail_last_update = Instant::now();
    }

    fn get_pids_for_command(&mut self, command: &str) -> Vec<i32> {
        if command.is_empty() {
            return Vec::new();
        }
        if let Some(pids) = self.pids_by_command.get(command)
            && !pids.is_empty()
        {
            return pids.clone();
        }
        self.detail_command = None;
        self.detail_last_update = Instant::now() - Duration::from_millis(self.poll_interval_ms);
        self.update_process_details(command);
        self.detail_info.iter().map(|d| d.pid).collect()
    }

    fn toggle_extended_selected(&mut self) {
        if let Some(proc) = self.selected_process() {
            let selected = proc.name.clone();
            if self.show_extended && self.detail_command.as_deref() == Some(selected.as_str()) {
                self.show_extended = false;
                self.detail_info.clear();
                self.detail_command = None;
                self.selected_name = None;
            } else {
                self.show_extended = true;
                self.selected_name = Some(selected.clone());
                self.update_process_details(&selected);
            }
        }
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
                self.show_extended = false;
                self.detail_info.clear();
                self.detail_command = None;
                self.selected_name = None;
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
            KeyCode::Enter => {
                if !self.tree_mode {
                    self.toggle_extended_selected();
                }
            }
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
                self.show_extended = false;
                self.previous_scroll_anchor = None;
                self.detail_info.clear();
                self.detail_command = None;
                self.selected_name = None;
            }
            KeyCode::Char(c) => {
                if is_search_input_char(c) {
                    self.filter_state.search_query.push(c);
                    self.selected_index = None;
                    self.show_extended = false;
                    self.previous_scroll_anchor = None;
                    self.detail_info.clear();
                    self.detail_command = None;
                    self.selected_name = None;
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

    let value_width: u16 = if app.lazy_mode { 12 } else { 10 };
    let graph_width: u16 = 8;
    let list_graphs_enabled = app.graph_mode.show_list();
    let detail_graphs_enabled = app.graph_mode.show_detail();
    let graph_columns_visible = !app.tree_mode
        && list_graphs_enabled
        && area.width > (value_width * 2 + graph_width * 2 + 6);
    let min_name = 1u16;
    let fixed_width = value_width * 2 + 2 + if graph_columns_visible { graph_width * 2 + 2 } else { 0 };
    let name_col_width = if area.width > fixed_width {
        area.width - fixed_width
    } else {
        min_name
    };

    let list_top = area.y + lines_before_header;
    let available_height = area.height.saturating_sub(lines_before_header);
    let detail_active = !app.tree_mode && app.show_extended && app.selected_name.is_some() && available_height > 1;

    let detail_rows = app.detail_info.len();
    let mut detail_height = 0u16;
    if detail_active {
        let mut required = 1u16;
        if detail_graphs_enabled {
            required = required.saturating_add(2);
        }
        if detail_rows > 0 {
            required = required.saturating_add(1 + detail_rows as u16);
        } else {
            required = required.saturating_add(1);
        }
        let requested = max(5, required as usize) as u16;
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
                    || (app.show_extended && app.selected_name.as_deref() == Some(proc.name.as_str()))
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
        x: area.x,
        y: list_top,
        width: area.width,
        height: min(
            table_height,
            area.height.saturating_sub(lines_before_header),
        ),
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

    if detail_height > 0
        && let Some(selected_name) = app.selected_name.clone()
    {
        app.update_process_details(&selected_name);
        let separator_y = list_top + table_height;
        if separator_y < area.y + area.height {
            let sep = Paragraph::new("─".repeat(area.width as usize));
            let sep_area = Rect {
                x: area.x,
                y: separator_y,
                width: area.width,
                height: 1,
            };
            f.render_widget(sep, sep_area);
        }

        let detail_area = Rect {
            x: area.x,
            y: separator_y + 1,
            width: area.width,
            height: detail_height,
        };

        let mut detail_lines = Vec::new();
        detail_lines.push(Line::from(vec![Span::styled(
            format!("Extended Info: {selected_name}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]));

        if detail_graphs_enabled {
            let detail_graph_width = detail_area.width.saturating_sub(14) as usize;
            let (cpu_graph, mem_graph) = app
                .metric_history
                .get(&selected_name)
                .map(|history| {
                    (
                        sparkline(&history.cpu, detail_graph_width),
                        sparkline(&history.mem, detail_graph_width),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        " ".repeat(detail_graph_width),
                        " ".repeat(detail_graph_width),
                    )
                });
            detail_lines.push(Line::from(format!("CPU trend: {cpu_graph}")));
            detail_lines.push(Line::from(format!("MEM trend: {mem_graph}")));
        }

        if detail_area.height > 1 {
            detail_lines.push(Line::from(Span::styled(
                "PID       CPU%      MEM(MB)",
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }

        let max_rows = detail_area.height.saturating_sub(2) as usize;
        if app.detail_info.is_empty() {
            if max_rows > 0 {
                detail_lines.push(Line::from("No processes found"));
            }
        } else {
            for detail in app.detail_info.iter().take(max_rows) {
                detail_lines.push(Line::from(format!(
                    "{:>6}   {:>6.2}     {:>7.2}",
                    detail.pid, detail.cpu, detail.mem_mb
                )));
            }
        }

        f.render_widget(Paragraph::new(detail_lines), detail_area);
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

#[cfg(test)]
mod tests {
    use super::{App, CollectorBackend, SortBy};
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
    fn builds_detail_rows_from_collector_fixture() {
        let collector = Box::new(MockCollector {
            rows: fixture_rows(),
        });
        let mut app = App::new(CollectorBackend::MacPs, collector);

        app.update_process_details("node");
        assert_eq!(app.detail_info.len(), 2);
        assert_eq!(app.detail_info[0].pid, 101);
        assert_eq!(app.detail_info[1].pid, 102);
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
}
