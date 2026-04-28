//! Minimal pty helper for keeping child stderr line-buffered.
//!
//! Many test runners (cargo, pytest, …) detect whether stderr is a tty and
//! switch to block-buffering when it isn't. Piping the child's stderr through
//! a `Stdio::piped()` pipe trips that detection — output appears all at once
//! when the child exits or the pipe buffer fills, which is no good for live
//! progress during a long compile.
//!
//! Allocating a pty for the child's stderr keeps `isatty()` returning true
//! inside the child while still letting the parent read everything via the
//! pty master fd.
//!
//! Only available on unix. Non-unix builds get a stub `open_stderr_pty` that
//! returns `None` so callers fall back to the `Stdio::piped()` path.

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd};

    /// Master/slave pair from `openpty(3)`. Both fds are owned; dropping
    /// closes them. The slave is meant to be handed to the child as stderr;
    /// the master is what the parent reads from.
    pub struct PtyPair {
        /// Read end held by the parent. Reading sees what the child writes
        /// to its stderr.
        pub master: File,
        /// Slave fd handed to the child as stderr. Drop the parent's copy
        /// after spawning so the master observes EOF on child exit.
        pub slave: OwnedFd,
    }

    /// Allocate a pty pair suitable for use as child stderr. Returns `None`
    /// if the kernel refuses (e.g. exhausted pty pool, restricted sandbox);
    /// callers should fall back to `Stdio::piped()`.
    pub fn open_stderr_pty() -> Option<PtyPair> {
        let mut master_fd: libc::c_int = -1;
        let mut slave_fd: libc::c_int = -1;
        // SAFETY: openpty writes through the two `c_int*` arguments and
        // returns 0 on success. We pass null for the optional name/termios/
        // winsize buffers, which the man page documents as "use defaults".
        let rc = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if rc != 0 {
            tracing::debug!(
                "openpty failed: {} — falling back to piped stderr",
                io::Error::last_os_error()
            );
            return None;
        }
        // Disable output postprocessing so the bytes we read from the master
        // are exactly the bytes the child wrote. Without this, the line
        // discipline translates every `\n` into `\r\n` (the `ONLCR` flag
        // that's on by default in cooked mode) — fine for a real terminal
        // but corrupts byte-exact captures and surprises tests.
        // SAFETY: tcgetattr/tcsetattr take an fd and a pointer to a fully
        // owned termios buffer; we read into a zeroed termios and write it
        // back without aliasing.
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave_fd, &mut tio) == 0 {
                tio.c_oflag &= !(libc::OPOST | libc::ONLCR);
                let _ = libc::tcsetattr(slave_fd, libc::TCSANOW, &tio);
            }
        }
        // SAFETY: openpty just gave us two fresh fds; we're the sole owner.
        let master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
        Some(PtyPair { master, slave })
    }
}

#[cfg(unix)]
pub use imp::{open_stderr_pty, PtyPair};

#[cfg(not(unix))]
mod imp {
    /// Placeholder type so callers can use `Option<PtyPair>` regardless of
    /// platform.
    pub struct PtyPair;

    /// Non-unix stub: never allocates a pty.
    pub fn open_stderr_pty() -> Option<PtyPair> {
        None
    }
}

#[cfg(not(unix))]
pub use imp::{open_stderr_pty, PtyPair};

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::{Command, Stdio};

    /// Verify the pty plumbing actually presents a tty to the child. We
    /// shell out to `sh` and run `[ -t 2 ]` (test "fd 2 is a terminal"),
    /// then echo the result via stdout. Anything piped would print "no";
    /// our pty wiring should make it print "yes".
    #[test]
    fn child_sees_tty_on_stderr() {
        let pair = open_stderr_pty().expect("openpty failed on this host");
        let PtyPair { master, slave } = pair;

        let output = Command::new("sh")
            .arg("-c")
            .arg("if [ -t 2 ]; then echo yes; else echo no; fi")
            .stdout(Stdio::piped())
            .stderr(Stdio::from(slave))
            .output()
            .expect("spawn");

        // Drain the master so the child doesn't block on a full pty buffer
        // (it doesn't write to stderr here, but read end has to exist).
        let mut master = master;
        let mut sink = Vec::new();
        let _ = master.read_to_end(&mut sink);

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "yes",
            "child did not see a tty on stderr"
        );
    }
}
