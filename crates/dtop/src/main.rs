use std::cmp::{max, min};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use libc::{SIGKILL, kill};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use dtop_core::{
    DetailEntry, FilterState, MetricHistory, ProcEntry, ProcSnapshotEntry, SortBy, SortState,
    TreeRow, median, parse_ps_line, shrink_deque,
};

const PS_AXO_FIELDS: &str = "pid=,ppid=,%cpu=,rss=,comm=";

#[derive(Clone, Debug)]
struct KillPrompt {
    command: String,
    pids: Vec<i32>,
    debug_summary: Option<String>,
}

struct App {
    poll_interval_ms: u64,
    sort_state: SortState,
    lazy_mode: bool,
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
    kill_prompt: Option<KillPrompt>,
    kill_prompt_error: Option<String>,
}

impl App {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            poll_interval_ms: 1000,
            sort_state: SortState::default(),
            lazy_mode: true,
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
        let output = Command::new("ps")
            .args(["-caxo", PS_AXO_FIELDS])
            .output();

        let Ok(output) = output else {
            self.pids_by_command.clear();
            self.process_snapshot.clear();
            return Vec::new();
        };
        if !output.status.success() {
            self.pids_by_command.clear();
            self.process_snapshot.clear();
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut grouped: HashMap<String, (f64, f64, usize, Vec<i32>)> = HashMap::new();
        let mut snapshot = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((pid, ppid, cpu, mem, command)) = parse_ps_line(line) else {
                continue;
            };
            let command_name = command.clone();
            let entry = grouped.entry(command).or_insert((0.0, 0.0, 0, Vec::new()));
            entry.0 += cpu;
            entry.1 += mem;
            entry.2 += 1;
            entry.3.push(pid);
            snapshot.push(ProcSnapshotEntry {
                pid,
                ppid,
                name: command_name,
                cpu,
                mem_mb: mem / 1024.0,
            });
        }
        self.process_snapshot = snapshot;

