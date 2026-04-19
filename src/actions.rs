//! Process-control actions invoked from the TUI.
//!
//! Each variant of `Action` corresponds to one operation that can be
//! applied to all PIDs of a selected (aggregated) process group.

use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    // Signals
    Term, // SIGTERM — graceful kill
    Kill, // SIGKILL — force kill (irreversible)
    Stop, // SIGSTOP — suspend
    Cont, // SIGCONT — resume
    Int,  // SIGINT — like Ctrl-C
    Hup,  // SIGHUP — hangup; many daemons reload config
    Usr1, // SIGUSR1 — app-defined
    Usr2, // SIGUSR2 — app-defined

    // Scheduling
    Nicer, // renice +10 (lower priority)

    // Investigate / external
    Lsof,         // dump open files for the group to a temp file
    OpenActivity, // open Activity Monitor (macOS)

    // System
    Purge, // free inactive memory (macOS purge)
}

impl Action {
    pub fn key(&self) -> char {
        match self {
            Action::Stop => 's',
            Action::Cont => 'r',
            Action::Term => 'k',
            Action::Kill => 'K',
            Action::Int => 'i',
            Action::Hup => 'H',
            Action::Usr1 => '1',
            Action::Usr2 => '2',
            Action::Nicer => 'n',
            Action::Lsof => 'l',
            Action::OpenActivity => 'm',
            Action::Purge => 'p',
        }
    }

    /// True if this action targets PIDs and therefore only works on
    /// processes the user owns (unless running as root).
    pub fn targets_pids(&self) -> bool {
        !matches!(self, Action::Purge | Action::OpenActivity)
    }

    /// Color tier for the persistent action bar
    pub fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color::*;
        match self {
            Action::Kill | Action::Term | Action::Int | Action::Hup => Red,
            Action::Stop | Action::Cont => Cyan,
            Action::Usr1 | Action::Usr2 => LightCyan,
            Action::Nicer => Yellow,
            Action::Lsof | Action::OpenActivity => Magenta,
            Action::Purge => LightMagenta,
        }
    }

    /// Return the actions menu in display order, ranked by how often
    /// someone reaching for this tool actually needs each action.
    pub fn menu() -> Vec<Action> {
        let mut v = vec![
            Action::Term,  // k — most common reaction to "this is hogging"
            Action::Kill,  // K — escalation when k doesn't stick
            Action::Stop,  // s — the reversible "give me my CPU back"
            Action::Cont,  // r — paired with s
            Action::Nicer, // n — non-destructive "take a back seat"
            Action::Int,   // i — niche: foreground CLI tools
            Action::Hup,   // H — niche: daemon config reload
            Action::Lsof,  // l — investigation, not emergency
        ];
        if cfg!(target_os = "macos") {
            v.push(Action::OpenActivity); // m — investigation fallback
            v.push(Action::Purge);        // p — system-level, occasional
        }
        v.push(Action::Usr1); // 1 — rare, app-specific
        v.push(Action::Usr2); // 2 — rare, app-specific
        v
    }
}

/// Return the effective UID of the current process (0 == running as root).
pub fn current_uid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

