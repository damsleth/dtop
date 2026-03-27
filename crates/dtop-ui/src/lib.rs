use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use std::collections::VecDeque;

use dtop_core::SortBy;

pub struct HelpModalData {
    pub poll_interval_ms: u64,
    pub mode_label: String,
    pub sort_label: String,
    pub lazy_window: usize,
    pub kill_debug_enabled: bool,
    pub backend_label: String,
    pub graph_mode: String,
    pub keybindings: Vec<(String, String)>,
}

pub struct KillModalData<'a> {
    pub command: &'a str,
    pub pids: &'a [i32],
    pub signal_label: &'a str,
    pub graceful_then_force: bool,
    pub require_typed_confirmation: bool,
    pub typed_confirmation: &'a str,
    pub debug_summary: Option<&'a str>,
    pub error: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThresholdLevel {
    Normal,
    Warning,
    Critical,
}

pub fn is_ctrl_c_quit(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL)
}

pub fn is_search_input_char(c: char) -> bool {
    c.is_ascii_graphic() || c == ' '
}

pub fn sort_header_style(active: bool) -> Style {
    let mut style = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    if active {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

pub fn threshold_level(value: f64, warning: f64, critical: f64) -> ThresholdLevel {
    if value >= critical {
        ThresholdLevel::Critical
    } else if value >= warning {
        ThresholdLevel::Warning
    } else {
        ThresholdLevel::Normal
    }
}

pub fn threshold_color(level: ThresholdLevel) -> Color {
    match level {
        ThresholdLevel::Normal => Color::Green,
        ThresholdLevel::Warning => Color::Yellow,
        ThresholdLevel::Critical => Color::Red,
    }
}

pub fn format_uptime_short(total_seconds: u64) -> String {
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

pub fn truncate_to_width(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    input.chars().take(max_width).collect()
}

pub fn sparkline(values: &VecDeque<f64>, width: usize) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if width == 0 {
        return String::new();
    }
    if values.is_empty() {
        return " ".repeat(width);
    }

    let sampled = sample_tail(values, width);
    let mut min_v = f64::INFINITY;
    let mut max_v = f64::NEG_INFINITY;
    for value in &sampled {
        min_v = min_v.min(*value);
        max_v = max_v.max(*value);
    }

    if (max_v - min_v).abs() < f64::EPSILON {
        if max_v <= 0.0 {
            return " ".repeat(width);
        }
        return std::iter::repeat_n(LEVELS[0], width).collect();
    }

    sampled
        .iter()
        .map(|value| {
            let normalized = ((value - min_v) / (max_v - min_v)).clamp(0.0, 1.0);
            let idx = (normalized * ((LEVELS.len() - 1) as f64)).round() as usize;
            LEVELS[idx]
        })
        .collect()
}

fn sample_tail(values: &VecDeque<f64>, width: usize) -> Vec<f64> {
    if values.len() <= width {
        let mut out = vec![0.0; width - values.len()];
        out.extend(values.iter().copied());
        return out;
    }

    let start = values.len() - width;
    values.iter().skip(start).copied().collect()
}

pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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

pub fn render_help_modal(f: &mut ratatui::Frame<'_>, data: &HelpModalData) {
    let logo = [
        "     _ _              ",
        "  __| | |_ ___  _ __  ",
        " / _` | __/ _ \\| '_ \\ ",
        "| (_| | || (_) | |_) |",
        " \\__,_|\\__\\___/| .__/ ",
        "    @damsleth's|_|top ",
    ];
    let status_lines = [
        format!("Poll interval: {}ms", data.poll_interval_ms),
        format!("Mode: {}", data.mode_label),
        format!("Collector: {}", data.backend_label),
        format!("Graphs: {}", data.graph_mode),
        format!("Sort: {}", data.sort_label),
        format!("# Average Samples: {}", data.lazy_window),
        format!(
            "Kill debug: {}",
            if data.kill_debug_enabled { "ON" } else { "OFF" }
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
    for (key, desc) in &data.keybindings {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:<12}"), Style::default().fg(Color::White)),
            Span::raw(desc.as_str()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

pub fn render_kill_modal(f: &mut ratatui::Frame<'_>, data: &KillModalData<'_>) {
    let mut lines = vec![
        "Dispatch signal?".to_string(),
        format!("Process: {}", data.command),
        format!("Signal: {}", data.signal_label),
        format!(
            "Graceful->force: {}",
            if data.graceful_then_force {
                "ON"
            } else {
                "OFF"
            }
        ),
    ];

    if data.pids.is_empty() {
        lines.push("No PIDs found".to_string());
    } else {
        let mut current = String::from("PIDs: ");
        let max_width = 50usize;
        for pid in data.pids {
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

    if data.require_typed_confirmation {
        lines.push("Type KILL then Enter to confirm".to_string());
        lines.push(format!("Confirm input: {}", data.typed_confirmation));
    } else {
        lines.push("Enter = confirm   Esc = cancel".to_string());
    }
    lines.push("1=SIGTERM 2=SIGINT 3=SIGKILL g=toggle graceful flow".to_string());
    if let Some(summary) = data.debug_summary {
        lines.push(summary.to_string());
    }
    if let Some(err) = data.error {
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

pub struct SystemHeaderData {
    pub cpu_total_pct: f64,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub swap_used_mb: Option<f64>,
    pub swap_total_mb: Option<f64>,
    pub load_avg_1: Option<f64>,
    pub load_avg_5: Option<f64>,
    pub load_avg_15: Option<f64>,
    pub uptime_seconds: Option<u64>,
}

pub fn render_system_overview_header(
    f: &mut ratatui::Frame<'_>,
    data: &SystemHeaderData,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let cpu_value = data.cpu_total_pct;
    let mem_pct = match (data.mem_used_mb, data.mem_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => (used / total) * 100.0,
        _ => 0.0,
    };
    let swap_pct = match (data.swap_used_mb, data.swap_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => (used / total) * 100.0,
        _ => 0.0,
    };

    let cpu_color = threshold_color(threshold_level(cpu_value, 70.0, 90.0));
    let mem_color = threshold_color(threshold_level(mem_pct, 75.0, 90.0));
    let swap_color = threshold_color(threshold_level(swap_pct, 80.0, 95.0));

    let load = match (data.load_avg_1, data.load_avg_5, data.load_avg_15) {
        (Some(a), Some(b), Some(c)) => format!("{a:.2}/{b:.2}/{c:.2}"),
        _ => "n/a".to_string(),
    };
    let uptime = data
        .uptime_seconds
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

pub struct StatusLineData<'a> {
    pub view_label: &'a str,
    pub sort_label: &'a str,
    pub sort_descending: bool,
    pub poll_interval_ms: u64,
    pub last_poll_ms: f64,
    pub last_parse_ms: f64,
    pub last_render_ms: f64,
    pub recent_exited_commands: &'a [String],
    pub action_audit_line: Option<&'a str>,
    pub last_snapshot_path: Option<&'a str>,
    pub status_error: Option<&'a str>,
    pub is_network_view: bool,
}

pub fn render_status_line(f: &mut ratatui::Frame<'_>, data: &StatusLineData<'_>, area: Rect) {
    let mut status = format!(
        "{} | Sort={}{} | Poll={}ms | Timing P {:.1}ms Parse {:.1}ms Render {:.1}ms",
        data.view_label,
        data.sort_label,
        if data.sort_descending { "↓" } else { "↑" },
        data.poll_interval_ms,
        data.last_poll_ms,
        data.last_parse_ms,
        data.last_render_ms,
    );

    if !data.recent_exited_commands.is_empty() {
        status.push_str(&format!(
            " | Exited: {}",
            data.recent_exited_commands.join(", ")
        ));
    }
    if let Some(line) = data.action_audit_line {
        status.push_str(&format!(" | Audit: {line}"));
    }
    if let Some(path) = data.last_snapshot_path {
        status.push_str(&format!(" | Snapshot: {path}"));
    }
    if let Some(err) = data.status_error {
        status.push_str(&format!(" | Error: {err}"));
    }
    if data.is_network_view {
        status.push_str(" | Interface-level throughput view");
    }

    f.render_widget(
        Paragraph::new(truncate_to_width(&status, area.width as usize)),
        area,
    );
}

pub struct ProcessRowData {
    pub name: String,
    pub cpu: f64,
    pub cpu_lazy: f64,
    pub cpu_delta: f64,
    pub mem_mb: f64,
    pub mem_lazy: f64,
    pub mem_delta_mb: f64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub is_new: bool,
    pub count: usize,
    pub cpu_history: VecDeque<f64>,
    pub mem_history: VecDeque<f64>,
}

pub struct ProcessTableData<'a> {
    pub rows: &'a [ProcessRowData],
    pub filtered_indices: &'a [usize],
    pub selected_index: Option<usize>,
    pub sort_by: SortBy,
    pub delta_mode: bool,
    pub lazy_mode: bool,
    pub show_disk_columns: bool,
    pub graph_enabled: bool,
}

pub fn draw_process_table(
    f: &mut ratatui::Frame<'_>,
    data: &ProcessTableData<'_>,
    area: Rect,
    start_index: usize,
    visible_rows: usize,
) -> usize {
    let name_width = if data.show_disk_columns {
        area.width.saturating_sub(46)
    } else {
        area.width.saturating_sub(30)
    };
    let graph_width: u16 = 8;
    let graph_enabled = data.graph_enabled && area.width > 80;

    let mut header_cells = vec![
        Cell::from(Span::styled(
            truncate_to_width("PROCESS", name_width as usize),
            sort_header_style(data.sort_by == SortBy::Name),
        )),
        Cell::from(Span::styled(
            if data.delta_mode { "CPU Δ" } else { "CPU%" },
            sort_header_style(matches!(data.sort_by, SortBy::Cpu | SortBy::CpuDelta)),
        )),
        Cell::from(Span::styled(
            if data.delta_mode { "MEM Δ" } else { "MEM(MB)" },
            sort_header_style(matches!(data.sort_by, SortBy::Mem | SortBy::MemDelta)),
        )),
    ];

    let mut constraints = vec![
        Constraint::Length(name_width.max(1)),
        Constraint::Length(10),
        Constraint::Length(10),
    ];

    if data.show_disk_columns {
        header_cells.push(Cell::from(Span::styled(
            "READ/s",
            sort_header_style(data.sort_by == SortBy::DiskRead),
        )));
        header_cells.push(Cell::from(Span::styled(
            "WRITE/s",
            sort_header_style(data.sort_by == SortBy::DiskWrite),
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
        if list_index >= data.filtered_indices.len() {
            break;
        }
        let real_idx = data.filtered_indices[list_index];
        let Some(proc) = data.rows.get(real_idx) else {
            continue;
        };

        let mut display_name = proc.name.clone();
        if proc.count > 1 {
            display_name.push_str(&format!(" ({})", proc.count));
        }
        if proc.is_new {
            display_name = format!("+ {display_name}");
        }

        let cpu_display = if data.delta_mode {
            format!("{:+.2}", proc.cpu_delta)
        } else if data.lazy_mode {
            format!("{:.2}", proc.cpu_lazy)
        } else {
            format!("{:.2}", proc.cpu)
        };

        let mem_display = if data.delta_mode {
            format!("{:+.2}", proc.mem_delta_mb)
        } else if data.lazy_mode {
            format!("{:.2}", proc.mem_lazy)
        } else {
            format!("{:.2}", proc.mem_mb)
        };

        let mut style = Style::default();
        let alert_value = if data.delta_mode {
            proc.cpu_delta.abs()
        } else if data.lazy_mode {
            proc.cpu_lazy
        } else {
            proc.cpu
        };
        style = style.fg(match threshold_level(alert_value, 20.0, 50.0) {
            ThresholdLevel::Normal => Color::Reset,
            ThresholdLevel::Warning => Color::Yellow,
            ThresholdLevel::Critical => Color::Red,
        });

        if data.selected_index == Some(list_index) {
            style = style.add_modifier(Modifier::REVERSED);
        }

        let mut cells = vec![
            Cell::from(truncate_to_width(&display_name, name_width as usize)),
            Cell::from(cpu_display),
            Cell::from(mem_display),
        ];

        if data.show_disk_columns {
            cells.push(Cell::from(format_bytes_per_second(proc.disk_read_bps)));
            cells.push(Cell::from(format_bytes_per_second(proc.disk_write_bps)));
        }

        if graph_enabled {
            cells.push(Cell::from(sparkline(&proc.cpu_history, graph_width as usize)));
            cells.push(Cell::from(sparkline(&proc.mem_history, graph_width as usize)));
        }

        rows.push(Row::new(cells).style(style));
    }

    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(1);
    f.render_widget(table, area);

    visible_rows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkSortBy {
    Rx,
    Tx,
    Total,
    Name,
}

pub struct NetworkRowData {
    pub name: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub total_bps: f64,
}

pub struct NetworkTableData<'a> {
    pub rows: &'a [NetworkRowData],
    pub filtered_indices: &'a [usize],
    pub selected_index: Option<usize>,
    pub sort_by: NetworkSortBy,
}

pub fn draw_network_table(
    f: &mut ratatui::Frame<'_>,
    data: &NetworkTableData<'_>,
    area: Rect,
    start_index: usize,
    visible_rows: usize,
) -> usize {
    let name_width = area.width.saturating_sub(36);
    let header = Row::new(vec![
        Cell::from(Span::styled(
            truncate_to_width("INTERFACE", name_width as usize),
            sort_header_style(data.sort_by == NetworkSortBy::Name),
        )),
        Cell::from(Span::styled(
            "RX/s",
            sort_header_style(data.sort_by == NetworkSortBy::Rx),
        )),
        Cell::from(Span::styled(
            "TX/s",
            sort_header_style(data.sort_by == NetworkSortBy::Tx),
        )),
        Cell::from(Span::styled(
            "TOTAL/s",
            sort_header_style(data.sort_by == NetworkSortBy::Total),
        )),
    ])
    .height(1);

    let mut rows = Vec::new();
    for row_i in 0..visible_rows {
        let list_index = start_index + row_i;
        if list_index >= data.filtered_indices.len() {
            break;
        }
        let real_idx = data.filtered_indices[list_index];
        let Some(net) = data.rows.get(real_idx) else {
            continue;
        };

        let mut style = Style::default();
        if data.selected_index == Some(list_index) {
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

pub fn format_bytes_per_second(bytes_per_second: f64) -> String {
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
    use super::{ThresholdLevel, format_uptime_short, sparkline, threshold_level};
    use std::collections::VecDeque;

    #[test]
    fn sparkline_uses_requested_width() {
        let values = VecDeque::from([0.0, 1.0, 2.0]);
        let out = sparkline(&values, 6);
        assert_eq!(out.chars().count(), 6);
    }

    #[test]
    fn sparkline_handles_flat_series() {
        let values = VecDeque::from([3.0, 3.0, 3.0, 3.0]);
        let out = sparkline(&values, 4);
        assert_eq!(out, "▁▁▁▁");
    }

    #[test]
    fn threshold_levels_map_to_expected_states() {
        assert_eq!(threshold_level(40.0, 70.0, 90.0), ThresholdLevel::Normal);
        assert_eq!(threshold_level(75.0, 70.0, 90.0), ThresholdLevel::Warning);
        assert_eq!(threshold_level(95.0, 70.0, 90.0), ThresholdLevel::Critical);
    }

    #[test]
    fn formats_uptime_compactly() {
        assert_eq!(format_uptime_short(59), "0m");
        assert_eq!(format_uptime_short(3_780), "1h3m");
        assert_eq!(format_uptime_short(90_120), "1d1h");
    }
}
