use std::io;
use std::process::Command;
#[cfg(feature = "linux-procfs")]
use std::fs;

use dtop_core::parse_ps_line;

const PS_AXO_FIELDS: &str = "pid=,ppid=,%cpu=,rss=,comm=";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectorBackend {
    MacPs,
    #[cfg(feature = "linux-procfs")]
    LinuxProcfs,
}

impl CollectorBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            CollectorBackend::MacPs => "mac-ps",
            #[cfg(feature = "linux-procfs")]
            CollectorBackend::LinuxProcfs => "linux-procfs",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "mac-ps" => Some(CollectorBackend::MacPs),
            #[cfg(feature = "linux-procfs")]
            "linux-procfs" => Some(CollectorBackend::LinuxProcfs),
            _ => None,
        }
    }
}

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

#[cfg(feature = "linux-procfs")]
#[derive(Default)]
pub struct LinuxProcfsCollector;

#[cfg(feature = "linux-procfs")]
impl Collector for LinuxProcfsCollector {
    fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>> {
        collect_linux_procfs_processes()
    }
}

pub fn build_collector(
    backend_override: Option<&str>,
) -> Result<(CollectorBackend, Box<dyn Collector>), String> {
    let backend = select_backend(backend_override)?;
    Ok((backend, collector_for_backend(backend)))
}

fn collector_for_backend(backend: CollectorBackend) -> Box<dyn Collector> {
    match backend {
        CollectorBackend::MacPs => Box::new(MacPsCollector),
        #[cfg(feature = "linux-procfs")]
        CollectorBackend::LinuxProcfs => Box::new(LinuxProcfsCollector),
    }
}

fn select_backend(backend_override: Option<&str>) -> Result<CollectorBackend, String> {
    if let Some(raw) = backend_override {
        let Some(backend) = CollectorBackend::from_name(raw) else {
            return Err(format!(
                "unknown collector backend '{raw}'. available: {}",
                available_backends().join(", ")
            ));
        };
        return Ok(backend);
    }

    #[cfg(all(target_os = "linux", feature = "linux-procfs"))]
    {
        Ok(CollectorBackend::LinuxProcfs)
    }
    #[cfg(not(all(target_os = "linux", feature = "linux-procfs")))]
    {
        Ok(CollectorBackend::MacPs)
    }
}

fn available_backends() -> Vec<&'static str> {
    #[cfg(feature = "linux-procfs")]
    {
        vec!["mac-ps", "linux-procfs"]
    }
    #[cfg(not(feature = "linux-procfs"))]
    {
        vec!["mac-ps"]
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

#[cfg(feature = "linux-procfs")]
fn collect_linux_procfs_processes() -> io::Result<Vec<ProcessRecord>> {
    let mut rows = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<i32>() else {
            continue;
        };

        let stat_path = format!("/proc/{pid}/stat");
        let stat = match fs::read_to_string(stat_path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let Some(ppid) = parse_linux_stat_ppid(&stat) else {
            continue;
        };

        let comm_path = format!("/proc/{pid}/comm");
        let name = fs::read_to_string(comm_path)
            .map(|raw| raw.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let status_path = format!("/proc/{pid}/status");
        let mem_mb = fs::read_to_string(status_path)
            .ok()
            .and_then(|content| parse_linux_rss_mb(&content))
            .unwrap_or(0.0);

        rows.push(ProcessRecord {
            pid,
            ppid,
            name,
            cpu: 0.0,
            mem_mb,
        });
    }
    Ok(rows)
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_stat_ppid(stat: &str) -> Option<i32> {
    let close_idx = stat.rfind(')')?;
    let after_comm = stat.get(close_idx + 2..)?;
    let mut fields = after_comm.split_whitespace();
    fields.next()?;
    fields.next()?.parse::<i32>().ok()
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_rss_mb(status: &str) -> Option<f64> {
    for line in status.lines() {
        let raw = line.strip_prefix("VmRSS:")?.trim();
        let kb = raw.split_whitespace().next()?.parse::<f64>().ok()?;
        return Some(kb / 1024.0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{CollectorBackend, parse_ps_output};

    #[test]
    fn parses_multiple_rows_and_skips_invalid_input() {
        let stdout = "123 1 1.0 2048 top\ninvalid\n777 1 4.5 1024 Finder\n";
        let parsed = parse_ps_output(stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].pid, 123);
        assert_eq!(parsed[0].mem_mb, 2.0);
        assert_eq!(parsed[1].name, "Finder");
    }

    #[test]
    fn exposes_backend_name() {
        assert_eq!(CollectorBackend::MacPs.as_str(), "mac-ps");
    }

    #[cfg(feature = "linux-procfs")]
    #[test]
    fn linux_backend_name_is_available() {
        assert_eq!(CollectorBackend::LinuxProcfs.as_str(), "linux-procfs");
    }
}
