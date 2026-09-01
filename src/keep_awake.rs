//! Hold the Mac awake for the lifetime of a visit.
//!
//! An overnight visit dies with the machine, and a laptop left alone idles to
//! sleep in minutes. `caffeinate -i -s -w <pid>` asserts against idle sleep
//! (`-i`) and, on AC power, system sleep (`-s`) until the runner's own process
//! exits, so a crash releases the assertion without any cleanup of ours. The
//! guard's `Drop` ends it early at homecoming. A closed laptop lid still
//! sleeps: caffeinate cannot override that, and the terminal line says so.

use std::process::{Child, Command, Stdio};

/// The caffeinate binary macOS ships; nothing else is searched.
pub const CAFFEINATE: &str = "/usr/bin/caffeinate";

/// One sentence for the visit's terminal, printed once when the hold begins.
pub const HOLD_MESSAGE: &str =
    "keeping this Mac awake until it comes home (idle sleep is off; a closed laptop lid still sleeps)";

/// Arguments that bind the assertion to `pid`: released when that process
/// exits, whether or not the guard is dropped.
pub fn caffeinate_args(pid: u32) -> [String; 4] {
    ["-i".into(), "-s".into(), "-w".into(), pid.to_string()]
}

/// A running `caffeinate` bound to this process. Dropping it releases the
/// hold; a runner that dies without dropping it is released by `-w`.
#[derive(Debug)]
pub struct KeepAwake {
    child: Child,
}

impl KeepAwake {
    /// Start the hold for the current process. `None` when this is not macOS
    /// or caffeinate could not be started: a visit runs the same without it,
    /// so the failure is reported by the caller, never fatal.
    pub fn for_this_visit() -> Option<KeepAwake> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        KeepAwake::spawn(CAFFEINATE, std::process::id())
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

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // Already exited (the -w target went away) is fine; so is a kill that
        // races its exit. Reap it so the visit leaves no zombie behind.
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
    fn a_missing_caffeinate_is_not_an_error() {
        assert!(KeepAwake::spawn("/nonexistent/caffeinate", std::process::id()).is_none());
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