        let mut refreshed_pids_by_command: HashMap<String, Vec<i32>> = HashMap::new();
        let mut proc_list: Vec<ProcEntry> = grouped
            .into_iter()
            .map(|(name, (cpu, mem_kb, count, pids))| {
                refreshed_pids_by_command.insert(name.clone(), pids);
                ProcEntry {
                    name,
                    cpu,
                    mem_mb: mem_kb / 1024.0,
                    count,
                    cpu_lazy: cpu,
                    mem_lazy: mem_kb / 1024.0,
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

        let output = Command::new("ps")
            .args(["-caxo", PS_AXO_FIELDS])
            .output();
        let Ok(output) = output else {
            self.detail_info.clear();
            self.detail_command = Some(command.to_string());
            self.detail_last_update = Instant::now();
            return;
        };
        if !output.status.success() {
            self.detail_info.clear();
            self.detail_command = Some(command.to_string());
            self.detail_last_update = Instant::now();
            return;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut details = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((pid, _ppid, cpu, mem, comm)) = parse_ps_line(line) else {
                continue;
            };
            if comm != command {
                continue;
            }
            details.push(DetailEntry {
                pid,
                cpu,
                mem_mb: mem / 1024.0,
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
        if self.tree_mode {
            self.toggle_tree_selected();
            return;
        }

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
                if self.tree_mode {
                    self.set_tree_selected_expanded(false);
                } else {
                    let next = match self.sort_state.by {
                        SortBy::Cpu => SortBy::Name,
                        SortBy::Mem => SortBy::Cpu,
                        SortBy::Name => SortBy::Mem,
                    };
                    self.change_sort(next);
                }
            }
            KeyCode::Right => {
                if self.tree_mode {
                    self.set_tree_selected_expanded(true);
                } else {
                    let next = match self.sort_state.by {
                        SortBy::Cpu => SortBy::Mem,
                        SortBy::Mem => SortBy::Name,
                        SortBy::Name => SortBy::Cpu,
                    };
                    self.change_sort(next);
                }
            }
            KeyCode::Char('.') => self.set_lazy_window(self.lazy_window + 1),
            KeyCode::Char(',') => self.set_lazy_window(self.lazy_window.saturating_sub(1)),
            KeyCode::PageDown => self.move_selection(1, visible_rows, true),
            KeyCode::PageUp => self.move_selection(-1, visible_rows, true),
            KeyCode::Down => self.move_selection(1, visible_rows, false),
            KeyCode::Up => self.move_selection(-1, visible_rows, false),
            KeyCode::Enter => self.toggle_extended_selected(),
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
                if c.is_ascii_graphic() || c == ' ' {
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
            child_list.sort_by(|a, b| {
                let a_name = by_pid
                    .get(a)
                    .map(|p| p.name.to_lowercase())
                    .unwrap_or_default();
                let b_name = by_pid
                    .get(b)
                    .map(|p| p.name.to_lowercase())
                    .unwrap_or_default();
                a_name.cmp(&b_name).then_with(|| a.cmp(b))
            });
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
        roots.sort_by(|a, b| {
            let a_name = by_pid
                .get(a)
                .map(|p| p.name.to_lowercase())
                .unwrap_or_default();
            let b_name = by_pid
                .get(b)
                .map(|p| p.name.to_lowercase())
                .unwrap_or_default();
            a_name.cmp(&b_name).then_with(|| a.cmp(b))
        });

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

    fn set_tree_selected_expanded(&mut self, expand: bool) {
        let Some(idx) = self.selected_index else {
            return;
        };
        let Some(row) = self.tree_rows.get(idx) else {
            return;
        };
        if !row.has_children {
            return;
        }

        if expand {
            self.expanded_pids.insert(row.pid);
        } else {
            self.expanded_pids.remove(&row.pid);
        }
        self.rebuild_tree_rows();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
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
            if key_event.code == KeyCode::Char('c')
                && key_event.modifiers.contains(KeyModifiers::CONTROL)
            {
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
    let min_name = 1u16;
    let name_col_width = if area.width > (value_width * 2 + 2) {
        area.width - (value_width * 2 + 2)
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
            cpu_header.push_str(" x̄");
            mem_header.push_str(" x̄");
        }
    }

    let header_cells = vec![
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

                rows.push(
                    Row::new(vec![
                        Cell::from(truncate_to_width(&display_name, name_col_width as usize)),
                        Cell::from(format!("{:>width$.2}", cpu, width = value_width as usize)),
                        Cell::from(format!("{:>width$.2}", mem, width = value_width as usize)),
                    ])
                    .style(style),
                );
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

    let table = Table::new(
        rows,
        [
            Constraint::Length(name_col_width),
            Constraint::Length(value_width),
            Constraint::Length(value_width),
        ],
    )
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
        render_help_modal(f, app);
    }
    if app.kill_prompt.is_some() {
        render_kill_modal(f, app);
    }

    visible_rows
}

fn sort_header_style(active: bool) -> Style {
    let mut style = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    if active {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn truncate_to_width(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    input.chars().take(max_width).collect()
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn render_help_modal(f: &mut ratatui::Frame<'_>, app: &App) {
    let logo = [
        "     _ _              ",
        "  __| | |_ ___  _ __  ",
        " / _` | __/ _ \\| '_ \\ ",
        "| (_| | || (_) | |_) |",
        " \\__,_|\\__\\___/| .__/ ",
        "    @damsleth's|_|top ",
    ];
    let shortcuts = [
        ("+/-", "Adjust polling interval"),
        ("↑/↓", "Select process row"),
        ("e", "Toggle tree view"),
        ("x", "Toggle average mode"),
        (",/.", "Adjust average sample size"),
        ("←/→", "Switch sort column"),
        ("enter", "Toggle extended info / tree node"),
        ("c", "Sort by CPU"),
        ("m", "Sort by Memory"),
        ("p", "Sort by Name"),
        ("r", "Reverse sort"),
        ("f", "Filter processes"),
        ("k", "Kill highlighted process"),
        ("d", "Toggle kill debug info"),
        ("esc/?", "Help or close search"),
        ("q", "Quit"),
    ];

    let mut sort_label = app.sort_state.by.label().to_string();
    if app.lazy_mode {
        sort_label.push_str(" average (x̄)");
    }
    let status_lines = [
        format!("Poll interval: {}ms", app.poll_interval_ms),
        format!(
            "Mode: {}",
            if app.tree_mode { "Tree" } else { "List" }
        ),
        format!("Sort: {sort_label}"),
        format!("# Average Samples: {}", app.lazy_window),
        format!(
            "Kill debug: {}",
            if app.debug_kill { "ON" } else { "OFF" }
        ),
    ];

    let modal = centered_rect(70, 80, f.area());
    f.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White))
        .title("Help");
    f.render_widget(block.clone(), modal);
    let inner = block.inner(modal);

    let mut lines = Vec::new();
    for line in logo {
        lines.push(Line::from(Span::styled(
            line,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    for status in status_lines {
        lines.push(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Green),
        )));
    }
    lines.push(Line::from(""));
    for (key, desc) in shortcuts {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:<7}"), Style::default().fg(Color::White)),
            Span::raw(desc),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_kill_modal(f: &mut ratatui::Frame<'_>, app: &App) {
    let Some(prompt) = app.kill_prompt.as_ref() else {
        return;
    };

    let mut lines = vec![
        "Send SIGKILL?".to_string(),
        format!("Process: {}", prompt.command),
    ];

    if prompt.pids.is_empty() {
        lines.push("No PIDs found".to_string());
    } else {
        let mut current = String::from("PIDs: ");
        let max_width = 50usize;
        for pid in &prompt.pids {
            let pid_str = pid.to_string();
            let sep = if current.ends_with(": ") { "" } else { ", " };
            let candidate = format!("{current}{sep}{pid_str}");
            if candidate.chars().count() > max_width {
                lines.push(current);
                current = format!("      {pid_str}");
            } else {
                current = candidate;
            }
        }
        lines.push(current);
    }

    lines.push("Enter = confirm   Esc = cancel".to_string());
    if let Some(summary) = prompt.debug_summary.as_ref() {
        lines.push(summary.clone());
    }
    if let Some(err) = app.kill_prompt_error.as_ref() {
        lines.push(format!("Error: {err}"));
    }

    let modal = centered_rect(60, 40, f.area());
    f.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Confirm Kill")
        .border_style(Style::default().fg(Color::White));
    f.render_widget(block.clone(), modal);
    let inner = block.inner(modal);
    let rendered = lines.into_iter().map(Line::from).collect::<Vec<_>>();
    f.render_widget(Paragraph::new(rendered), inner);
}
