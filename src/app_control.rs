//! Per-application process control for the running-app menu: force close,
//! restart, and a CPU priority (nice value) remembered by WM_CLASS.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Installed with cap_sys_nice; see src/bin/aurora-renice.rs.
pub(crate) const RENICE_HELPER: &str = "/usr/local/libexec/aurora-renice";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub(crate) const ALL: [Priority; 3] = [Priority::Low, Priority::Normal, Priority::High];

    pub(crate) fn nice(self) -> i32 {
        match self {
            Priority::Low => 10,
            Priority::Normal => 0,
            Priority::High => -5,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Priority::Low => "Low",
            Priority::Normal => "Normal",
            Priority::High => "High",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }

    fn parse(text: &str) -> Option<Priority> {
        Priority::ALL.into_iter().find(|level| level.key() == text)
    }
}

/// WM_CLASS (lowercase) -> priority.
pub(crate) type PriorityMap = BTreeMap<String, Priority>;

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("aurora-wm").join("app-priority"))
}

/// Saved priorities, one `wm_class<TAB>level` line per application.
pub(crate) fn load_priorities() -> PriorityMap {
    let Some(path) = config_path() else {
        return PriorityMap::new();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return PriorityMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (class, level) = line.split_once('\t')?;
            Some((class.to_string(), Priority::parse(level.trim())?))
        })
        .collect()
}

pub(crate) fn save_priorities(map: &PriorityMap) -> io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text: String = map
        .iter()
        .map(|(class, level)| format!("{class}\t{}\n", level.key()))
        .collect();
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)
}

/// (state, parent pid) from /proc/PID/stat.
fn read_stat(pid: i32) -> Option<(char, i32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name sits in parentheses and may contain spaces.
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

/// The process and all of its descendants, parents before children.
pub(crate) fn process_tree(root: i32) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if let Some((_, ppid)) = read_stat(pid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut tree = vec![root];
    let mut next = 0;
    while next < tree.len() {
        if let Some(kids) = children.get(&tree[next]) {
            tree.extend(kids.iter().copied());
        }
        next += 1;
    }
    tree
}

/// Exists and has not exited (a zombie counts as exited).
pub(crate) fn is_running(pid: i32) -> bool {
    matches!(read_stat(pid), Some((state, _)) if state != 'Z')
}

/// SIGKILL to the process and its descendants. Returns how many were signalled.
pub(crate) fn kill_tree(root: i32) -> usize {
    let tree = process_tree(root);
    for &pid in &tree {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    tree.len()
}

/// Waits briefly for the process to go away so a restart does not race it.
pub(crate) fn wait_for_exit(pid: i32, limit: Duration) {
    let started = Instant::now();
    while is_running(pid) && started.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// argv and working directory of a running process.
pub(crate) fn command_line(pid: i32) -> Option<(Vec<String>, PathBuf)> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let trimmed: &[u8] = if raw.last() == Some(&0) {
        &raw[..raw.len() - 1]
    } else {
        &raw[..]
    };
    let argv: Vec<String> = trimmed
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    match argv.first() {
        Some(first) if !first.is_empty() => {}
        _ => return None,
    }
    let cwd = fs::read_link(format!("/proc/{pid}/cwd")).unwrap_or_else(|_| PathBuf::from("/"));
    Some((argv, cwd))
}

/// Sets the nice value of the given processes. Raising priority (a lower nice)
/// needs CAP_SYS_NICE, so pids the kernel refuses go through the helper, which
/// only touches processes owned by the caller.
pub(crate) fn apply_nice(pids: &[i32], nice: i32) -> Result<(), String> {
    let mut refused = Vec::new();
    for &pid in pids {
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
        if rc != 0 {
            if let Some(libc::EPERM | libc::EACCES) = io::Error::last_os_error().raw_os_error() {
                refused.push(pid);
            }
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    let status = Command::new(RENICE_HELPER)
        .arg(nice.to_string())
        .args(refused.iter().map(|pid| pid.to_string()))
        .status()
        .map_err(|err| format!("{RENICE_HELPER}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{RENICE_HELPER} could not set nice {nice}"))
    }
}