/// Resolve a UID to its username via getpwuid. Returns None if the user
/// is not in the passwd database.
pub fn username_for(uid: u32) -> Option<String> {
    unsafe {
        let pw = libc::getpwuid(uid as libc::uid_t);
        if pw.is_null() {
            return None;
        }
        let name_ptr = (*pw).pw_name;
        if name_ptr.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(name_ptr)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Ask the kernel whether we can signal this PID without actually sending
/// anything. `kill(pid, 0)` returns 0 if permitted and the PID exists,
/// -1/EPERM if denied, -1/ESRCH if the PID is gone. We treat both errors
/// as "cannot signal" — from the user's perspective both mean the action
/// won't reach this PID.
pub fn can_signal_pid(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Live availability summary for rendering in the Actions bar.
#[derive(Debug, Clone, Copy)]
pub enum Availability {
    /// Action will reach every PID / resource.
    AllOk,
    /// Action will reach some PIDs.
    Partial { allowed: usize, total: usize },
    /// Action cannot proceed at all.
    Blocked { reason: &'static str },
    /// Action always works (no per-PID check needed).
    AlwaysOk,
}

/// Compute availability for `action` against the given PID group,
/// using live kernel permission checks where applicable.
pub fn availability(action: Action, pids: &[u32]) -> Availability {
    let is_root = current_uid() == 0;

    match action {
        Action::Purge => {
            if is_root {
                Availability::AllOk
            } else {
                Availability::Blocked {
                    reason: "needs sudo",
                }
            }
        }
        Action::OpenActivity => Availability::AlwaysOk,
        _ if action.targets_pids() => {
            if pids.is_empty() {
                return Availability::Blocked {
                    reason: "no PIDs",
                };
            }
            if is_root {
                return Availability::AllOk;
            }
            let allowed = pids.iter().filter(|&&p| can_signal_pid(p)).count();
            if allowed == pids.len() {
                Availability::AllOk
            } else if allowed == 0 {
                Availability::Blocked {
                    reason: "no permission",
                }
            } else {
                Availability::Partial {
                    allowed,
                    total: pids.len(),
                }
            }
        }
        _ => Availability::AlwaysOk,
    }
}

#[derive(Debug)]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
}

pub fn apply(action: Action, name: &str, pids: &[u32]) -> ActionResult {
    match action {
        Action::Term => signal(pids, libc::SIGTERM, verb(action)),
        Action::Kill => signal(pids, libc::SIGKILL, verb(action)),
        Action::Stop => signal(pids, libc::SIGSTOP, verb(action)),
        Action::Cont => signal(pids, libc::SIGCONT, verb(action)),
        Action::Int => signal(pids, libc::SIGINT, verb(action)),
        Action::Hup => signal(pids, libc::SIGHUP, verb(action)),
        Action::Usr1 => signal(pids, libc::SIGUSR1, verb(action)),
        Action::Usr2 => signal(pids, libc::SIGUSR2, verb(action)),
        Action::Nicer => renice(pids, 10),
        Action::Lsof => lsof(name, pids),
        Action::OpenActivity => open_activity_monitor(),
        Action::Purge => purge(),
    }
}

/// Human-readable past-tense verb + clarification for each signal, so the
/// status banner tells the user what *happened*, not just "SIGSTOP was sent".
fn verb(action: Action) -> Verb {
    match action {
        Action::Term => Verb {
            past: "Asked to quit",
            hint: Some("graceful exit — processes may take a moment to clean up"),
        },
        Action::Kill => Verb {
            past: "Killed",
            hint: Some("SIGKILL — processes are gone, no cleanup"),
        },
        Action::Stop => Verb {
            past: "Paused",
            hint: Some("still alive, frozen in memory — press r to resume"),
        },
        Action::Cont => Verb {
            past: "Resumed",
            hint: Some("continuing from where they were paused"),
        },
        Action::Int => Verb {
            past: "Interrupted",
            hint: Some("SIGINT — like pressing Ctrl-C in the target's terminal"),
        },
        Action::Hup => Verb {
            past: "Sent hangup to",
            hint: Some("SIGHUP — most daemons reload config on this"),
        },
        Action::Usr1 => Verb {
            past: "Sent SIGUSR1 to",
            hint: Some("meaning depends on the app"),
        },
        Action::Usr2 => Verb {
            past: "Sent SIGUSR2 to",
            hint: Some("meaning depends on the app"),
        },
        Action::Nicer | Action::Lsof | Action::OpenActivity | Action::Purge => Verb {
            past: "(non-signal action)",
            hint: None,
        },
    }
}

struct Verb {
    past: &'static str,
    hint: Option<&'static str>,
}

fn signal(pids: &[u32], sig: i32, verb: Verb) -> ActionResult {
    if pids.is_empty() {
        return ActionResult {
            ok: false,
            message: "No PIDs to signal".to_string(),
        };
    }
    let mut sent = 0usize;
    let mut failed = 0usize;
    for &pid in pids {
        let r = unsafe { libc::kill(pid as libc::pid_t, sig) };
        if r == 0 {
            sent += 1;
        } else {
            failed += 1;
        }
    }
    let hint = verb.hint.map(|h| format!(" — {}", h)).unwrap_or_default();
    if failed == 0 {
        ActionResult {
            ok: true,
            message: format!("✓ {} {} PID(s){}", verb.past, sent, hint),
        }
    } else if sent == 0 {
        ActionResult {
            ok: false,
            message: format!(
                "✗ Could not signal any of the {} PID(s) — permission denied or already exited",
                failed
            ),
        }
    } else {
        ActionResult {
            ok: true,
            message: format!(
                "⚠ {} {} PID(s); {} could not be signalled (permission?){}",
                verb.past, sent, failed, hint
            ),
        }
    }
}

fn renice(pids: &[u32], adjustment: i32) -> ActionResult {
    let mut ok = 0usize;
    let mut failed = 0usize;
    for &pid in pids {
        // getpriority sets errno on failure but returns -1 (which is also a valid value).
        // We accept the small chance of misreading as default 0 — common case.
        let cur = unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) };
        let target = (cur + adjustment).clamp(-20, 19);
        let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, target) };
        if r == 0 {
            ok += 1;
        } else {
            failed += 1;
        }
    }
    if failed == 0 {
        ActionResult {
            ok: true,
            message: format!("✓ Renice +{} on {} PID(s)", adjustment, ok),
        }
    } else if ok == 0 {
        ActionResult {
            ok: false,
            message: format!(
                "✗ Renice failed on all {} PID(s) (negative renice needs root)",
                failed
            ),
        }
    } else {
        ActionResult {
            ok: true,
            message: format!("⚠ Reniced {} PID(s); {} failed", ok, failed),
        }
    }
}

