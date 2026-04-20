use crate::collect::{AggProcess, Metrics, ProcSample};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Culprit {
    pub label: &'static str,
    pub score: u8,
    /// Short stat summary shown in the System Load bar row
    pub detail: String,
    /// One-line blame shown in the Blameboard ("what exactly is causing this")
    pub blame_line: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlameEntry {
    pub name: String,
    pub count: usize,
    pub cpu: f32,
    pub mem: u64,
    pub disk_bps: f64,
    /// 0–100 composite impact score, weighted by subsystem stress
    pub impact: u8,
    /// Which axis drives this entry's guilt: "CPU", "MEM", "DISK", or combos
    pub why: &'static str,
    pub max_uptime_secs: u64,
    pub cmd: String,
    pub samples: Vec<ProcSample>,
    pub all_pids: Vec<u32>,
    pub all_uids: Vec<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub culprits: Vec<Culprit>,
    pub blameboard: Vec<BlameEntry>,
    pub top_cpu: Vec<AggProcess>,
    pub top_mem: Vec<AggProcess>,
    pub verdict: String,
    /// Total system RAM in bytes, propagated for display percent math
    pub total_mem: u64,
    /// How the blameboard was sorted: "CPU" | "MEMORY" | "DISK" | "IMPACT"
    pub sort_basis: &'static str,
}

pub fn diagnose(m: &Metrics) -> Report {
    let cpu_score = score_cpu(m);
    let mem_score = score_memory(m);
    let disk_score = score_disk(m);
    let net_score = score_network(m);

    let mut culprits = vec![
        Culprit {
            label: "CPU",
            score: cpu_score,
            detail: cpu_detail(m),
            blame_line: cpu_blame(m),
        },
        Culprit {
            label: "MEMORY",
            score: mem_score,
            detail: mem_detail(m),
            blame_line: mem_blame(m),
        },
        Culprit {
            label: "DISK",
            score: disk_score,
            detail: disk_detail(m),
            blame_line: disk_blame(m),
        },
        Culprit {
            label: "NETWORK",
            score: net_score,
            detail: net_detail(m),
            blame_line: net_blame(m),
        },
    ];
    culprits.sort_by(|a, b| b.score.cmp(&a.score));

    let (blameboard, sort_basis) = build_blameboard(m, cpu_score, mem_score, disk_score);
    let verdict = build_verdict(&culprits, m);

    Report {
        culprits,
        blameboard,
        top_cpu: m.top_cpu.clone(),
        top_mem: m.top_mem.clone(),
        verdict,
        total_mem: m.mem.total,
        sort_basis,
    }
}

// ---------------------------------------------------------------------------
// Scorers
// ---------------------------------------------------------------------------

fn score_cpu(m: &Metrics) -> u8 {
    let mut s = m.cpu.usage_pct;
    let load_ratio = m.cpu.load1 / m.cpu.logical_cores.max(1) as f64;
    s += if load_ratio > 2.0 {
        25.0
    } else if load_ratio > 1.0 {
        10.0
    } else {
        0.0
    } as f32;
    s.min(100.0) as u8
}

fn score_memory(m: &Metrics) -> u8 {
    // Used RAM alone should not cross CRITICAL: modern OSes intentionally fill
    // RAM with cache and compression. A full RAM bar at 100% maps to 70 here,
    // leaving the top 30 points for the real distress signals: active paging
    // and chronic large swap use.
    let used_pct = if m.mem.total > 0 {
        (m.mem.used as f64 / m.mem.total as f64 * 100.0) as f32
    } else {
        0.0
    };
    let mut s = used_pct * 0.7;

    // Active paging is what actually makes a machine feel slow.
    let swap_mbps = m.mem.swap_rate as f64 / 1_048_576.0;
    if swap_mbps > 10.0 {
        s += 40.0; // hard paging: severe latency
    } else if swap_mbps > 1.0 {
        s += 20.0; // moderate paging
    }

    // Chronic swap use only counts when it's large. macOS routinely keeps a
    // few hundred MB compressed in swap without the user noticing anything.
    let swap_gb = m.mem.swap_used as f64 / 1_073_741_824.0;
    if swap_gb > 2.0 {
        s += 15.0;
    } else if swap_gb > 0.5 {
        s += 5.0;
    }

    s.min(100.0) as u8
}

fn score_disk(m: &Metrics) -> u8 {
    let mbps = (m.disk.read_bps + m.disk.write_bps) / 1_048_576.0;
    let s = if mbps > 200.0 {
        60.0
    } else if mbps > 80.0 {
        40.0
    } else if mbps > 40.0 {
        20.0
    } else {
        mbps / 2.0
    };
    s.min(100.0) as u8
}

fn score_network(m: &Metrics) -> u8 {
    let mbps = (m.net.rx_bps + m.net.tx_bps) / 1_048_576.0;
    let mut s = mbps.min(50.0) as f32;
    if m.net.errors + m.net.drops > 0 {
        s += 30.0;
    }
    s.min(100.0) as u8
}

// ---------------------------------------------------------------------------
// Stat detail strings (shown in System Load bar row)
// ---------------------------------------------------------------------------

fn cpu_detail(m: &Metrics) -> String {
    let load_ratio = m.cpu.load1 / m.cpu.logical_cores.max(1) as f64;
    let mut parts = vec![format!("{:.0}% used", m.cpu.usage_pct)];
    if load_ratio > 2.0 {
        parts.push(format!(
            "load {:.1} on {} cores: queue congested",
            m.cpu.load1, m.cpu.logical_cores
        ));
    } else if load_ratio > 1.0 {
        parts.push(format!("load {:.1}: slightly oversubscribed", m.cpu.load1));
    } else {
        parts.push(format!("load {:.1}", m.cpu.load1));
    }
    parts.join(", ")
}

fn mem_detail(m: &Metrics) -> String {
    let used_gb = m.mem.used as f64 / 1_073_741_824.0;
    let total_gb = m.mem.total as f64 / 1_073_741_824.0;
    let mut s = format!("{:.1}/{:.1} GB", used_gb, total_gb);
    if m.mem.swap_used > 0 {
        let swap_gb = m.mem.swap_used as f64 / 1_073_741_824.0;
        s.push_str(&format!("  swap: {:.1} GB", swap_gb));
        if m.mem.swap_rate > 1_000_000 {
            s.push_str(" (PAGING)");
        }
    }
    s
}

fn disk_detail(m: &Metrics) -> String {
    format!(
        "R: {}  W: {}",
        fmt_bps(m.disk.read_bps),
        fmt_bps(m.disk.write_bps)
    )
}

fn net_detail(m: &Metrics) -> String {
    let mut s = format!(
        "in: {}  out: {}",
        fmt_bps(m.net.rx_bps),
        fmt_bps(m.net.tx_bps)
    );
    if m.net.errors + m.net.drops > 0 {
        s.push_str(&format!("  err: {}  drop: {}", m.net.errors, m.net.drops));
    }
    s
}

// ---------------------------------------------------------------------------
// Blame one-liners (shown in Blameboard)
// ---------------------------------------------------------------------------

fn cpu_blame(m: &Metrics) -> String {
    let load_ratio = m.cpu.load1 / m.cpu.logical_cores.max(1) as f64;
    let load_str = if load_ratio > 2.0 {
        format!(
            "load {:.1}/{} cores: tasks queuing",
            m.cpu.load1, m.cpu.logical_cores
        )
    } else if load_ratio > 1.0 {
        format!(
            "load {:.1}/{} cores: oversubscribed",
            m.cpu.load1, m.cpu.logical_cores
        )
    } else {
        format!("{:.0}% total CPU", m.cpu.usage_pct)
    };

    match m.top_cpu.first() {
        Some(p) => format!(
            "{} ({} proc{}, {:.1}% CPU)  {}",
            p.name,
            p.count,
            if p.count == 1 { "" } else { "s" },
            p.cpu,
            load_str,
        ),
        None => load_str,
    }
}

fn mem_blame(m: &Metrics) -> String {
    let used_pct = m.mem.used as f64 / m.mem.total.max(1) as f64 * 100.0;
    let reason = if m.mem.swap_rate > 1_000_000 {
        "ACTIVELY PAGING TO DISK".to_string()
    } else if m.mem.swap_used > 0 {
        let swap_gb = m.mem.swap_used as f64 / 1_073_741_824.0;
        format!(
            "{:.0}% RAM used, {:.1} GB spilled to swap",
            used_pct, swap_gb
        )
    } else {
        format!("{:.0}% RAM used", used_pct)
    };

    match m.top_mem.first() {
        Some(p) => format!(
            "{} ({} proc{}, {})  {}",
            p.name,
            p.count,
            if p.count == 1 { "" } else { "s" },
            fmt_bytes(p.mem),
            reason,
        ),
        None => reason,
    }
}

fn disk_blame(m: &Metrics) -> String {
    let mbps = (m.disk.read_bps + m.disk.write_bps) / 1_048_576.0;
    if mbps < 0.1 {
        "Minimal disk activity".to_string()
    } else {
        format!(
            "Read: {}  Write: {}  ({:.0} MB/s total)",
            fmt_bps(m.disk.read_bps),
            fmt_bps(m.disk.write_bps),
            mbps
        )
    }
}

fn net_blame(m: &Metrics) -> String {
    if m.net.errors + m.net.drops > 0 {
        format!(
            "{} error(s), {} drop(s): packet loss causing retransmits",
            m.net.errors, m.net.drops
        )
    } else {
        let mbps = (m.net.rx_bps + m.net.tx_bps) / 1_048_576.0;
        if mbps < 0.01 {
            "Minimal network activity".to_string()
        } else {
            format!(
                "In: {}  Out: {}",
                fmt_bps(m.net.rx_bps),
                fmt_bps(m.net.tx_bps)
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Blameboard: unified per-process ranking by weighted impact
// ---------------------------------------------------------------------------

fn build_blameboard(
    m: &Metrics,
    cpu_score: u8,
    mem_score: u8,
    disk_score: u8,
) -> (Vec<BlameEntry>, &'static str) {
    use std::collections::HashMap;

    let mut seen: HashMap<String, BlameEntry> = HashMap::new();

    for p in m
        .top_cpu
        .iter()
        .chain(m.top_mem.iter())
        .chain(m.top_disk.iter())
    {
        let entry = seen.entry(p.name.clone()).or_insert_with(|| BlameEntry {
            name: p.name.clone(),
            count: p.count,
            cpu: p.cpu,
            mem: p.mem,
            disk_bps: p.disk_bps,
            impact: 0,
            why: "",
            max_uptime_secs: p.max_uptime_secs,
            cmd: p.cmd.clone(),
            samples: p.samples.clone(),
            all_pids: p.all_pids.clone(),
            all_uids: p.all_uids.clone(),
        });
        if p.count > entry.count {
            entry.count = p.count;
        }
        if p.cpu > entry.cpu {
            entry.cpu = p.cpu;
        }
        if p.mem > entry.mem {
            entry.mem = p.mem;
        }
        if p.disk_bps > entry.disk_bps {
            entry.disk_bps = p.disk_bps;
        }
        if p.max_uptime_secs > entry.max_uptime_secs {
            entry.max_uptime_secs = p.max_uptime_secs;
        }
        if entry.cmd.is_empty() && !p.cmd.is_empty() {
            entry.cmd = p.cmd.clone();
        }
        if entry.samples.is_empty() && !p.samples.is_empty() {
            entry.samples = p.samples.clone();
        }
        if entry.all_pids.is_empty() && !p.all_pids.is_empty() {
            entry.all_pids = p.all_pids.clone();
        }
        if entry.all_uids.is_empty() && !p.all_uids.is_empty() {
            entry.all_uids = p.all_uids.clone();
        }
    }

    // Compute raw impact components in comparable units (0..100 scale).
    // Each component is boosted when its subsystem is stressed.
    let total_mem = m.mem.total.max(1) as f64;
    let cpu_boost = 1.0 + cpu_score as f64 / 50.0;
    let mem_boost = 1.0 + mem_score as f64 / 50.0;
    let disk_boost = 1.0 + disk_score as f64 / 50.0;

    for entry in seen.values_mut() {
        let cpu_component = entry.cpu as f64 * cpu_boost;
        let mem_pct = entry.mem as f64 / total_mem * 100.0;
        let mem_component = mem_pct * mem_boost;
        // Disk: 100 MB/s = full impact contribution
        let disk_mbps = entry.disk_bps / 1_048_576.0;
        let disk_component = disk_mbps * disk_boost;

        entry.impact = cpu_component
            .max(mem_component)
            .max(disk_component)
            .min(100.0) as u8;

        let dominant_cpu = cpu_component >= 10.0;
        let dominant_mem = mem_component >= 10.0;
        let dominant_disk = disk_component >= 10.0;
        entry.why = match (dominant_cpu, dominant_mem, dominant_disk) {
            (true, true, true) => "CPU+MEM+DISK",
            (true, true, false) => "CPU+MEM",
            (true, false, true) => "CPU+DISK",
            (false, true, true) => "MEM+DISK",
            (true, false, false) => "CPU",
            (false, true, false) => "MEM",
            (false, false, true) => "DISK",
            (false, false, false) => {
                if cpu_component >= mem_component && cpu_component >= disk_component {
                    "cpu"
                } else if mem_component >= disk_component {
                    "mem"
                } else {
                    "disk"
                }
            }
        };
    }

    let mut entries: Vec<BlameEntry> = seen.into_values().collect();

    // Dominant-axis sort: if one subsystem is clearly the bottleneck
    // (score >= 40 AND leads the runner-up by 20+ points) then rank
    // processes by that axis's raw magnitude so the culprits for *that*
    // bottleneck surface first. Otherwise fall back to composite impact.
    let scores = [
        ("CPU", cpu_score),
        ("MEMORY", mem_score),
        ("DISK", disk_score),
    ];
    let mut sorted = scores;
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    let (top_label, top_score) = sorted[0];
    let second_score = sorted[1].1;
    let dominant_clear = top_score >= 40 && (top_score as i32 - second_score as i32) >= 20;

    let basis: &'static str = if dominant_clear {
        match top_label {
            "CPU" => {
                entries.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap());
                "CPU"
            }
            "MEMORY" => {
                entries.sort_by(|a, b| b.mem.cmp(&a.mem));
                "MEMORY"
            }
            "DISK" => {
                entries.sort_by(|a, b| b.disk_bps.partial_cmp(&a.disk_bps).unwrap());
                "DISK"
            }
            _ => {
                entries.sort_by(|a, b| b.impact.cmp(&a.impact));
                "IMPACT"
            }
        }
    } else {
        entries.sort_by(|a, b| b.impact.cmp(&a.impact));
        "IMPACT"
    };

    (entries, basis)
}

// ---------------------------------------------------------------------------
// Verdict (used in JSON output)
// ---------------------------------------------------------------------------

fn build_verdict(culprits: &[Culprit], m: &Metrics) -> String {
    let top = culprits.first().map(|c| c.score).unwrap_or(0);
    if top < 20 {
        return "System appears healthy. No significant bottlenecks detected.".to_string();
    }

    let mut sentences: Vec<String> = Vec::new();

    for c in culprits.iter().filter(|c| c.score >= 40).take(2) {
        match c.label {
            "CPU" => {
                let load_ratio = m.cpu.load1 / m.cpu.logical_cores.max(1) as f64;
                if load_ratio > 2.0 {
                    sentences.push(format!(
                        "CPU is the primary bottleneck: load average {:.1} on {} cores means tasks are queuing.",
                        m.cpu.load1, m.cpu.logical_cores
                    ));
                } else {
                    sentences.push(format!("CPU usage is high at {:.0}%.", m.cpu.usage_pct));
                }
                if let Some(p) = m.top_cpu.first() {
                    sentences.push(format!(
                        "{} ({} proc{}) is consuming {:.1}% CPU.",
                        p.name,
                        p.count,
                        if p.count == 1 { "" } else { "s" },
                        p.cpu
                    ));
                }
            }
            "MEMORY" => {
                let used_pct = m.mem.used as f64 / m.mem.total.max(1) as f64 * 100.0;
                if m.mem.swap_rate > 1_000_000 {
                    sentences.push(
                        "RAM is exhausted and the system is actively paging to swap. Severe latency expected.".to_string(),
                    );
                } else if m.mem.swap_used > 0 {
                    sentences.push(format!(
                        "Memory at {:.0}%: spilled into swap, currently stable but at risk.",
                        used_pct
                    ));
                } else {
                    sentences.push(format!("Memory usage is high at {:.0}%.", used_pct));
                }
                if let Some(p) = m.top_mem.first() {
                    let mem_gb = p.mem as f64 / 1_073_741_824.0;
                    sentences.push(format!(
                        "{} ({} proc{}) holds {:.1} GB.",
                        p.name,
                        p.count,
                        if p.count == 1 { "" } else { "s" },
                        mem_gb
                    ));
                }
            }
            "DISK" => {
                let mbps = (m.disk.read_bps + m.disk.write_bps) / 1_048_576.0;
                sentences.push(format!("Disk I/O is heavy at {:.0} MB/s total.", mbps));
            }
            "NETWORK" => {
                if m.net.errors + m.net.drops > 0 {
                    sentences.push(format!(
                        "Network has {} error(s) and {} drop(s). Packet loss may cause retransmits.",
                        m.net.errors, m.net.drops
                    ));
                } else {
                    let mbps = (m.net.rx_bps + m.net.tx_bps) / 1_048_576.0;
                    sentences.push(format!("Network throughput is {:.0} MB/s.", mbps));
                }
            }
            _ => {}
        }
    }

    if sentences.is_empty() {
        "System load is moderate. Monitor for changes.".to_string()
    } else {
        sentences.join("  ")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn fmt_bps(bps: f64) -> String {
    if bps >= 1_073_741_824.0 {
        format!("{:.1} GB/s", bps / 1_073_741_824.0)
    } else if bps >= 1_048_576.0 {
        format!("{:.1} MB/s", bps / 1_048_576.0)
    } else if bps >= 1_024.0 {
        format!("{:.0} KB/s", bps / 1_024.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.0} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.0} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{} B", bytes)
    }
}
