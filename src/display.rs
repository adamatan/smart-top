use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::{io, time::Duration};

use crate::actions::{self, Action};
use crate::diagnose::{diagnose, fmt_bps, fmt_bytes, BlameEntry, Culprit, Report};

const HISTORY_LEN: usize = 24;
const SPARK_CHARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// ---------------------------------------------------------------------------
// View state — selection, history, prev report, view mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    Dashboard,
    ProcessDetail,
}

struct UiState {
    selected_name: Option<String>,
    mode: ViewMode,
    /// Per-process impact history (rightmost = newest)
    impact_history: HashMap<String, VecDeque<u8>>,
    /// Per-subsystem score history
    load_history: HashMap<String, VecDeque<u8>>,
    /// Previous report — used for delta annotations
    prev_report: Option<Report>,
    /// Transient feedback line shown after running an action
    status: Option<(String, Instant, bool /*ok*/)>,
    /// Effective UID at startup (0 when running as root)
    current_uid: u32,
    /// Username resolved from current_uid, or None if unresolvable
    current_username: Option<String>,
}

impl UiState {
    fn new() -> Self {
        Self {
            selected_name: None,
            mode: ViewMode::Dashboard,
            impact_history: HashMap::new(),
            load_history: HashMap::new(),
            prev_report: None,
            status: None,
            current_uid: actions::current_uid(),
            current_username: actions::username_for(actions::current_uid()),
        }
    }

    fn is_root(&self) -> bool {
        self.current_uid == 0
    }

    fn set_status(&mut self, message: String, ok: bool) {
        self.status = Some((message, Instant::now(), ok));
    }

    fn current_status(&self) -> Option<(&str, bool)> {
        self.status.as_ref().and_then(|(m, t, ok)| {
            if t.elapsed() < Duration::from_secs(5) {
                Some((m.as_str(), *ok))
            } else {
                None
            }
        })
    }

