//! Process Manager — enumerate and classify running AI-agent processes.
//!
//! Uses the `sysinfo` crate (pure Rust, no subprocess, cross-platform) to
//! scan the system process list on demand. Classification mirrors Lanes':
//!
//! - **Tracked**: PID is in Quay's in-memory session registry — we know
//!   exactly which task it belongs to.
//! - **External**: another `claude`/`codex`/`opencode`/`gemini` process
//!   that Quay did not start.
//! - **Orphan**: a process whose parent is Quay but whose PID is not in
//!   the tracked set — a leftover child from a crashed session.

#![allow(dead_code)]

use std::collections::HashSet;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessClass {
    Tracked,
    External,
    Orphan,
}

impl ProcessClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tracked => "tracked",
            Self::External => "external",
            Self::Orphan => "orphan",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub cmdline: String,
    pub class: ProcessClass,
    pub parent_pid: Option<u32>,
}

const AGENT_PROCESS_NAMES: &[&str] = &["claude", "opencode", "codex", "gemini"];

/// Enumerate all running processes relevant to Quay.
pub fn enumerate(tracked_pids: &HashSet<u32>) -> Vec<ProcessEntry> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let quay_pid = std::process::id();
    let mut out: Vec<ProcessEntry> = Vec::new();

    for (pid, proc) in sys.processes() {
        let name_str = proc.name().to_string_lossy().into_owned();
        let cmdline_parts: Vec<String> = proc
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let cmdline = cmdline_parts.join(" ");

        let pid_u32 = pid.as_u32();
        let parent_pid = proc.parent().map(|p| p.as_u32());

        let is_agent_name = AGENT_PROCESS_NAMES
            .iter()
            .any(|n| name_str.eq_ignore_ascii_case(n) || name_str.starts_with(n));

        let class = if tracked_pids.contains(&pid_u32) {
            ProcessClass::Tracked
        } else if parent_pid == Some(quay_pid) && pid_u32 != quay_pid {
            ProcessClass::Orphan
        } else if is_agent_name && pid_u32 != quay_pid {
            ProcessClass::External
        } else {
            continue;
        };

        out.push(ProcessEntry {
            pid: pid_u32,
            name: name_str,
            cmdline,
            class,
            parent_pid,
        });
    }

    out.sort_by(|a, b| match (a.class, b.class) {
        (ProcessClass::Tracked, ProcessClass::Tracked) => a.pid.cmp(&b.pid),
        (ProcessClass::Tracked, _) => std::cmp::Ordering::Less,
        (_, ProcessClass::Tracked) => std::cmp::Ordering::Greater,
        _ => a.pid.cmp(&b.pid),
    });
    out
}

/// Send SIGTERM (or equivalent). Never escalates to SIGKILL.
pub fn terminate(pid: u32) -> anyhow::Result<()> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything(),
    );
    let proc = sys
        .process(Pid::from_u32(pid))
        .ok_or_else(|| anyhow::anyhow!("process {pid} not found"))?;
    if !proc.kill_with(sysinfo::Signal::Term).unwrap_or(false) {
        anyhow::bail!("terminate({pid}) failed");
    }
    Ok(())
}

/// Force-kill (SIGKILL / TerminateProcess).
pub fn force_kill(pid: u32) -> anyhow::Result<()> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::everything(),
    );
    let proc = sys
        .process(Pid::from_u32(pid))
        .ok_or_else(|| anyhow::anyhow!("process {pid} not found"))?;
    if !proc.kill() {
        anyhow::bail!("force_kill({pid}) failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_never_panics() {
        let tracked = HashSet::new();
        let _ = enumerate(&tracked);
    }

    #[test]
    fn sort_puts_tracked_first() {
        let mut entries = vec![
            ProcessEntry {
                pid: 10,
                name: "external".into(),
                cmdline: String::new(),
                class: ProcessClass::External,
                parent_pid: None,
            },
            ProcessEntry {
                pid: 20,
                name: "tracked".into(),
                cmdline: String::new(),
                class: ProcessClass::Tracked,
                parent_pid: None,
            },
        ];
        entries.sort_by(|a, b| match (a.class, b.class) {
            (ProcessClass::Tracked, ProcessClass::Tracked) => a.pid.cmp(&b.pid),
            (ProcessClass::Tracked, _) => std::cmp::Ordering::Less,
            (_, ProcessClass::Tracked) => std::cmp::Ordering::Greater,
            _ => a.pid.cmp(&b.pid),
        });
        assert_eq!(entries[0].class, ProcessClass::Tracked);
    }
}
