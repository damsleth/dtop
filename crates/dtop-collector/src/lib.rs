use std::io;

mod mac;
#[cfg(feature = "linux-procfs")]
mod linux;

pub use mac::MacPsCollector;
#[cfg(feature = "linux-procfs")]
pub use linux::LinuxProcfsCollector;

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
        let all = [
            CollectorBackend::MacPs,
            #[cfg(feature = "linux-procfs")]
            CollectorBackend::LinuxProcfs,
        ];
        all.into_iter().find(|b| b.as_str() == name)
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
    let all = [
        CollectorBackend::MacPs,
        #[cfg(feature = "linux-procfs")]
        CollectorBackend::LinuxProcfs,
    ];
    all.iter().map(|b| b.as_str()).collect()
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

#[cfg(test)]
mod tests {
    use super::{
        Collector, CollectorBackend, InterfaceCounter, ProcessRecord, SystemStats,
        sum_interface_totals,
    };
    use crate::mac::{
        parse_macos_loadavg, parse_macos_swapusage, parse_netstat_interface_counters,
        parse_ps_output, parse_vm_stat_used_mb,
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
