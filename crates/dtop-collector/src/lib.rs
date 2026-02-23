use std::collections::HashMap;
use std::io;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "linux-procfs")]
use std::fs;
#[cfg(feature = "linux-procfs")]
use std::sync::Mutex;

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
    pub disk_read_bytes_total: Option<u64>,
    pub disk_write_bytes_total: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct InterfaceCounter {
    pub name: String,
    pub rx_bytes_total: u64,
    pub tx_bytes_total: u64,
}

#[derive(Clone, Debug)]
pub struct SystemStats {
    pub cpu_total_pct: Option<f64>,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub swap_used_mb: Option<f64>,
    pub swap_total_mb: Option<f64>,
    pub load_avg_1: Option<f64>,
    pub load_avg_5: Option<f64>,
    pub load_avg_15: Option<f64>,
    pub uptime_seconds: Option<u64>,
    pub net_rx_bytes_total: Option<u64>,
    pub net_tx_bytes_total: Option<u64>,
    pub disk_read_bytes_total: Option<u64>,
    pub disk_write_bytes_total: Option<u64>,
}

pub trait Collector: Send {
    fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>>;

    fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
        Ok(None)
    }

    fn collect_network_interfaces(&self) -> io::Result<Vec<InterfaceCounter>> {
        Ok(Vec::new())
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
        let mut rows = parse_ps_output(&stdout);
        let disk_by_pid = collect_macos_disk_counters_by_pid().unwrap_or_default();
        for row in &mut rows {
            if let Some((read, write)) = disk_by_pid.get(&row.pid) {
                row.disk_read_bytes_total = Some(*read);
                row.disk_write_bytes_total = Some(*write);
            }
        }
        Ok(rows)
    }

    fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
        let cpu_total_pct = collect_cpu_total_pct()?;
        let (mem_used_mb, mem_total_mb) = collect_memory_stats_mb()?;
        let (swap_used_mb, swap_total_mb) = collect_swap_stats_mb()?;
        let (load_avg_1, load_avg_5, load_avg_15) = collect_macos_load_avg()?;
        let uptime_seconds = collect_macos_uptime_seconds()?;
        let (net_rx_bytes_total, net_tx_bytes_total) = collect_network_totals()?;
        let (disk_read_bytes_total, disk_write_bytes_total) = collect_macos_disk_totals()?;
        Ok(Some(SystemStats {
            cpu_total_pct,
            mem_used_mb,
            mem_total_mb,
            swap_used_mb,
            swap_total_mb,
            load_avg_1,
            load_avg_5,
            load_avg_15,
            uptime_seconds,
            net_rx_bytes_total,
            net_tx_bytes_total,
            disk_read_bytes_total,
            disk_write_bytes_total,
        }))
    }

    fn collect_network_interfaces(&self) -> io::Result<Vec<InterfaceCounter>> {
        let output = Command::new("netstat").args(["-ibn"]).output()?;
        if !output.status.success() {
            return Ok(Vec::new());
        }
        Ok(parse_netstat_interface_counters(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

#[cfg(feature = "linux-procfs")]
#[derive(Default)]
pub struct LinuxProcfsCollector {
    state: Mutex<LinuxCollectorState>,
}

#[cfg(feature = "linux-procfs")]
#[derive(Default)]
struct LinuxCollectorState {
    last_total_jiffies: Option<u64>,
    last_idle_jiffies: Option<u64>,
    last_proc_jiffies: HashMap<i32, u64>,
}

#[cfg(feature = "linux-procfs")]
impl Collector for LinuxProcfsCollector {
    fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>> {
        collect_linux_procfs_processes(&self.state)
    }

    fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
        collect_linux_system_stats(&self.state)
    }

    fn collect_network_interfaces(&self) -> io::Result<Vec<InterfaceCounter>> {
        let content = fs::read_to_string("/proc/net/dev")?;
        Ok(parse_linux_net_dev(&content))
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
        CollectorBackend::LinuxProcfs => Box::new(LinuxProcfsCollector::default()),
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
            disk_read_bytes_total: None,
            disk_write_bytes_total: None,
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

fn collect_swap_stats_mb() -> io::Result<(Option<f64>, Option<f64>)> {
    let output = Command::new("sysctl")
        .args(["-n", "vm.swapusage"])
        .output()?;
    if !output.status.success() {
        return Ok((None, None));
    }
    Ok(parse_macos_swapusage(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_macos_swapusage(raw: &str) -> (Option<f64>, Option<f64>) {
    let total_mb = parse_size_value_mb(raw, "total = ");
    let used_mb = parse_size_value_mb(raw, "used = ");
    (used_mb, total_mb)
}

fn parse_size_value_mb(raw: &str, key: &str) -> Option<f64> {
    let idx = raw.find(key)? + key.len();
    let tail = raw.get(idx..)?;
    let token = tail.split(',').next()?.trim();
    let mut digits = String::new();
    let mut unit = 'M';
    for ch in token.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            digits.push(ch);
        } else if ch.is_ascii_alphabetic() {
            unit = ch;
            break;
        }
    }
    let value = digits.parse::<f64>().ok()?;
    let mb = match unit {
        'G' => value * 1024.0,
        'T' => value * 1024.0 * 1024.0,
        _ => value,
    };
    Some(mb)
}

fn collect_macos_load_avg() -> io::Result<(Option<f64>, Option<f64>, Option<f64>)> {
    let output = Command::new("sysctl").args(["-n", "vm.loadavg"]).output()?;
    if !output.status.success() {
        return Ok((None, None, None));
    }
    Ok(parse_macos_loadavg(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_macos_loadavg(raw: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let trimmed = raw.trim().trim_start_matches('{').trim_end_matches('}');
    let mut nums = trimmed
        .split_whitespace()
        .filter_map(|n| n.parse::<f64>().ok());
    (nums.next(), nums.next(), nums.next())
}

fn collect_macos_uptime_seconds() -> io::Result<Option<u64>> {
    let output = Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_macos_uptime_seconds(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_macos_uptime_seconds(raw: &str) -> Option<u64> {
    let sec_idx = raw.find("sec = ")? + "sec = ".len();
    let tail = raw.get(sec_idx..)?;
    let sec_token = tail.split(',').next()?.trim();
    let boot_epoch = sec_token.parse::<u64>().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now.saturating_sub(boot_epoch))
}

fn collect_network_totals() -> io::Result<(Option<u64>, Option<u64>)> {
    let output = Command::new("netstat").args(["-ibn"]).output()?;
    if !output.status.success() {
        return Ok((None, None));
    }
    Ok(sum_interface_totals(&parse_netstat_interface_counters(
        &String::from_utf8_lossy(&output.stdout),
    )))
}

fn collect_macos_disk_counters_by_pid() -> io::Result<HashMap<i32, (u64, u64)>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,rbytes=,wbytes="])
        .output()?;
    if !output.status.success() {
        return Ok(HashMap::new());
    }

    let mut counters = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(pid) = parts[0].parse::<i32>() else {
            continue;
        };
        let Ok(read_bytes) = parts[1].parse::<u64>() else {
            continue;
        };
        let Ok(write_bytes) = parts[2].parse::<u64>() else {
            continue;
        };
        counters.insert(pid, (read_bytes, write_bytes));
    }
    Ok(counters)
}

fn collect_macos_disk_totals() -> io::Result<(Option<u64>, Option<u64>)> {
    let output = Command::new("iostat")
        .args(["-Id", "-w", "1", "-c", "1"])
        .output()?;
    if !output.status.success() {
        return Ok((None, None));
    }
    Ok(parse_macos_iostat_totals(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_macos_iostat_totals(raw: &str) -> (Option<u64>, Option<u64>) {
    let mut read_kb = 0.0;
    let mut write_kb = 0.0;
    for line in raw.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let maybe_read = parts[parts.len() - 2].parse::<f64>();
        let maybe_write = parts[parts.len() - 1].parse::<f64>();
        if let (Ok(r), Ok(w)) = (maybe_read, maybe_write) {
            read_kb += r;
            write_kb += w;
        }
    }

    if read_kb <= 0.0 && write_kb <= 0.0 {
        return (None, None);
    }

    (
        Some((read_kb * 1024.0).round() as u64),
        Some((write_kb * 1024.0).round() as u64),
    )
}

fn parse_netstat_interface_counters(stdout: &str) -> Vec<InterfaceCounter> {
    let mut header: Option<Vec<&str>> = None;
    let mut per_interface: HashMap<String, (u64, u64)> = HashMap::new();

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

    let mut counters: Vec<InterfaceCounter> = per_interface
        .into_iter()
        .map(|(name, (rx, tx))| InterfaceCounter {
            name,
            rx_bytes_total: rx,
            tx_bytes_total: tx,
        })
        .collect();
    counters.sort_by(|a, b| a.name.cmp(&b.name));
    counters
}

fn sum_interface_totals(counters: &[InterfaceCounter]) -> (Option<u64>, Option<u64>) {
    if counters.is_empty() {
        return (None, None);
    }
    let rx = counters
        .iter()
        .fold(0u64, |acc, row| acc.saturating_add(row.rx_bytes_total));
    let tx = counters
        .iter()
        .fold(0u64, |acc, row| acc.saturating_add(row.tx_bytes_total));
    (Some(rx), Some(tx))
}

#[cfg(feature = "linux-procfs")]
fn collect_linux_procfs_processes(
    state: &Mutex<LinuxCollectorState>,
) -> io::Result<Vec<ProcessRecord>> {
    let (total_jiffies, _) = read_linux_total_and_idle_jiffies()?;
    let mut entries = Vec::new();
    let mut proc_jiffies_now: HashMap<i32, u64> = HashMap::new();

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
        let Some(parsed) = parse_linux_stat(&stat) else {
            continue;
        };
        let ppid = parsed.ppid;
        let proc_jiffies = parsed.utime.saturating_add(parsed.stime);
        proc_jiffies_now.insert(pid, proc_jiffies);

        let comm_path = format!("/proc/{pid}/comm");
        let name = fs::read_to_string(comm_path)
            .map(|raw| raw.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let status_path = format!("/proc/{pid}/status");
        let mem_mb = fs::read_to_string(status_path)
            .ok()
            .and_then(|content| parse_linux_rss_mb(&content))
            .unwrap_or(0.0);

        let io_path = format!("/proc/{pid}/io");
        let (disk_read_bytes_total, disk_write_bytes_total) = fs::read_to_string(io_path)
            .ok()
            .map(|content| parse_linux_proc_io(&content))
            .unwrap_or((None, None));

        entries.push(ProcessRecord {
            pid,
            ppid,
            name,
            cpu: 0.0,
            mem_mb,
            disk_read_bytes_total,
            disk_write_bytes_total,
        });
    }

    let mut guard = state
        .lock()
        .map_err(|_| io::Error::other("linux collector state poisoned"))?;
    if let Some(last_total) = guard.last_total_jiffies {
        let total_delta = total_jiffies.saturating_sub(last_total) as f64;
        if total_delta > 0.0 {
            for entry in &mut entries {
                if let Some(prev_proc) = guard.last_proc_jiffies.get(&entry.pid) {
                    let proc_now = proc_jiffies_now.get(&entry.pid).copied().unwrap_or(0);
                    let proc_delta = proc_now.saturating_sub(*prev_proc) as f64;
                    entry.cpu = (proc_delta / total_delta) * 100.0;
                }
            }
        }
    }

    guard.last_total_jiffies = Some(total_jiffies);
    guard.last_proc_jiffies = proc_jiffies_now;

    Ok(entries)
}

#[cfg(feature = "linux-procfs")]
fn collect_linux_system_stats(
    state: &Mutex<LinuxCollectorState>,
) -> io::Result<Option<SystemStats>> {
    let (total_jiffies, idle_jiffies) = read_linux_total_and_idle_jiffies()?;
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let loadavg = fs::read_to_string("/proc/loadavg")?;
    let uptime_raw = fs::read_to_string("/proc/uptime")?;
    let net_dev = fs::read_to_string("/proc/net/dev")?;
    let diskstats = fs::read_to_string("/proc/diskstats").ok();

    let mut guard = state
        .lock()
        .map_err(|_| io::Error::other("linux collector state poisoned"))?;
    let cpu_total_pct = if let (Some(last_total), Some(last_idle)) =
        (guard.last_total_jiffies, guard.last_idle_jiffies)
    {
        let total_delta = total_jiffies.saturating_sub(last_total) as f64;
        let idle_delta = idle_jiffies.saturating_sub(last_idle) as f64;
        if total_delta > 0.0 {
            Some(((total_delta - idle_delta) / total_delta) * 100.0)
        } else {
            None
        }
    } else {
        None
    };
    guard.last_total_jiffies = Some(total_jiffies);
    guard.last_idle_jiffies = Some(idle_jiffies);

    let mem_total_mb = parse_meminfo_mb(&meminfo, "MemTotal:");
    let mem_available_mb = parse_meminfo_mb(&meminfo, "MemAvailable:");
    let mem_used_mb = match (mem_total_mb, mem_available_mb) {
        (Some(total), Some(available)) => Some((total - available).max(0.0)),
        _ => None,
    };

    let swap_total_mb = parse_meminfo_mb(&meminfo, "SwapTotal:");
    let swap_free_mb = parse_meminfo_mb(&meminfo, "SwapFree:");
    let swap_used_mb = match (swap_total_mb, swap_free_mb) {
        (Some(total), Some(free)) => Some((total - free).max(0.0)),
        _ => None,
    };

    let (load_avg_1, load_avg_5, load_avg_15) = parse_linux_loadavg(&loadavg);
    let uptime_seconds = parse_linux_uptime_seconds(&uptime_raw);

    let interfaces = parse_linux_net_dev(&net_dev);
    let (net_rx_bytes_total, net_tx_bytes_total) = sum_interface_totals(&interfaces);

    let (disk_read_bytes_total, disk_write_bytes_total) = diskstats
        .as_deref()
        .map(parse_linux_diskstats_totals)
        .unwrap_or((None, None));

    Ok(Some(SystemStats {
        cpu_total_pct,
        mem_used_mb,
        mem_total_mb,
        swap_used_mb,
        swap_total_mb,
        load_avg_1,
        load_avg_5,
        load_avg_15,
        uptime_seconds,
        net_rx_bytes_total,
        net_tx_bytes_total,
        disk_read_bytes_total,
        disk_write_bytes_total,
    }))
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_stat_ppid(stat: &str) -> Option<i32> {
    parse_linux_stat(stat).map(|s| s.ppid)
}

#[cfg(feature = "linux-procfs")]
#[derive(Clone, Copy)]
struct LinuxStatFields {
    ppid: i32,
    utime: u64,
    stime: u64,
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_stat(stat: &str) -> Option<LinuxStatFields> {
    let close_idx = stat.rfind(')')?;
    let after_comm = stat.get(close_idx + 2..)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    if fields.len() < 15 {
        return None;
    }

    let ppid = fields.get(1)?.parse::<i32>().ok()?;
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(LinuxStatFields { ppid, utime, stime })
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

#[cfg(feature = "linux-procfs")]
fn parse_linux_proc_io(content: &str) -> (Option<u64>, Option<u64>) {
    let read = parse_linux_proc_io_value(content, "read_bytes:");
    let write = parse_linux_proc_io_value(content, "write_bytes:");
    (read, write)
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_proc_io_value(content: &str, key: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(value) = line.strip_prefix(key) {
            return value.trim().parse::<u64>().ok();
        }
    }
    None
}

#[cfg(feature = "linux-procfs")]
fn read_linux_total_and_idle_jiffies() -> io::Result<(u64, u64)> {
    let stat = fs::read_to_string("/proc/stat")?;
    let Some(line) = stat.lines().next() else {
        return Err(io::Error::other("/proc/stat empty"));
    };
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    if values.len() < 4 {
        return Err(io::Error::other("/proc/stat cpu format invalid"));
    }
    let total = values.iter().copied().sum::<u64>();
    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    Ok((total, idle))
}

#[cfg(feature = "linux-procfs")]
fn parse_meminfo_mb(content: &str, key: &str) -> Option<f64> {
    for line in content.lines() {
        if let Some(raw) = line.strip_prefix(key) {
            let kb = raw
                .trim()
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())?;
            return Some(kb / 1024.0);
        }
    }
    None
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_loadavg(content: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut parts = content.split_whitespace();
    (
        parts.next().and_then(|v| v.parse::<f64>().ok()),
        parts.next().and_then(|v| v.parse::<f64>().ok()),
        parts.next().and_then(|v| v.parse::<f64>().ok()),
    )
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_uptime_seconds(content: &str) -> Option<u64> {
    let first = content.split_whitespace().next()?;
    let seconds = first.parse::<f64>().ok()?;
    Some(seconds.max(0.0).floor() as u64)
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_net_dev(content: &str) -> Vec<InterfaceCounter> {
    let mut rows = Vec::new();
    for line in content.lines().skip(2) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut split = line.split(':');
        let Some(iface) = split.next().map(|v| v.trim()) else {
            continue;
        };
        if iface.starts_with("lo") {
            continue;
        }
        let Some(stats) = split.next() else {
            continue;
        };
        let values: Vec<&str> = stats.split_whitespace().collect();
        if values.len() < 16 {
            continue;
        }
        let Ok(rx) = values[0].parse::<u64>() else {
            continue;
        };
        let Ok(tx) = values[8].parse::<u64>() else {
            continue;
        };
        rows.push(InterfaceCounter {
            name: iface.to_string(),
            rx_bytes_total: rx,
            tx_bytes_total: tx,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

#[cfg(feature = "linux-procfs")]
fn parse_linux_diskstats_totals(content: &str) -> (Option<u64>, Option<u64>) {
    let mut read_bytes = 0u64;
    let mut write_bytes = 0u64;
    let mut found = 0usize;
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 14 {
            continue;
        }
        let dev = parts[2];
        if dev.starts_with("loop") || dev.starts_with("ram") {
            continue;
        }

        let Ok(read_sectors) = parts[5].parse::<u64>() else {
            continue;
        };
        let Ok(write_sectors) = parts[9].parse::<u64>() else {
            continue;
        };

        read_bytes = read_bytes.saturating_add(read_sectors.saturating_mul(512));
        write_bytes = write_bytes.saturating_add(write_sectors.saturating_mul(512));
        found += 1;
    }

    if found == 0 {
        (None, None)
    } else {
        (Some(read_bytes), Some(write_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Collector, CollectorBackend, InterfaceCounter, ProcessRecord, SystemStats,
        parse_macos_loadavg, parse_macos_swapusage, parse_netstat_interface_counters,
        parse_ps_output, parse_vm_stat_used_mb, sum_interface_totals,
    };
    use std::io;

    #[test]
    fn parses_multiple_rows_and_skips_invalid_input() {
        let stdout = "123 1 1.0 2048 top\ninvalid\n777 1 4.5 1024 Finder\n";
        let parsed = parse_ps_output(stdout);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].pid, 123);
        assert_eq!(parsed[0].mem_mb, 2.0);
        assert_eq!(parsed[1].name, "Finder");
        assert_eq!(parsed[0].disk_read_bytes_total, None);
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
        let counters = parse_netstat_interface_counters(netstat);
        assert_eq!(counters.len(), 1);
        assert_eq!(counters[0].name, "en0");
        let (rx, tx) = sum_interface_totals(&counters);
        assert_eq!(rx, Some(1100));
        assert_eq!(tx, Some(2000));
    }

    #[test]
    fn parses_swap_usage_and_load_average() {
        let swap = "total = 1024.00M  used = 256.00M  free = 768.00M (encrypted)";
        let (used, total) = parse_macos_swapusage(swap);
        assert_eq!(used, Some(256.0));
        assert_eq!(total, Some(1024.0));

        let load = "{ 3.50 2.20 1.10 }";
        let (l1, l5, l15) = parse_macos_loadavg(load);
        assert_eq!(l1, Some(3.5));
        assert_eq!(l5, Some(2.2));
        assert_eq!(l15, Some(1.1));
    }

    struct FixtureCollector;

    impl Collector for FixtureCollector {
        fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>> {
            Ok(vec![ProcessRecord {
                pid: 1,
                ppid: 0,
                name: "init".to_string(),
                cpu: 1.0,
                mem_mb: 32.0,
                disk_read_bytes_total: Some(1024),
                disk_write_bytes_total: Some(2048),
            }])
        }

        fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
            Ok(Some(SystemStats {
                cpu_total_pct: Some(25.0),
                mem_used_mb: Some(512.0),
                mem_total_mb: Some(1024.0),
                swap_used_mb: Some(128.0),
                swap_total_mb: Some(256.0),
                load_avg_1: Some(0.5),
                load_avg_5: Some(0.4),
                load_avg_15: Some(0.3),
                uptime_seconds: Some(900),
                net_rx_bytes_total: Some(1_000),
                net_tx_bytes_total: Some(2_000),
                disk_read_bytes_total: Some(3_000),
                disk_write_bytes_total: Some(4_000),
            }))
        }

        fn collect_network_interfaces(&self) -> io::Result<Vec<InterfaceCounter>> {
            Ok(vec![InterfaceCounter {
                name: "eth0".to_string(),
                rx_bytes_total: 100,
                tx_bytes_total: 200,
            }])
        }
    }

    #[test]
    fn collector_contract_supports_processes_system_and_network() {
        let collector = FixtureCollector;
        let procs = collector.collect_processes().expect("process collection");
        assert_eq!(procs.len(), 1);
        assert!(procs[0].disk_read_bytes_total.is_some());

        let system = collector
            .collect_system_stats()
            .expect("system collection")
            .expect("system stats expected");
        assert_eq!(system.load_avg_1, Some(0.5));
        assert_eq!(system.uptime_seconds, Some(900));

        let net = collector
            .collect_network_interfaces()
            .expect("network collection");
        assert_eq!(net.len(), 1);
        assert_eq!(net[0].name, "eth0");
    }

    #[cfg(feature = "linux-procfs")]
    #[test]
    fn linux_backend_name_is_available() {
        assert_eq!(CollectorBackend::LinuxProcfs.as_str(), "linux-procfs");
    }
}
