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
    pub net_rx_bytes_total: Option<u64>,
    pub net_tx_bytes_total: Option<u64>,
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

    fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
        let cpu_total_pct = collect_cpu_total_pct()?;
        let (mem_used_mb, mem_total_mb) = collect_memory_stats_mb()?;
        let (net_rx_bytes_total, net_tx_bytes_total) = collect_network_totals()?;
        Ok(Some(SystemStats {
            cpu_total_pct,
            mem_used_mb,
            mem_total_mb,
            net_rx_bytes_total,
            net_tx_bytes_total,
        }))
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

fn collect_cpu_total_pct() -> io::Result<Option<f64>> {
    let output = Command::new("ps").args(["-A", "-o", "%cpu="]).output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut sum = 0.0;
    let mut count = 0usize;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = trimmed.parse::<f64>() {
            sum += v;
            count += 1;
        }
    }
    if count == 0 { Ok(None) } else { Ok(Some(sum)) }
}

fn collect_memory_stats_mb() -> io::Result<(Option<f64>, Option<f64>)> {
    let total_bytes = {
        let output = Command::new("sysctl").args(["-n", "hw.memsize"]).output()?;
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        } else {
            None
        }
    };

    let used_mb = {
        let output = Command::new("vm_stat").output()?;
        if output.status.success() {
            parse_vm_stat_used_mb(&String::from_utf8_lossy(&output.stdout))
        } else {
            None
        }
    };

    Ok((used_mb, total_bytes.map(|b| b / (1024.0 * 1024.0))))
}

fn parse_vm_stat_used_mb(stdout: &str) -> Option<f64> {
    let page_size_bytes = stdout
        .lines()
        .next()
        .and_then(|line| {
            let marker = "page size of ";
            let start = line.find(marker)? + marker.len();
            let rest = line.get(start..)?;
            let end = rest.find(" bytes")?;
            rest.get(..end)?.trim().parse::<f64>().ok()
        })
        .unwrap_or(4096.0);

    let mut active = 0.0;
    let mut wired = 0.0;
    let mut compressed = 0.0;

    for line in stdout.lines() {
        if let Some(v) = parse_vm_stat_pages(line, "Pages active") {
            active = v;
        }
        if let Some(v) = parse_vm_stat_pages(line, "Pages wired down") {
            wired = v;
        }
        if let Some(v) = parse_vm_stat_pages(line, "Pages occupied by compressor") {
            compressed = v;
        }
    }

    let used_pages = active + wired + compressed;
    Some((used_pages * page_size_bytes) / (1024.0 * 1024.0))
}

fn parse_vm_stat_pages(line: &str, key: &str) -> Option<f64> {
    if !line.starts_with(key) {
        return None;
    }
    let after_colon = line.split(':').nth(1)?.trim();
    let value = after_colon.trim_end_matches('.').replace('.', "");
    value.parse::<f64>().ok()
}

fn collect_network_totals() -> io::Result<(Option<u64>, Option<u64>)> {
    let output = Command::new("netstat").args(["-ibn"]).output()?;
    if !output.status.success() {
        return Ok((None, None));
    }
    Ok(parse_netstat_interface_totals(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_netstat_interface_totals(stdout: &str) -> (Option<u64>, Option<u64>) {
    let mut header: Option<Vec<&str>> = None;
    let mut per_interface: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        if parts[0] == "Name" {
            header = Some(parts);
            continue;
        }
        let Some(columns) = header.as_ref() else {
            continue;
        };

        let name_idx = columns.iter().position(|c| *c == "Name");
        let rx_idx = columns.iter().position(|c| *c == "Ibytes");
        let tx_idx = columns.iter().position(|c| *c == "Obytes");
        let (Some(name_idx), Some(rx_idx), Some(tx_idx)) = (name_idx, rx_idx, tx_idx) else {
            continue;
        };
        let max_idx = name_idx.max(rx_idx).max(tx_idx);
        if parts.len() <= max_idx {
            continue;
        }

        let iface = parts[name_idx];
        if iface.starts_with("lo") {
            continue;
        }
        let (Ok(rx), Ok(tx)) = (parts[rx_idx].parse::<u64>(), parts[tx_idx].parse::<u64>()) else {
            continue;
        };

        per_interface
            .entry(iface.to_string())
            .and_modify(|current| {
                current.0 = current.0.max(rx);
                current.1 = current.1.max(tx);
            })
            .or_insert((rx, tx));
    }

    if per_interface.is_empty() {
        return (None, None);
    }
    let mut total_rx = 0u64;
    let mut total_tx = 0u64;
    for (rx, tx) in per_interface.values() {
        total_rx = total_rx.saturating_add(*rx);
        total_tx = total_tx.saturating_add(*tx);
    }
    (Some(total_rx), Some(total_tx))
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
    use super::{
        CollectorBackend, parse_netstat_interface_totals, parse_ps_output, parse_vm_stat_used_mb,
    };

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

    #[test]
    fn parses_vm_stat_used_memory() {
        let vm_stat = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages active:                               100.\nPages wired down:                            50.\nPages occupied by compressor:                25.\n";
        let used_mb = parse_vm_stat_used_mb(vm_stat).expect("expected parse success");
        assert!((used_mb - 0.68359375).abs() < 0.0001);
    }

    #[test]
    fn parses_network_totals_without_loopback() {
        let netstat = "Name  Mtu   Network       Address            Ipkts Ierrs    Opkts Oerrs  Coll  Drop     Ibytes    Obytes\nen0   1500  <Link#4>      aa:bb:cc:dd:ee:ff  10    0        20    0      0     0        1000      2000\nen0   1500  192.168.1     192.168.1.2        11    0        21    0      0     0        1100      1900\nlo0   16384 <Link#1>      lo0                99    0        99    0      0     0       99999     99999\n";
        let (rx, tx) = parse_netstat_interface_totals(netstat);
        assert_eq!(rx, Some(1100));
        assert_eq!(tx, Some(2000));
    }

    #[cfg(feature = "linux-procfs")]
    #[test]
    fn linux_backend_name_is_available() {
        assert_eq!(CollectorBackend::LinuxProcfs.as_str(), "linux-procfs");
    }
}