fn lsof(name: &str, pids: &[u32]) -> ActionResult {
    if pids.is_empty() {
        return ActionResult {
            ok: false,
            message: "No PIDs to inspect".to_string(),
        };
    }
    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect();
    let safe_name = if safe_name.is_empty() {
        "process".to_string()
    } else {
        safe_name
    };
    let path = format!("/tmp/stop-lsof-{}.txt", safe_name);
    let pid_list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    // lsof -p accepts multiple PIDs separated by commas
    let pid_arg = pid_list.join(",");
    let out = Command::new("lsof")
        .args(["-p", &pid_arg])
        .output();
    match out {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => {
            if let Err(e) = std::fs::write(&path, &o.stdout) {
                return ActionResult {
                    ok: false,
                    message: format!("✗ wrote 0 bytes: {}", e),
                };
            }
            ActionResult {
                ok: true,
                message: format!("✓ open files written to {}", path),
            }
        }
        Ok(o) => ActionResult {
            ok: false,
            message: format!(
                "✗ lsof failed: {}",
                String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("(no output)")
            ),
        },
        Err(e) => ActionResult {
            ok: false,
            message: format!("✗ lsof not available: {}", e),
        },
    }
}

#[cfg(target_os = "macos")]
fn open_activity_monitor() -> ActionResult {
    let r = Command::new("open")
        .args(["-a", "Activity Monitor"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match r {
        Ok(s) if s.success() => ActionResult {
            ok: true,
            message: "✓ Activity Monitor opened".to_string(),
        },
        Ok(_) => ActionResult {
            ok: false,
            message: "✗ open returned non-zero".to_string(),
        },
        Err(e) => ActionResult {
            ok: false,
            message: format!("✗ {}", e),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn open_activity_monitor() -> ActionResult {
    ActionResult {
        ok: false,
        message: "Activity Monitor is macOS only".to_string(),
    }
}

#[cfg(target_os = "macos")]
fn purge() -> ActionResult {
    let r = Command::new("purge")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match r {
        Ok(s) if s.success() => ActionResult {
            ok: true,
            message: "✓ purged inactive memory".to_string(),
        },
        Ok(_) => ActionResult {
            ok: false,
            message: "✗ purge returned non-zero (try with sudo)".to_string(),
        },
        Err(e) => ActionResult {
            ok: false,
            message: format!("✗ purge unavailable: {}", e),
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn purge() -> ActionResult {
    ActionResult {
        ok: false,
        message: "purge is macOS only".to_string(),
    }
}
