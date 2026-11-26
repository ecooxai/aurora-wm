//! aurora-renice NICE PID...: sets the nice value of the given processes.
//!
//! Installed with cap_sys_nice (setcap cap_sys_nice+ep) so the unprivileged
//! aurora-wm can raise an application back to normal or to high priority.
//! Only processes whose real uid matches the caller's are touched.

use std::fs;

fn main() {
    let mut args = std::env::args().skip(1);
    let nice: i32 = match args.next().and_then(|value| value.parse().ok()) {
        Some(value) if (-20..=19).contains(&value) => value,
        _ => {
            eprintln!("usage: aurora-renice NICE PID...");
            std::process::exit(2);
        }
    };
    let uid = unsafe { libc::getuid() };
    let mut failed = false;
    for arg in args {
        let Ok(pid) = arg.parse::<i32>() else {
            eprintln!("bad pid {arg}");
            failed = true;
            continue;
        };
        if pid <= 1 || owner_uid(pid) != Some(uid) {
            eprintln!("refusing pid {pid}: not owned by uid {uid}");
            failed = true;
            continue;
        }
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
        if rc != 0 {
            eprintln!("pid {pid}: {}", std::io::Error::last_os_error());
            failed = true;
        }
    }
    std::process::exit(if failed { 1 } else { 0 });
}

fn owner_uid(pid: i32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|value| value.split_whitespace().next()?.parse().ok())
}
