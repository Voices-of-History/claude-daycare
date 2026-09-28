//! Hold the machine awake for the lifetime of a visit.
//!
//! An overnight visit dies with the machine, and a laptop left alone idles to
//! sleep in minutes.
//!
//! - macOS: `caffeinate -i -s -w <pid>` asserts against idle sleep (`-i`) and,
//!   on AC power, system sleep (`-s`) until the runner's own process exits, so
//!   a crash releases the assertion without any cleanup of ours. A closed
//!   laptop lid still sleeps: caffeinate cannot override that.
//! - Linux: `systemd-inhibit --what=idle:sleep --mode=block` around a
//!   `tail --pid=<pid>` that ends when the runner does, so a crash releases the
//!   inhibitor the same way. logind's lid switch is a separate inhibitor that
//!   Daycare does not take, so closing a lid may still suspend.
//!
//! The guard's `Drop` ends the hold early at homecoming. Where no hold is
//! possible (no systemd, logind refusing an inactive session, WSL, a
//! container) the visit runs the same without one and says nothing about
//! staying awake.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The caffeinate binary macOS ships; nothing else is searched.
pub const CAFFEINATE: &str = "/usr/bin/caffeinate";

/// systemd's inhibitor client, where systemd distributions install it.
pub const SYSTEMD_INHIBIT: &str = "/usr/bin/systemd-inhibit";

/// One sentence for the visit's terminal, printed once when the hold begins.
pub const HOLD_MESSAGE: &str = if cfg!(target_os = "macos") {
    "keeping this Mac awake until it comes home (idle sleep is off; a closed laptop lid still sleeps)"
} else {
    "keeping this machine awake until it comes home (idle sleep and suspend are blocked; closing a laptop lid may still suspend it)"
};

/// Arguments that bind the assertion to `pid`: released when that process
/// exits, whether or not the guard is dropped.
pub fn caffeinate_args(pid: u32) -> [String; 4] {
    ["-i".into(), "-s".into(), "-w".into(), pid.to_string()]
}

/// The Linux equivalent: an inhibitor held by `systemd-inhibit` for as long
/// as its child runs, and the child is a `tail` that exits with `pid`.
pub fn systemd_inhibit_args(pid: u32) -> Vec<String> {
    vec![
        "--what=idle:sleep".into(),
        "--who=Claude Daycare".into(),
        "--why=A Claude is at daycare; the visit stops if this machine sleeps".into(),
        "--mode=block".into(),
        "/usr/bin/tail".into(),
        format!("--pid={pid}"),
        "-f".into(),
        "/dev/null".into(),
    ]
}

/// How long `systemd-inhibit` gets to refuse. It asks logind over D-Bus and
/// exits at once on "Access denied" (an ssh session polkit will not let
/// inhibit) or a missing bus; one that is still running after this holds the
/// inhibitor.
const INHIBIT_SETTLE: Duration = Duration::from_millis(750);

/// A running hold bound to this process. Dropping it releases the hold; a
/// runner that dies without dropping it is released by the pid binding.
#[derive(Debug)]
pub struct KeepAwake {
    child: Child,
}

impl KeepAwake {
    /// Start the hold for the current process. `None` when this platform has
    /// no hold, or it could not be taken: a visit runs the same without it, so
    /// the failure is reported by the caller, never fatal.
    pub fn for_this_visit() -> Option<KeepAwake> {
        let pid = std::process::id();
        if cfg!(target_os = "macos") {
            return KeepAwake::spawn(CAFFEINATE, pid);
        }
        if cfg!(target_os = "linux") {
            // Inside WSL an inhibitor, even when systemd grants one, holds
            // only the distro; `visit start` warns about the Windows host.
            if crate::wsl::detect().is_some() {
                return None;
            }
            return KeepAwake::spawn_checked(SYSTEMD_INHIBIT, &systemd_inhibit_args(pid));
        }
        None
    }

    pub fn spawn(binary: &str, pid: u32) -> Option<KeepAwake> {
        let child = Command::new(binary)
            .args(caffeinate_args(pid))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(KeepAwake { child })
    }

    /// Spawn a hold that may refuse right away, and count it only if it is
    /// still running after `INHIBIT_SETTLE`.
    pub fn spawn_checked(binary: &str, args: &[String]) -> Option<KeepAwake> {
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let settled = Instant::now() + INHIBIT_SETTLE;
        while Instant::now() < settled {
            match child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                // Exited (refused) or unwaitable: no hold.
                _ => {
                    let _ = child.wait();
                    return None;
                }
            }
        }
        Some(KeepAwake { child })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // Already exited (the pid binding went away) is fine; so is a kill
        // that races its exit. Reap it so the visit leaves no zombie behind.
        // Killing systemd-inhibit closes its inhibitor fd, which is what
        // releases the hold; its `tail` exits when the runner does.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hold_is_bound_to_the_runner_pid_and_covers_idle_and_system_sleep() {
        assert_eq!(caffeinate_args(4242), ["-i", "-s", "-w", "4242"]);
    }

    #[test]
    fn the_linux_hold_blocks_idle_and_sleep_until_the_runner_exits() {
        let args = systemd_inhibit_args(4242);
        assert_eq!(args[0], "--what=idle:sleep");
        assert!(args.contains(&"--mode=block".to_string()));
        assert_eq!(
            args[4..],
            ["/usr/bin/tail", "--pid=4242", "-f", "/dev/null"]
        );
    }

    #[test]
    fn a_missing_caffeinate_is_not_an_error() {
        assert!(KeepAwake::spawn("/nonexistent/caffeinate", std::process::id()).is_none());
    }

    #[test]
    fn an_inhibitor_that_refuses_at_once_is_no_hold() {
        // `false` exits immediately, as systemd-inhibit does on "Access denied".
        assert!(KeepAwake::spawn_checked("/bin/false", &[]).is_none());
        assert!(KeepAwake::spawn_checked("/nonexistent/systemd-inhibit", &[]).is_none());
    }

    #[test]
    fn an_inhibitor_still_running_after_the_settle_is_a_hold_until_dropped() {
        let guard = KeepAwake::spawn_checked("/bin/sleep", &["30".to_string()])
            .expect("a running child counts as a hold");
        let pid = guard.pid();
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        drop(guard);
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_the_guard_ends_the_hold() {
        let guard =
            KeepAwake::spawn(CAFFEINATE, std::process::id()).expect("macOS ships caffeinate");
        let pid = guard.pid();
        // Alive while held.
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        drop(guard);
        // Reaped: a signal to the pid no longer finds our child.
        let mut gone = false;
        for _ in 0..50 {
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            gone,
            "caffeinate {pid} still running after the guard was dropped"
        );
    }
}
