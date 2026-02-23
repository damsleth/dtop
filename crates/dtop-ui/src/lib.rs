use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::VecDeque;

pub struct HelpModalData {
    pub poll_interval_ms: u64,
    pub mode_label: String,
    pub sort_label: String,
    pub lazy_window: usize,
    pub kill_debug_enabled: bool,
    pub backend_label: String,
    pub graph_mode: String,
}

pub struct KillModalData<'a> {
    pub command: &'a str,
    pub pids: &'a [i32],
    pub debug_summary: Option<&'a str>,
    pub error: Option<&'a str>,
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
    let shortcuts = [
        ("+/-", "Adjust polling interval"),
        ("↑/↓", "Select process row"),
        ("e", "Toggle tree view"),
        ("x", "Toggle average mode"),
        ("g", "Cycle graph mode (Off/List/Detail/Both)"),
        ("1/2/3", "Toggle CPU/MEM/NET panels"),
        (",/.", "Adjust average sample size"),
        ("←/→", "Switch sort column"),
        ("space", "Expand/collapse tree node"),
        ("enter", "Toggle extended info"),
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
    for (key, desc) in shortcuts {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:<7}"), Style::default().fg(Color::White)),
            Span::raw(desc),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

pub fn render_kill_modal(f: &mut ratatui::Frame<'_>, data: &KillModalData<'_>) {
    let mut lines = vec![
        "Send SIGKILL?".to_string(),
        format!("Process: {}", data.command),
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

    lines.push("Enter = confirm   Esc = cancel".to_string());
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

#[cfg(test)]
mod tests {
    use super::sparkline;
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
}
