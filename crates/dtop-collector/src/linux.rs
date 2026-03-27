use std::collections::HashMap;
use std::fs;
use std::io;
use std::sync::Mutex;

use crate::{Collector, InterfaceCounter, ProcessRecord, SystemStats, sum_interface_totals};

#[derive(Default)]
pub struct LinuxProcfsCollector {
    state: Mutex<LinuxCollectorState>,
}

#[derive(Default)]
struct LinuxCollectorState {
    last_total_jiffies: Option<u64>,
    last_idle_jiffies: Option<u64>,
    last_proc_jiffies: HashMap<i32, u64>,
    /// Cached jiffies from the most recent collect_processes call,
    /// consumed by collect_system_stats to avoid reading /proc/stat twice.
    cached_jiffies: Option<(u64, u64)>,
}

impl Collector for LinuxProcfsCollector {
    fn collect_processes(&self) -> io::Result<Vec<ProcessRecord>> {
        let (total_jiffies, idle_jiffies) = read_linux_total_and_idle_jiffies()?;
        {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| io::Error::other("linux collector state poisoned"))?;
            guard.cached_jiffies = Some((total_jiffies, idle_jiffies));
        }
        collect_linux_procfs_processes(&self.state, total_jiffies)
    }

    fn collect_system_stats(&self) -> io::Result<Option<SystemStats>> {
        collect_linux_system_stats(&self.state)
    }

    fn collect_network_interfaces(&self) -> io::Result<Vec<InterfaceCounter>> {
        let content = fs::read_to_string("/proc/net/dev")?;
        Ok(parse_linux_net_dev(&content))
    }
}

fn collect_linux_procfs_processes(
    state: &Mutex<LinuxCollectorState>,
    total_jiffies: u64,
) -> io::Result<Vec<ProcessRecord>> {
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

    guard.last_proc_jiffies = proc_jiffies_now;
    // last_total_jiffies is updated by collect_linux_system_stats to avoid double-write

    Ok(entries)
}

fn collect_linux_system_stats(
    state: &Mutex<LinuxCollectorState>,
) -> io::Result<Option<SystemStats>> {
    let (total_jiffies, idle_jiffies) = {
        let guard = state
            .lock()
            .map_err(|_| io::Error::other("linux collector state poisoned"))?;
        match guard.cached_jiffies {
            Some(j) => j,
            None => read_linux_total_and_idle_jiffies()?,
        }
    };
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

#[derive(Clone, Copy)]
struct LinuxStatFields {
    ppid: i32,
    utime: u64,
    stime: u64,
}

fn parse_linux_stat(stat: &str) -> Option<LinuxStatFields> {
    let close_idx = stat.rfind(')')?;
    let after_comm = stat.get(close_idx + 2..)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    if fields.len() < 15 {
        return None;
    }

    // /proc/[pid]/stat fields after "(comm) state": index 1=ppid, 11=utime, 12=stime (proc(5))
    let ppid = fields.get(1)?.parse::<i32>().ok()?;
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(LinuxStatFields { ppid, utime, stime })
}

fn parse_linux_rss_mb(status: &str) -> Option<f64> {
    for line in status.lines() {
        if let Some(raw) = line.strip_prefix("VmRSS:") {
            let kb = raw.trim().split_whitespace().next()?.parse::<f64>().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}

fn parse_linux_proc_io(content: &str) -> (Option<u64>, Option<u64>) {
    let read = parse_linux_proc_io_value(content, "read_bytes:");
    let write = parse_linux_proc_io_value(content, "write_bytes:");
    (read, write)
}

fn parse_linux_proc_io_value(content: &str, key: &str) -> Option<u64> {
    for line in content.lines() {
        if let Some(value) = line.strip_prefix(key) {
            return value.trim().parse::<u64>().ok();
        }
    }
    None
}

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

fn parse_linux_loadavg(content: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut parts = content.split_whitespace();
    (
        parts.next().and_then(|v| v.parse::<f64>().ok()),
        parts.next().and_then(|v| v.parse::<f64>().ok()),
        parts.next().and_then(|v| v.parse::<f64>().ok()),
    )
}

fn parse_linux_uptime_seconds(content: &str) -> Option<u64> {
    let first = content.split_whitespace().next()?;
    let seconds = first.parse::<f64>().ok()?;
    Some(seconds.max(0.0).floor() as u64)
}

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

        // /proc/diskstats fields: index 5=sectors read, 9=sectors written (Documentation/iostats.txt)
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
