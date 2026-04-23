//! Kill a stale process by pid. Used by the inspect dashboard's port
//! reclaim path when a previous tebis instance crashed while holding
//! the port.

use std::time::Duration;

const TERM_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TERM_POLL_ATTEMPTS: usize = 30;
const POST_KILL_WAIT: Duration = Duration::from_millis(200);

#[cfg(unix)]
mod unix {
    use super::{POST_KILL_WAIT, TERM_POLL_ATTEMPTS, TERM_POLL_INTERVAL};

    /// SIGTERM, poll for exit, then SIGKILL. Synchronous — startup-time
    /// only. `pid` as i32 can only misbehave for pids ≥ 2^31, which no
    /// real system emits.
    pub fn kill_and_wait(pid: u32) {
        // SAFETY: `kill(2)` with a valid pid is sound.
        unsafe {
            libc::kill(pid.cast_signed(), libc::SIGTERM);
        }
        for _ in 0..TERM_POLL_ATTEMPTS {
            std::thread::sleep(TERM_POLL_INTERVAL);
            // SAFETY: `kill(pid, 0)` is the standard "does this pid exist?"
            // probe. Returns 0 if alive, -1 with ESRCH if gone.
            let alive = unsafe { libc::kill(pid.cast_signed(), 0) } == 0;
            if !alive {
                return;
            }
        }
        tracing::warn!(pid, "stale process didn't exit on SIGTERM; sending SIGKILL");
        unsafe {
            libc::kill(pid.cast_signed(), libc::SIGKILL);
        }
        std::thread::sleep(POST_KILL_WAIT);
    }
}

#[cfg(windows)]
mod windows {
    use super::{POST_KILL_WAIT, TERM_POLL_ATTEMPTS, TERM_POLL_INTERVAL};
    use std::process::Command;

    /// Windows has no SIGTERM. `taskkill /PID <pid> /T` without `/F`
    /// sends WM_CLOSE to GUI apps and closes the console handle for
    /// console apps — the closest graceful analogue. If the process
    /// is still alive after the poll window, escalate to `/F`
    /// (`TerminateProcess` under the hood).
    ///
    /// Using the `taskkill.exe` subprocess keeps this file off the
    /// `windows` crate until the Phase 6 full port; the inspect
    /// reclaim path is startup-only and not latency-sensitive.
    pub fn kill_and_wait(pid: u32) {
        let pid_s = pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid_s, "/T"])
            .output();
        for _ in 0..TERM_POLL_ATTEMPTS {
            std::thread::sleep(TERM_POLL_INTERVAL);
            if !is_alive(pid) {
                return;
            }
        }
        tracing::warn!(
            pid,
            "stale process didn't exit on graceful taskkill; escalating to /F"
        );
        let _ = Command::new("taskkill")
            .args(["/PID", &pid_s, "/T", "/F"])
            .output();
        std::thread::sleep(POST_KILL_WAIT);
    }

    fn is_alive(pid: u32) -> bool {
        // `tasklist /FI "PID eq <pid>" /NH` prints nothing (or a header)
        // when the pid is gone, and a one-line entry when it's alive.
        // Good enough for this startup probe.
        let Ok(out) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().any(|l| l.contains(&pid.to_string()))
    }
}

#[cfg(unix)]
pub use unix::kill_and_wait;
#[cfg(windows)]
pub use windows::kill_and_wait;
