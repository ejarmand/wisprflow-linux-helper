//! Optional transcript hook: turn a dictated transcript into a command instead
//! of pasting it (voice commands like "workspace three").
//!
//! When `WISPR_TRANSCRIPT_HOOK` names an executable, `PasteText` first runs it
//! with the transcript as its only argument. Exit status 0 means the hook
//! handled the transcript, so the paste is skipped. Any other status, a failure
//! to start, or running past [`TIMEOUT`] means "not a command" and the text is
//! pasted as usual, so a broken hook can never swallow dictation.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const ENV_VAR: &str = "WISPR_TRANSCRIPT_HOOK";

/// How long a hook may take to decide. `PasteText` blocks the dispatch loop
/// while it runs, so hooks should start long-lived work detached and exit.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Run the configured hook, if any. True when it handled the transcript.
pub fn handled(text: &str) -> bool {
    match std::env::var_os(ENV_VAR) {
        Some(hook) if !hook.is_empty() => run(&hook, text, TIMEOUT),
        _ => false,
    }
}

fn run(hook: &OsStr, text: &str, timeout: Duration) -> bool {
    let mut cmd = Command::new(hook);
    // fd 0 is the IPC command pipe and fd 3 the IPC return pipe. The hook must
    // not read the one or hold the other open: whatever it leaves running in
    // the background (a launcher, say) would keep the app's pipe alive.
    cmd.arg(text).stdin(Stdio::null()).stdout(Stdio::null());
    // SAFETY: close(2) is async-signal-safe, as pre_exec requires.
    unsafe {
        cmd.pre_exec(|| {
            libc::close(3);
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            log::warn!("transcript hook {hook:?} failed to start: {e}; pasting");
            return false;
        }
    };

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    log::info!("transcript hook handled the transcript; not pasting");
                }
                return status.success();
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                log::warn!("transcript hook ran past {timeout:?}; killed it, pasting");
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Err(e) => {
                log::warn!("transcript hook wait failed: {e}; pasting");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_default(hook: &str, text: &str) -> bool {
        run(OsStr::new(hook), text, TIMEOUT)
    }

    #[test]
    fn exit_zero_means_handled() {
        assert!(run_default("true", "workspace three"));
    }

    #[test]
    fn nonzero_exit_means_paste() {
        assert!(!run_default("false", "workspace three"));
    }

    #[test]
    fn missing_hook_means_paste() {
        assert!(!run_default("/nonexistent/wispr-hook", "workspace three"));
    }

    // test(1) with one argument exits 0 iff it is non-empty. Were the
    // transcript word-split, `test a = b` would compare "a" to "b" and fail.
    #[test]
    fn transcript_is_one_argument() {
        assert!(run_default("test", "a = b"));
        assert!(!run_default("test", ""));
    }

    #[test]
    fn slow_hook_is_killed_and_pastes() {
        let started = Instant::now();
        assert!(!run(OsStr::new("sleep"), "5", Duration::from_millis(100)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
