use std::collections::HashMap;
use std::thread;
use std::time::Duration;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

#[derive(Debug, Clone, serde::Serialize)]
pub struct CpuMetrics {
    pub usage_pct: f32,
    pub load1: f64,
    pub logical_cores: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemMetrics {
    pub total: u64,
    pub used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// Bytes/s of swap growth (positive = consuming more swap)
    pub swap_rate: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiskMetrics {
    /// Bytes/s read across all processes
    pub read_bps: f64,
    /// Bytes/s written across all processes
    pub write_bps: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NetMetrics {
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub errors: u64,
    pub drops: u64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct ProcSample {
    pub pid: u32,
    pub cpu: f32,
    pub mem: u64,
    pub disk_bps: f64,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct AggProcess {
    pub name: String,
    pub cpu: f32,
    /// bytes (RSS)
    pub mem: u64,
    pub count: usize,
    /// Aggregated disk read+write bytes/sec
    pub disk_bps: f64,
    /// Longest-running PID's uptime, seconds
    pub max_uptime_secs: u64,
    /// Representative command line (from highest-memory PID)
    pub cmd: String,
    /// Top-5 PIDs in this group, sorted by memory desc
    pub samples: Vec<ProcSample>,
    /// Every PID in this group (so actions can target all of them)
    pub all_pids: Vec<u32>,
    /// Owner UID per corresponding entry in `all_pids` (0 if unknown)
    pub all_uids: Vec<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Metrics {
    pub cpu: CpuMetrics,
    pub mem: MemMetrics,
    pub disk: DiskMetrics,
    pub net: NetMetrics,
    pub top_cpu: Vec<AggProcess>,
    pub top_mem: Vec<AggProcess>,
    pub top_disk: Vec<AggProcess>,
}

pub fn collect(sample_interval_ms: u64, top_n: usize) -> Metrics {
    // --- First sample ---
    let mut sys = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything())
            .with_processes(sysinfo::ProcessRefreshKind::everything()),
    );
    sys.refresh_all();

    let mut nets = Networks::new_with_refreshed_list();

    let swap_used_1 = sys.used_swap();

    // Disk I/O sample 1: aggregate across all processes
    let (disk_read_1, disk_write_1) = proc_disk_totals(&sys);

    // Net sample 1
    let (rx_1, tx_1, errs_1, drops_1) = net_totals(&nets);

    thread::sleep(Duration::from_millis(sample_interval_ms));

    // --- Second sample ---
    sys.refresh_all();
    nets.refresh();

    let swap_used_2 = sys.used_swap();
    let secs = sample_interval_ms as f64 / 1000.0;

    let (disk_read_2, disk_write_2) = proc_disk_totals(&sys);
    let (rx_2, tx_2, errs_2, drops_2) = net_totals(&nets);

    // --- CPU ---
    let cpu_usage = sys.global_cpu_usage();
    let load_avg = System::load_average();
    let logical_cores = sys.cpus().len();

    // --- Memory ---
    let swap_delta = swap_used_2.saturating_sub(swap_used_1);
    let swap_rate = (swap_delta as f64 / secs) as u64;

    // --- Disk ---
    let read_bps = delta_rate(disk_read_1, disk_read_2, secs);
    let write_bps = delta_rate(disk_write_1, disk_write_2, secs);

    // --- Network ---
    let rx_bps = delta_rate(rx_1, rx_2, secs);
    let tx_bps = delta_rate(tx_1, tx_2, secs);
    let errors = errs_2.saturating_sub(errs_1);
    let drops = drops_2.saturating_sub(drops_1);

    // --- Processes (aggregate by name) ---
    let mut groups: HashMap<String, AggProcess> = HashMap::new();
    // Track best (highest-memory) cmd source per group
    let mut cmd_winner: HashMap<String, u64> = HashMap::new();

    for proc in sys.processes().values() {
        let raw = proc.name().to_string_lossy().into_owned();
        let name = normalize_name(&raw);
        let cpu_pct = proc.cpu_usage();
        let mem = proc.memory();
        let pid = proc.pid().as_u32();
        let uptime = proc.run_time();
        // disk_usage().{read,written}_bytes is delta since last refresh,
        // i.e. over our sample window. Convert to bytes/sec.
        let du = proc.disk_usage();
        let proc_disk_bps = (du.read_bytes + du.written_bytes) as f64 / secs;

        let entry = groups.entry(name.clone()).or_insert_with(|| AggProcess {
            name: name.clone(),
            ..Default::default()
        });
        entry.cpu += cpu_pct;
        entry.mem += mem;
        entry.count += 1;
        entry.disk_bps += proc_disk_bps;
        if uptime > entry.max_uptime_secs {
            entry.max_uptime_secs = uptime;
        }
        entry.samples.push(ProcSample {
            pid,
            cpu: cpu_pct,
            mem,
            disk_bps: proc_disk_bps,
        });
        entry.all_pids.push(pid);
        // Sysinfo Uid derefs to u32; 0 (root) is a safe fallback for
        // processes whose owner we can't read — they'll show as foreign
        // to a non-root user, which is the correct assumption.
        let uid: u32 = proc.user_id().map(|u| **u).unwrap_or(0);
        entry.all_uids.push(uid);

        // Capture the command line of the process that owns the most memory
        let cmd_str = {
            let cmd = proc.cmd();
            if cmd.is_empty() {
                proc.exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                cmd.iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        let prev_best = cmd_winner.entry(name.clone()).or_insert(0);
        if mem >= *prev_best && !cmd_str.is_empty() {
            *prev_best = mem;
            entry.cmd = cmd_str;
        }
    }

    // Normalise CPU: sysinfo reports per-core %, divide by logical cores
    let cores = logical_cores.max(1) as f32;
    for p in groups.values_mut() {
        p.cpu /= cores;
        for s in &mut p.samples {
            s.cpu /= cores;
        }
        // Sort samples by mem desc, keep top 5
        p.samples.sort_by(|a, b| b.mem.cmp(&a.mem));
        p.samples.truncate(5);
    }

    let mut procs: Vec<AggProcess> = groups.into_values().collect();

    let mut top_cpu = procs.clone();
    top_cpu.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap());
    top_cpu.truncate(top_n);

    let mut top_disk = procs.clone();
    top_disk.sort_by(|a, b| b.disk_bps.partial_cmp(&a.disk_bps).unwrap());
    top_disk.truncate(top_n);

    procs.sort_by(|a, b| b.mem.cmp(&a.mem));
    procs.truncate(top_n);

    Metrics {
        cpu: CpuMetrics {
            usage_pct: cpu_usage,
            load1: load_avg.one,
            logical_cores,
        },
        mem: MemMetrics {
            total: sys.total_memory(),
            used: sys.used_memory(),
            swap_total: sys.total_swap(),
            swap_used: sys.used_swap(),
            swap_rate,
        },
        disk: DiskMetrics {
            read_bps,
            write_bps,
        },
        net: NetMetrics {
            rx_bps,
            tx_bps,
            errors,
            drops,
        },
        top_cpu,
        top_mem: procs,
        top_disk,
    }
}

fn proc_disk_totals(sys: &System) -> (u64, u64) {
    let mut read = 0u64;
    let mut write = 0u64;
    for proc in sys.processes().values() {
        let du = proc.disk_usage();
        read += du.read_bytes;
        write += du.written_bytes;
    }
    (read, write)
}

fn net_totals(nets: &Networks) -> (u64, u64, u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut errs = 0u64;
    let mut drops = 0u64;
    for (_, data) in nets.iter() {
        rx += data.total_received();
        tx += data.total_transmitted();
        errs += data.total_errors_on_received() + data.total_errors_on_transmitted();
        // packets received vs bytes received is not a direct proxy for drops;
        // use the errors fields only — leave drops at 0 to avoid false positives
        let _ = drops; // suppress unused warning below
    }
    drops = 0; // sysinfo 0.32 doesn't expose drop counters reliably
    (rx, tx, errs, drops)
}

fn delta_rate(before: u64, after: u64, secs: f64) -> f64 {
    (after.saturating_sub(before) as f64) / secs
}

/// Collapse common multi-process app names to a canonical label.
fn normalize_name(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("chrome") || lower.contains("chromium") {
        return "Chrome".to_string();
    }
    if lower.contains("firefox") {
        return "Firefox".to_string();
    }
    if lower.contains("safari") {
        return "Safari".to_string();
    }
    raw.trim_end_matches(" Helper")
        .trim_end_matches(" helper")
        .trim_end_matches("_helper")
        .to_string()
}
