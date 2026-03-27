use std::collections::HashMap;
use std::io;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use dtop_core::parse_ps_line;

use crate::{Collector, InterfaceCounter, ProcessRecord, SystemStats, sum_interface_totals};

const PS_AXO_FIELDS: &str = "pid=,ppid=,%cpu=,rss=,comm=";

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
        let processes = self.collect_processes()?;
        let cpu_total_pct = cpu_total_from_processes(&processes);
        let (mem_used_mb, mem_total_mb) = collect_memory_stats_mb()?;
        let (swap_used_mb, swap_total_mb) = collect_swap_stats_mb()?;
        let (load_avg_1, load_avg_5, load_avg_15) = collect_macos_load_avg()?;
        let uptime_seconds = collect_macos_uptime_seconds()?;
        let (net_rx_bytes_total, net_tx_bytes_total) = {
            let ifaces = self.collect_network_interfaces().unwrap_or_default();
            sum_interface_totals(&ifaces)
        };
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
            return Err(io::Error::other(format!(
                "netstat failed with status {}",
                output.status
            )));
        }
        Ok(parse_netstat_interface_counters(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

pub(crate) fn parse_ps_output(stdout: &str) -> Vec<ProcessRecord> {
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

fn cpu_total_from_processes(processes: &[ProcessRecord]) -> Option<f64> {
    if processes.is_empty() {
        return None;
    }
    Some(processes.iter().map(|p| p.cpu).sum())
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

pub(crate) fn parse_vm_stat_used_mb(stdout: &str) -> Option<f64> {
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

pub(crate) fn parse_macos_swapusage(raw: &str) -> (Option<f64>, Option<f64>) {
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

pub(crate) fn parse_macos_loadavg(raw: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
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

pub(crate) fn parse_netstat_interface_counters(stdout: &str) -> Vec<InterfaceCounter> {
    let mut col_indices: Option<(usize, usize, usize)> = None;
    let mut per_interface: HashMap<String, (u64, u64)> = HashMap::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        if parts[0] == "Name" {
            let name_idx = parts.iter().position(|c| *c == "Name");
            let rx_idx = parts.iter().position(|c| *c == "Ibytes");
            let tx_idx = parts.iter().position(|c| *c == "Obytes");
            if let (Some(n), Some(r), Some(t)) = (name_idx, rx_idx, tx_idx) {
                col_indices = Some((n, r, t));
            }
            continue;
        }
        let Some((name_idx, rx_idx, tx_idx)) = col_indices else {
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
