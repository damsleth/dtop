use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct ProcEntry {
    pub name: String,
    pub cpu: f64,
    pub mem_mb: f64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub cpu_delta: f64,
    pub mem_delta_mb: f64,
    pub is_new: bool,
    pub count: usize,
    pub cpu_lazy: f64,
    pub mem_lazy: f64,
}

#[derive(Clone, Debug)]
pub struct ProcSnapshotEntry {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub cpu: f64,
    pub mem_mb: f64,
    pub disk_read_bytes_total: Option<u64>,
    pub disk_write_bytes_total: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct MetricHistory {
    pub cpu: VecDeque<f64>,
    pub mem: VecDeque<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Cpu,
    Mem,
    Name,
    DiskRead,
    DiskWrite,
    CpuDelta,
    MemDelta,
}

impl SortBy {
    pub fn label(self) -> &'static str {
        match self {
            SortBy::Cpu => "CPU",
            SortBy::Mem => "MEM",
            SortBy::Name => "NAME",
            SortBy::DiskRead => "DISK READ",
            SortBy::DiskWrite => "DISK WRITE",
            SortBy::CpuDelta => "CPU DELTA",
            SortBy::MemDelta => "MEM DELTA",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SortState {
    pub by: SortBy,
    pub descending: bool,
}

impl Default for SortState {
    fn default() -> Self {
        Self {
            by: SortBy::Cpu,
            descending: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FilterState {
    pub search_mode: bool,
    pub search_query: String,
}

pub fn shrink_deque(deque: &mut VecDeque<f64>, max_len: usize) {
    while deque.len() > max_len {
        deque.pop_front();
    }
}

pub fn median(values: &VecDeque<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.iter().copied().collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some((sorted[mid - 1] + sorted[mid]) / 2.0)
    } else {
        Some(sorted[mid])
    }
}

pub fn parse_ps_line(line: &str) -> Option<(i32, i32, f64, f64, String)> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<i32>().ok()?;
    let ppid = parts.next()?.parse::<i32>().ok()?;
    let cpu = parts.next()?.parse::<f64>().ok()?;
    let mem = parts.next()?.parse::<f64>().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some((pid, ppid, cpu, mem, command))
}

#[cfg(test)]
mod tests {
    use super::{median, parse_ps_line};
    use std::collections::VecDeque;

    #[test]
    fn parses_ps_line_with_spaces_in_command() {
        let line = "123 1 12.5 2048 /Applications/My App/bin";
        let parsed = parse_ps_line(line).expect("expected parse success");
        assert_eq!(parsed.0, 123);
        assert_eq!(parsed.1, 1);
        assert_eq!(parsed.2, 12.5);
        assert_eq!(parsed.3, 2048.0);
        assert_eq!(parsed.4, "/Applications/My App/bin");
    }

    #[test]
    fn returns_none_on_invalid_ps_line() {
        assert!(parse_ps_line("abc 1 0.1 2.0 proc").is_none());
        assert!(parse_ps_line("123 1 0.1 2.0").is_none());
    }

    #[test]
    fn median_for_even_and_odd_inputs() {
        let odd = VecDeque::from([5.0, 1.0, 9.0]);
        assert_eq!(median(&odd), Some(5.0));

        let even = VecDeque::from([1.0, 4.0, 2.0, 8.0]);
        assert_eq!(median(&even), Some(3.0));
    }
}
