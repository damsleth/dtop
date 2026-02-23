use std::io;
use std::process::Command;

use dtop_core::parse_ps_line;

const PS_AXO_FIELDS: &str = "pid=,ppid=,%cpu=,rss=,comm=";

#[derive(Clone, Debug)]
pub struct ProcessRecord {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub cpu: f64,
    pub mem_mb: f64,
}

#[derive(Clone, Debug)]
pub struct SystemStats {
    pub cpu_total_pct: Option<f64>,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
}

pub trait Collector {
    fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>>;

    fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
        Ok(None)
    }
}

#[derive(Default)]
pub struct MacPsCollector;

impl Collector for MacPsCollector {
    fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>> {
        let output = Command::new("ps").args(["-caxo", PS_AXO_FIELDS]).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "ps failed with status {}",
                output.status
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_ps_output(&stdout))
    }
}

fn parse_ps_output(stdout: &str) -> Vec<ProcessRecord> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((pid, ppid, cpu, mem_kb, name)) = parse_ps_line(line) else {
            continue;
        };
        rows.push(ProcessRecord {
            pid,
            ppid,
            name,
            cpu,
            mem_mb: mem_kb / 1024.0,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::parse_ps_output;

    #[test]
    fn parses_multiple_rows_and_skips_invalid_input() {
        let stdout = "123 1 1.0 2048 top\ninvalid\n777 1 4.5 1024 Finder\n";
        let parsed = parse_ps_output(stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].pid, 123);
        assert_eq!(parsed[0].mem_mb, 2.0);
        assert_eq!(parsed[1].name, "Finder");
    }
}