    fn ingest(&mut self, report: &Report) {
        for e in &report.blameboard {
            let buf = self
                .impact_history
                .entry(e.name.clone())
                .or_insert_with(VecDeque::new);
            buf.push_back(e.impact);
            while buf.len() > HISTORY_LEN {
                buf.pop_front();
            }
        }
        for c in &report.culprits {
            let buf = self
                .load_history
                .entry(c.label.to_string())
                .or_insert_with(VecDeque::new);
            buf.push_back(c.score);
            while buf.len() > HISTORY_LEN {
                buf.pop_front();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub fn render_once(metrics: &crate::collect::Metrics) -> io::Result<()> {
    let report = diagnose(metrics);
    let mut state = UiState::new();
    state.ingest(&report);
    if let Some(e) = report.blameboard.first() {
        state.selected_name = Some(e.name.clone());
    }

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    loop {
        let visible = report.blameboard.len().min(10);
        terminal.draw(|f| draw(f, &report, &state))?;

        if event::poll(Duration::from_secs(60))? {
            if let Event::Key(key) = event::read()? {
                if !handle_key(key.code, key.modifiers, &mut state, &report, visible) {
                    break;
                }
            }
        } else {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

pub fn render_watch(interval_secs: u64, top_n: usize, _no_color: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Adaptive refresh: shared atomic that the UI updates from latest
    // severity, and the collector reads to set its sleep duration.
    let sleep_ms = Arc::new(AtomicU64::new(interval_secs.max(1) * 1000));
    let sleep_ms_for_thread = Arc::clone(&sleep_ms);

    let (tx, rx) = std::sync::mpsc::channel::<Report>();
    std::thread::spawn(move || loop {
        let metrics = crate::collect::collect(1_000, top_n);
        let report = diagnose(&metrics);
        if tx.send(report).is_err() {
            break;
        }
        let dur = Duration::from_millis(sleep_ms_for_thread.load(Ordering::Relaxed).max(100));
        std::thread::sleep(dur);
    });

    let mut state = UiState::new();
    let mut report: Option<Report> = None;

    loop {
        // Drain pending reports — keep the most recent
        let mut got_new = false;
        while let Ok(r) = rx.try_recv() {
            if let Some(prev) = report.take() {
                state.prev_report = Some(prev);
            }
            state.ingest(&r);
            report = Some(r);
            got_new = true;
        }

        // Adapt refresh interval to severity
        if got_new {
            if let Some(ref r) = report {
                let max_score = r.culprits.iter().map(|c| c.score).max().unwrap_or(0);
                let new_sleep = if max_score >= 70 {
                    250
                } else if max_score >= 40 {
                    1_000
                } else {
                    (interval_secs.max(2)) * 1_000
                };
                sleep_ms.store(new_sleep, Ordering::Relaxed);
            }
        }

        // Resolve selection from name
        let visible = report
            .as_ref()
            .map(|r| r.blameboard.len().min(10))
            .unwrap_or(0);
        if state.selected_name.is_none() {
            if let Some(ref r) = report {
                if let Some(e) = r.blameboard.first() {
                    state.selected_name = Some(e.name.clone());
                }
            }
        }

        if let Some(ref r) = report {
            terminal.draw(|f| draw(f, r, &state))?;
        } else {
            terminal.draw(|f| draw_splash(f))?;
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let cont = match &report {
                    Some(r) => handle_key(key.code, key.modifiers, &mut state, r, visible),
                    None => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => false,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            false
                        }
                        _ => true,
                    },
                };
                if !cont {
                    break;
                }
            }
        }
    }

    drop(rx);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

/// Returns `false` if the program should exit.
fn handle_key(
    code: KeyCode,
    mods: KeyModifiers,
    state: &mut UiState,
    report: &Report,
    visible: usize,
) -> bool {
    match (state.mode, code) {
        (_, KeyCode::Char('q')) => return false,
        (_, KeyCode::Char('c')) if mods.contains(KeyModifiers::CONTROL) => return false,
        (ViewMode::ProcessDetail, KeyCode::Esc) => state.mode = ViewMode::Dashboard,
        (ViewMode::Dashboard, KeyCode::Esc) => return false,
        (ViewMode::Dashboard, KeyCode::Enter) => state.mode = ViewMode::ProcessDetail,
        (ViewMode::ProcessDetail, KeyCode::Enter) => state.mode = ViewMode::Dashboard,
        (_, KeyCode::Up) => move_selection(state, report, visible, -1),
        (_, KeyCode::Down) => move_selection(state, report, visible, 1),
        (_, KeyCode::Char(ch)) => {
            if let Some(action) = Action::menu().into_iter().find(|a| a.key() == ch) {
                run_selected_action(action, state, report);
            }
        }
        _ => {}
    }
    true
}

fn run_selected_action(action: Action, state: &mut UiState, report: &Report) {
    let entry = state
        .selected_name
        .as_ref()
        .and_then(|n| report.blameboard.iter().find(|e| &e.name == n))
        .or_else(|| report.blameboard.first());
    let Some(e) = entry else {
        state.set_status("No process selected".to_string(), false);
        return;
    };
    let result = actions::apply(action, &e.name, &e.all_pids);
    state.set_status(
        format!("{}: {}", e.name, result.message),
        result.ok,
    );
}

fn move_selection(state: &mut UiState, report: &Report, visible: usize, delta: i32) {
    if visible == 0 {
        return;
    }
    let cur = state
        .selected_name
        .as_ref()
        .and_then(|n| {
            report
                .blameboard
                .iter()
                .take(visible)
                .position(|e| &e.name == n)
        })
        .unwrap_or(0) as i32;
    let new = (cur + delta).clamp(0, visible as i32 - 1) as usize;
    if let Some(e) = report.blameboard.get(new) {
        state.selected_name = Some(e.name.clone());
    }
}

// ---------------------------------------------------------------------------
// Splash
// ---------------------------------------------------------------------------

fn draw_splash(f: &mut Frame) {
    let area = f.area();
    let block = Block::default().borders(Borders::ALL);
    f.render_widget(block, area);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "stop",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            "why is your computer slow?",
            Style::default().fg(Color::Gray),
        ))
        .alignment(Alignment::Center),
        Line::from(""),
        Line::from(Span::styled(
            "sampling system metrics, one moment…",
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Center),
    ];

    // Center vertically
    let inner = Rect {
        x: area.x + 1,
        y: area.y + (area.height / 2).saturating_sub(3),
        width: area.width.saturating_sub(2),
        height: 5.min(area.height.saturating_sub(2)),
    };
    let p = Paragraph::new(lines);
    f.render_widget(p, inner);
}

// ---------------------------------------------------------------------------
// Top-level layout
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, report: &Report, state: &UiState) {
    if state.mode == ViewMode::ProcessDetail {
        draw_process_detail_fullscreen(f, report, state);
        return;
    }
    draw_dashboard(f, report, state);
}

fn draw_dashboard(f: &mut Frame, report: &Report, state: &UiState) {

    let area = f.area();

    let load_collapsed = report.culprits.iter().all(|c| c.score < 40);
    let load_height: u16 = if load_collapsed {
        3
    } else {
        report.culprits.len() as u16 + 2
    };
    let details_height = 11u16;
    let footer_height = 1u16;
    let hint_height: u16 = if report.blameboard.iter().any(|e| e.impact >= 40) {
        1
    } else {
        0
    };

    let actions_height = Action::menu().len() as u16 + 3; // actions + footer note + 2 border
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(hint_height),
            Constraint::Length(details_height),
            Constraint::Length(load_height),
            Constraint::Length(actions_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

    draw_blameboard(f, report, state, chunks[0]);
    if hint_height > 0 {
        draw_action_hint(f, report, chunks[1]);
    }
    draw_details(f, report, state, chunks[2]);
    if load_collapsed {
        draw_system_load_collapsed(f, report, chunks[3]);
    } else {
        draw_system_load(f, report, state, chunks[3]);
    }
    draw_actions_bar(f, report, state, chunks[4]);
    draw_footer(f, state, chunks[5]);
}

// ---------------------------------------------------------------------------
// Blameboard
// ---------------------------------------------------------------------------

fn draw_blameboard(f: &mut Frame, report: &Report, state: &UiState, area: Rect) {
    let entries: &[BlameEntry] = &report.blameboard;
    let inner_rows = area.height.saturating_sub(3) as usize;
    let visible = entries.len().min(inner_rows).min(10);

    let hstyle = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let header = Row::new(vec![
        Cell::from(" # ").style(hstyle),
        Cell::from("Process").style(hstyle),
        Cell::from("Why").style(hstyle),
        Cell::from(" CPU%").style(hstyle),
        Cell::from("  Memory").style(hstyle),
        Cell::from("  Disk").style(hstyle),
        Cell::from("Trend").style(hstyle),
    ]);

    let total_mem = if let Some(c) = report.culprits.iter().find(|c| c.label == "MEMORY") {
        c.score // unused — we'll pull real total from Report below
    } else {
        0
    };
    let _ = total_mem;
    let total_ram = report.total_mem;

    let prev_lookup: HashMap<&str, &BlameEntry> = state
        .prev_report
        .as_ref()
        .map(|r| r.blameboard.iter().map(|e| (e.name.as_str(), e)).collect())
        .unwrap_or_default();

    let rows: Vec<Row> = entries[..visible]
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let prev = prev_lookup.get(e.name.as_str()).copied();
            let history = state
                .impact_history
                .get(&e.name)
                .map(|v| v.iter().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            blame_entry_row(i + 1, e, prev, &history, total_ram)
        })
        .collect();

    let title = match report.sort_basis {
        "IMPACT" => " BLAMEBOARD — who is making your computer slow ".to_string(),
        "CPU" => " BLAMEBOARD — sorted by CPU usage (the bottleneck) ".to_string(),
        "MEMORY" => " BLAMEBOARD — sorted by memory use (the bottleneck) ".to_string(),
        "DISK" => " BLAMEBOARD — sorted by disk I/O (the bottleneck) ".to_string(),
        other => format!(" BLAMEBOARD — sorted by {} ", other.to_lowercase()),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            title,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(24),
            Constraint::Length(10),
            Constraint::Length(13),
            Constraint::Length(18),
            Constraint::Length(11),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(
        Style::default()
            .bg(Color::Rgb(50, 50, 80))
            .add_modifier(Modifier::BOLD),
    );

    let mut tstate = TableState::default();
    if visible > 0 {
        let cur = state
            .selected_name
            .as_ref()
            .and_then(|n| {
                report
                    .blameboard
                    .iter()
                    .take(visible)
                    .position(|e| &e.name == n)
            })
            .unwrap_or(0);
        tstate.select(Some(cur));
    }

    f.render_stateful_widget(table, area, &mut tstate);
}

fn blame_entry_row(
    rank: usize,
    e: &BlameEntry,
    prev: Option<&BlameEntry>,
    history: &[u8],
    total_ram: u64,
) -> Row<'static> {
    let is_critical = e.impact >= 70;
    let is_warn = e.impact >= 40;
    let is_culprit = is_warn;
    let color = score_color(e.impact);
    let is_top = rank == 1 && is_culprit;

    let rank_cell = if is_top {
        Cell::from(Line::from(vec![Span::styled(
            "\u{25B6} 1 ",
            Style::default()
                .fg(color)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )]))
    } else if is_culprit {
        Cell::from(format!("{:>2} ", rank))
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
    } else {
        Cell::from(" \u{00B7} ").style(Style::default().fg(Color::DarkGray))
    };

    let proc_label = if e.count > 1 {
        format!("{} ({})", e.name, e.count)
    } else {
        e.name.clone()
    };
    let name_style = if is_critical {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else if is_warn {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let name_cell = Cell::from(proc_label).style(name_style);

    // Why tag
    let why_cell = if is_culprit {
        let why_upper = e.why.to_uppercase();
        let why_color = if why_upper.contains('+') {
            Color::LightRed
        } else {
            match why_upper.as_str() {
                "CPU" => Color::Cyan,
                "MEM" => Color::Magenta,
                "DISK" => Color::Yellow,
                _ => Color::DarkGray,
            }
        };
        Cell::from(why_upper).style(Style::default().fg(why_color).add_modifier(Modifier::BOLD))
    } else {
        Cell::from("").style(Style::default().fg(Color::DarkGray))
    };

    // CPU% with delta
    let cpu_delta = prev.map(|p| e.cpu - p.cpu).unwrap_or(0.0);
    let cpu_arrow = delta_arrow_f32(cpu_delta, 5.0);
    let cpu_cell = Cell::from(Line::from(vec![
        Span::styled(
            format!("{:>5.1}%", e.cpu),
            if is_culprit {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            cpu_arrow.0,
            Style::default().fg(cpu_arrow.1).add_modifier(Modifier::BOLD),
        ),
    ]));

    // Memory with delta + percent of total RAM
    let mem_pct = if total_ram > 0 {
        e.mem as f64 / total_ram as f64 * 100.0
    } else {
        0.0
    };
    let mem_delta_bytes = prev.map(|p| e.mem as i64 - p.mem as i64).unwrap_or(0);
    let mem_arrow = delta_arrow_bytes(mem_delta_bytes, 100 * 1024 * 1024); // 100 MB threshold

    let mem_text = if mem_pct >= 1.0 {
        format!("{:>8} ({:>2.0}%)", fmt_bytes(e.mem), mem_pct)
    } else {
        format!("{:>8}      ", fmt_bytes(e.mem))
    };
    let mem_cell = Cell::from(Line::from(vec![
        Span::styled(
            mem_text,
            if is_culprit {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            mem_arrow.0,
            Style::default().fg(mem_arrow.1).add_modifier(Modifier::BOLD),
        ),
    ]));

    // Disk
    let disk_text = if e.disk_bps >= 1024.0 {
        fmt_bps(e.disk_bps)
    } else {
        String::new()
    };
    let disk_cell = Cell::from(format!("{:>10}", disk_text)).style(if e.disk_bps >= 1_048_576.0 {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if !disk_text.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::DarkGray)
    });

    // Sparkline trend
    let spark = sparkline(history, 10, color, is_culprit);
    let spark_cell = Cell::from(Line::from(spark));

    Row::new(vec![
        rank_cell,
        name_cell,
        why_cell,
        cpu_cell,
        mem_cell,
        disk_cell,
        spark_cell,
    ])
}

// ---------------------------------------------------------------------------
// Action hint — single line under blameboard with actionable advice
// ---------------------------------------------------------------------------

fn draw_action_hint(f: &mut Frame, report: &Report, area: Rect) {
    let hint = match report.blameboard.first() {
        Some(e) if e.impact >= 40 => build_action_hint(e, report.total_mem),
        _ => String::new(),
    };
    if hint.is_empty() {
        return;
    }
    let p = Paragraph::new(Line::from(vec![
        Span::styled(
            "  → ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(hint, Style::default().fg(Color::White)),
    ]));
    f.render_widget(p, area);
}

fn build_action_hint(e: &BlameEntry, total_ram: u64) -> String {
    let why = e.why.to_uppercase();
    let count_str = if e.count > 1 {
        format!(" ({} processes)", e.count)
    } else {
        String::new()
    };

    if why.contains("MEM") {
        let pct = if total_ram > 0 {
            e.mem as f64 / total_ram as f64 * 100.0
        } else {
            0.0
        };
        return format!(
            "{}{} is using {:.0}% of RAM ({}). Quitting it would free that memory.",
            e.name,
            count_str,
            pct,
            fmt_bytes(e.mem),
        );
    }
    if why.contains("DISK") {
        return format!(
            "{}{} is hammering disk at {}. That can stall the whole system.",
            e.name,
            count_str,
            fmt_bps(e.disk_bps),
        );
    }
    if why.contains("CPU") {
        return format!(
            "{}{} is consuming {:.1}% CPU. Killing or pausing it would free that compute.",
            e.name, count_str, e.cpu,
        );
    }
    format!("{}{} is the top current load.", e.name, count_str)
}

// ---------------------------------------------------------------------------
// Details panel
// ---------------------------------------------------------------------------

fn draw_details(f: &mut Frame, report: &Report, state: &UiState, area: Rect) {
    let entry: Option<&BlameEntry> = state
        .selected_name
        .as_ref()
        .and_then(|n| report.blameboard.iter().find(|e| &e.name == n))
        .or_else(|| report.blameboard.first());

    let inner_width = area.width.saturating_sub(4) as usize;

    let lines: Vec<Line> = match entry {
        None => vec![Line::from(Span::styled(
            "  No process selected",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(e) => {
            let mut out: Vec<Line> = Vec::new();

            let mem_pct = if report.total_mem > 0 {
                e.mem as f64 / report.total_mem as f64 * 100.0
            } else {
                0.0
            };

            out.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    format!("{} process{}", e.count, if e.count == 1 { "" } else { "es" }),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("RAM: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{} ({:.1}% of {})",
                        fmt_bytes(e.mem),
                        mem_pct,
                        fmt_bytes(report.total_mem)
                    ),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("CPU: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:.1}%", e.cpu),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("Disk: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    if e.disk_bps >= 1024.0 {
                        fmt_bps(e.disk_bps)
                    } else {
                        "idle".to_string()
                    },
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw("   "),
                Span::styled("Up: ", Style::default().fg(Color::DarkGray)),
                Span::styled(fmt_uptime(e.max_uptime_secs), Style::default().fg(Color::Yellow)),
            ]));

            let cmd = if e.cmd.is_empty() {
                "(no command line available)".to_string()
            } else {
                let max = inner_width.saturating_sub(8);
                if e.cmd.len() > max {
                    format!("{}…", &e.cmd[..max.saturating_sub(1)])
                } else {
                    e.cmd.clone()
                }
            };
            out.push(Line::from(vec![
                Span::styled("  cmd:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(cmd, Style::default().fg(Color::Gray)),
            ]));

            out.push(Line::from(""));

            out.push(Line::from(vec![Span::styled(
                "  TOP PIDS BY MEMORY",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )]));

            let shown = e.samples.iter().take(5);
            let shown_count = e.samples.len().min(5);
            for s in shown {
                out.push(Line::from(vec![
                    Span::styled(
                        format!("    {:>7}", s.pid),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        format!("{:>9}", fmt_bytes(s.mem)),
                        Style::default().fg(Color::Magenta),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        format!("{:>5.1}% CPU", s.cpu),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        if s.disk_bps >= 1024.0 {
                            fmt_bps(s.disk_bps)
                        } else {
                            String::new()
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }

            if e.count > shown_count {
                out.push(Line::from(vec![Span::styled(
                    format!("    … and {} more process(es) — press Enter to expand", e.count - shown_count),
                    Style::default().fg(Color::DarkGray),
                )]));
            }

            out
        }
    };

    let title = match entry {
        Some(e) => format!(" DETAILS: {} ", e.name),
        None => " DETAILS ".to_string(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            title,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));

    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, area);
}

// ---------------------------------------------------------------------------
// Process Detail (fullscreen Enter mode)
// ---------------------------------------------------------------------------

fn draw_process_detail_fullscreen(f: &mut Frame, report: &Report, state: &UiState) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(1)])
        .split(area);

    let entry = state
        .selected_name
        .as_ref()
        .and_then(|n| report.blameboard.iter().find(|e| &e.name == n));

    let title = match entry {
        Some(e) => format!(" PROCESS DETAIL: {}  (Esc / Enter to return) ", e.name),
        None => " PROCESS DETAIL ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        title,
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    ));

    let lines: Vec<Line> = match entry {
        None => vec![Line::from("  No process selected")],
        Some(e) => {
            let mut out: Vec<Line> = Vec::new();
            let mem_pct = if report.total_mem > 0 {
                e.mem as f64 / report.total_mem as f64 * 100.0
            } else {
                0.0
            };
            out.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", e.name),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("({} process{})", e.count, if e.count == 1 { "" } else { "es" }),
                    Style::default().fg(Color::Gray),
                ),
            ]));
            out.push(Line::from(""));
            out.push(Line::from(vec![
                Span::styled("  Total RAM:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "{} ({:.1}% of {})",
                        fmt_bytes(e.mem),
                        mem_pct,
                        fmt_bytes(report.total_mem)
                    ),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(vec![
                Span::styled("  Total CPU:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:.1}%", e.cpu),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(vec![
                Span::styled("  Disk I/O:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    if e.disk_bps >= 1024.0 {
                        fmt_bps(e.disk_bps)
                    } else {
                        "idle".to_string()
                    },
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            out.push(Line::from(vec![
                Span::styled("  Oldest PID:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    fmt_uptime(e.max_uptime_secs),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            out.push(Line::from(vec![
                Span::styled("  Why:          ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    e.why.to_uppercase(),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(""));
            out.push(Line::from(vec![Span::styled(
                "  COMMAND LINE",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )]));
            out.push(Line::from(vec![Span::styled(
                format!("    {}", if e.cmd.is_empty() { "(unavailable)".to_string() } else { e.cmd.clone() }),
                Style::default().fg(Color::Gray),
            )]));
            out.push(Line::from(""));
            out.push(Line::from(vec![Span::styled(
                "  ALL PIDS",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )]));
            for s in &e.samples {
                out.push(Line::from(vec![
                    Span::styled(
                        format!("    {:>8}", s.pid),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        format!("{:>9}", fmt_bytes(s.mem)),
                        Style::default().fg(Color::Magenta),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        format!("{:>5.1}% CPU", s.cpu),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw("   "),
                    Span::styled(
                        if s.disk_bps >= 1024.0 {
                            fmt_bps(s.disk_bps)
                        } else {
                            String::new()
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }
            if e.count > e.samples.len() {
                out.push(Line::from(vec![Span::styled(
                    format!("    … {} additional PIDs not sampled", e.count - e.samples.len()),
                    Style::default().fg(Color::DarkGray),
                )]));
            }
            out
        }
    };

    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, chunks[0]);
    draw_footer(f, state, chunks[1]);
}

// ---------------------------------------------------------------------------
// System Load (full and collapsed)
// ---------------------------------------------------------------------------

fn draw_system_load(f: &mut Frame, report: &Report, state: &UiState, area: Rect) {
    let bar_width = (area.width.saturating_sub(4 + 10 + 4 + 4 + 14 + 12) as usize)
        .max(8)
        .min(40);

    let lines: Vec<Line> = report
        .culprits
        .iter()
        .map(|c| {
            let history = state
                .load_history
                .get(c.label)
                .map(|v| v.iter().copied().collect::<Vec<_>>())
                .unwrap_or_default();
            load_row(c, bar_width, &history)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " SYSTEM LOAD ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, area);
}

fn draw_system_load_collapsed(f: &mut Frame, report: &Report, area: Rect) {
    // Build a compact one-line summary
    let mut spans: Vec<Span> = vec![Span::styled(
        "  ✓ ",
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        "System load OK ",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    ));
    let parts: Vec<String> = report
        .culprits
        .iter()
        .map(|c| format!("{} {}", c.label.to_lowercase(), short_metric(c)))
        .collect();
    spans.push(Span::styled(
        format!("· {}", parts.join("  ·  ")),
        Style::default().fg(Color::Gray),
    ));

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " SYSTEM LOAD ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let p = Paragraph::new(Line::from(spans)).block(block);
    f.render_widget(p, area);
}

fn short_metric(c: &Culprit) -> String {
    // Pull a compact representation from the detail string
    let d = c.detail.clone();
    if d.len() > 30 {
        format!("{}…", &d[..29])
    } else {
        d
    }
}

fn load_row(c: &Culprit, bar_width: usize, history: &[u8]) -> Line<'static> {
    let color = score_color(c.score);
    let filled = (c.score as usize * bar_width / 100).min(bar_width);
    let empty = bar_width - filled;

    let (status_icon, status_text) = status_label(c.score);
    let spark = sparkline(history, 8, color, c.score >= 40);

    let mut spans: Vec<Span> = vec![
        Span::styled(
            format!("  {:<9}", c.label),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "\u{2591}".repeat(empty),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{:3}", c.score),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{} {:<8}", status_icon, status_text),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    spans.extend(spark);
    spans.push(Span::styled(
        format!("  {}", c.detail),
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Footer hint line
// ---------------------------------------------------------------------------

fn draw_footer(f: &mut Frame, state: &UiState, area: Rect) {
    // If a recent action produced a status message, show it on the left;
    // hint always shows on the right.
    let hint = match state.mode {
        ViewMode::Dashboard => " ↑/↓ inspect · Enter expand · q quit ",
        ViewMode::ProcessDetail => " Esc / Enter return · q quit ",
    };

    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(hint.chars().count() as u16 + 2)])
        .split(area);

    if let Some((msg, ok)) = state.current_status() {
        let style = if ok {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        };
        let p = Paragraph::new(Span::styled(format!("  {}", msg), style));
        f.render_widget(p, halves[0]);
    }

    let p = Paragraph::new(Span::styled(
        hint,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Right);
    f.render_widget(p, halves[1]);
}

// ---------------------------------------------------------------------------
// Persistent actions bar — all shortcuts visible, applied to selected
// ---------------------------------------------------------------------------

fn draw_actions_bar(f: &mut Frame, report: &Report, state: &UiState, area: Rect) {
    let entry = state
        .selected_name
        .as_ref()
        .and_then(|n| report.blameboard.iter().find(|e| &e.name == n))
        .or_else(|| report.blameboard.first());

    let target = match entry {
        Some(e) => format!(
            "→ {} ({} PID{})",
            e.name,
            e.all_pids.len(),
            if e.all_pids.len() == 1 { "" } else { "s" }
        ),
        None => "→ (no process selected)".to_string(),
    };

    // PIDs of the currently selected group — used for live permission checks
    let pids: &[u32] = entry.map(|e| e.all_pids.as_slice()).unwrap_or(&[]);

    // One action per line. Each row gets:
    //   - coloured key letter (dim grey when the action is unavailable)
    //   - full explanatory sentence (dim grey when unavailable)
    //   - live ● status tag computed from kill(pid, 0) on every PID
    let mut lines: Vec<Line> = Action::menu()
        .into_iter()
        .map(|a| {
            let avail = actions::availability(a, pids);
            let blocked = matches!(avail, actions::Availability::Blocked { .. });

            let key_style = if blocked {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
            } else {
                Style::default()
                    .fg(a.color())
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            };
            let sentence_style = if blocked {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM | Modifier::CROSSED_OUT)
            } else {
                Style::default().fg(Color::White)
            };

            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!(" {} ", a.key()), key_style),
                Span::raw("  "),
                Span::styled(action_sentence(a), sentence_style),
            ];

            // Only surface a tag when the action is NOT fully OK.
            if let Some((color, text)) = avail_indicator(avail) {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    "●",
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    text,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }

            Line::from(spans)
        })
        .collect();

    // Footer note — only the current identity; dots speak for themselves.
    let who = match state.current_username.as_deref() {
        Some(name) => format!("{} (uid {})", name, state.current_uid),
        None => format!("uid {}", state.current_uid),
    };
    let footer_text = if state.is_root() {
        format!("✓ Running as {}.", who)
    } else {
        format!("ⓘ Running as {}.", who)
    };
    let footer_color = if state.is_root() {
        Color::Green
    } else {
        Color::DarkGray
    };
    lines.push(
        Line::from(Span::styled(
            footer_text,
            Style::default()
                .fg(footer_color)
                .add_modifier(Modifier::ITALIC),
        ))
        .alignment(Alignment::Center),
    );

    let block = Block::default().borders(Borders::ALL).title(Line::from(vec![
        Span::styled(
            " ACTIONS ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            target,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]));

    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, area);
}

/// Map a computed Availability to an optional coloured dot + tag.
/// Returns `None` when the action would succeed — silence is the signal.
fn avail_indicator(a: actions::Availability) -> Option<(Color, String)> {
    use actions::Availability::*;
    match a {
        AllOk | AlwaysOk => None,
        Partial { allowed, total } => Some((
            Color::Yellow,
            format!("{}/{} PIDs reachable — sudo for the rest", allowed, total),
        )),
        Blocked { reason } => Some((Color::Red, reason.to_string())),
    }
}

fn action_sentence(a: Action) -> &'static str {
    // POSIX-standard signal numbers (HUP=1, INT=2, KILL=9, TERM=15) are
    // included inline. SIGSTOP/CONT/USR1/USR2 numbers differ between
    // Linux and macOS (e.g. USR1 is 10 on Linux, 30 on macOS), so we
    // omit them rather than lie.
    match a {
        Action::Stop =>
            "Pause the process in place (SIGSTOP). Memory state is preserved — resume with r.",
        Action::Cont =>
            "Resume a previously paused process (SIGCONT); it continues where it left off.",
        Action::Term =>
            "Ask the process to quit gracefully (SIGTERM, signal 15). It gets to flush files and settings.",
        Action::Kill =>
            "Force-kill the process (SIGKILL, signal 9 — `kill -9`). No cleanup; may leave locks or temp files.",
        Action::Int =>
            "Send SIGINT (signal 2) — same as pressing Ctrl-C. Usually aborts a CLI tool in the foreground.",
        Action::Hup =>
            "Send SIGHUP (signal 1) — originally \"terminal closed\". Most daemons reload their config on this.",
        Action::Usr1 =>
            "Send SIGUSR1 — app-defined meaning. Some apps dump state, rotate logs, or toggle debug.",
        Action::Usr2 =>
            "Send SIGUSR2 — a second app-defined signal. Consult the app's documentation.",
        Action::Nicer =>
            "Lower CPU priority (renice +10). The process keeps running but yields CPU to others.",
        Action::Lsof =>
            "Dump open files and sockets (lsof) to /tmp/stop-lsof-<name>.txt for inspection.",
        Action::OpenActivity =>
            "Open macOS Activity Monitor for a richer graphical view alongside stop.",
        Action::Purge =>
            "Run macOS `purge` to flush cached inactive memory — often recovers a few GB of RAM.",
    }
}

// ---------------------------------------------------------------------------
// Sparkline + delta helpers
// ---------------------------------------------------------------------------

fn sparkline(history: &[u8], width: usize, color: Color, is_culprit: bool) -> Vec<Span<'static>> {
    if history.is_empty() || width == 0 {
        return vec![Span::styled(
            " ".repeat(width),
            Style::default().fg(Color::DarkGray),
        )];
    }
    // Take last `width` samples
    let start = history.len().saturating_sub(width);
    let samples = &history[start..];
    let max = samples.iter().copied().max().unwrap_or(1).max(1) as f32;

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(width);
    // Pad left with spaces if fewer samples than width
    let pad = width.saturating_sub(samples.len());
    if pad > 0 {
        spans.push(Span::styled(
            " ".repeat(pad),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let style = if is_culprit {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let chars: String = samples
        .iter()
        .map(|&v| {
            let level = ((v as f32 / max) * (SPARK_CHARS.len() - 1) as f32).round() as usize;
            SPARK_CHARS[level.min(SPARK_CHARS.len() - 1)]
        })
        .collect();
    spans.push(Span::styled(chars, style));
    spans
}

fn delta_arrow_f32(delta: f32, threshold: f32) -> (&'static str, Color) {
    if delta.abs() < threshold {
        ("  ", Color::DarkGray)
    } else if delta > 0.0 {
        (" ↑", Color::Red)
    } else {
        (" ↓", Color::Green)
    }
}

fn delta_arrow_bytes(delta: i64, threshold: i64) -> (&'static str, Color) {
    if delta.abs() < threshold {
        ("  ", Color::DarkGray)
    } else if delta > 0 {
        (" ↑", Color::Red)
    } else {
        (" ↓", Color::Green)
    }
}

fn fmt_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let minutes = (secs % 3600) / 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

fn score_color(score: u8) -> Color {
    if score >= 70 {
        Color::Red
    } else if score >= 40 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn status_label(score: u8) -> (&'static str, &'static str) {
    if score >= 70 {
        ("✗", "CRITICAL")
    } else if score >= 40 {
        ("⚠", "MODERATE")
    } else {
        ("✓", "OK")
    }
}
