//! Pane backends adopted from a live server handoff.
//!
//! The successor server receives each pane's PTY master fd over the handoff
//! socket (SCM_RIGHTS) and wraps it in [`FdPaneBackend`]: a reader thread
//! pumps output exactly like the spawn path, writes and resizes go straight
//! to the inherited descriptor, and the transferred child pid keeps agent
//! detection and pane-close kill semantics working.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::session::pane::PaneBackend;
use crate::session::pty_backend::{
    OutputPipe, PaneFactory, PaneSpawnConfig, PipePoll, pump_reader,
};

/// One pane's transferable state received during a handoff.
pub struct PaneHandoffSource {
    /// PTY master fd transferred from the previous server.
    pub master: OwnedFd,
    /// Pid of the pane's child process (still running, now re-parented).
    pub child_pid: Option<u32>,
}

/// Key identifying a pane across the handoff: pane ids are only unique
/// within one session, so the session id disambiguates.
pub type PaneHandoffKey = (String, usize);

/// Pane factory used while reconstructing sessions from a handoff snapshot:
/// every pane in the snapshot must resolve to a transferred fd (a missing
/// entry aborts the restore), and once the restore is done the sessions are
/// switched back to [`crate::session::pty_backend::PtyPaneFactory`] so
/// future splits spawn fresh PTYs.
pub struct HandoffPaneFactory {
    sources: Mutex<HashMap<PaneHandoffKey, PaneHandoffSource>>,
}

impl HandoffPaneFactory {
    pub fn new(sources: HashMap<PaneHandoffKey, PaneHandoffSource>) -> Self {
        Self {
            sources: Mutex::new(sources),
        }
    }
}

impl PaneFactory for HandoffPaneFactory {
    fn spawn(&self, config: &PaneSpawnConfig) -> io::Result<Box<dyn PaneBackend>> {
        let source = self
            .sources
            .lock()
            .map_err(|_| io::Error::other("handoff pane source registry poisoned"))?
            .remove(&(config.session_id.clone(), config.pane_id))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "handoff state references pane {} of session {} but no fd was transferred for it",
                        config.pane_id, config.session_id
                    ),
                )
            })?;
        Ok(Box::new(FdPaneBackend::adopt(
            source.master,
            source.child_pid,
        )?))
    }
}

/// [`PaneBackend`] over an already-open PTY master fd.
pub struct FdPaneBackend {
    master: File,
    child_pid: Option<u32>,
    output_pipe: Arc<OutputPipe>,
    output_channel_open: bool,
    exited: bool,
    kill_child_on_drop: bool,
}

impl FdPaneBackend {
    /// Wrap an inherited PTY master fd, spawning the reader thread that
    /// feeds pane output into the usual mpsc channel.
    pub fn adopt(master: OwnedFd, child_pid: Option<u32>) -> io::Result<Self> {
        let master = File::from(master);
        let reader = master.try_clone()?;
        let output_pipe = OutputPipe::new();
        let pipe = Arc::clone(&output_pipe);
        thread::Builder::new()
            .name("spectra-handoff-pane-reader".to_string())
            .spawn(move || {
                let mut reader = reader;
                pump_reader(&mut reader, &pipe);
            })?;

        Ok(Self {
            master,
            child_pid,
            output_pipe,
            output_channel_open: true,
            exited: false,
            kill_child_on_drop: true,
        })
    }
}

impl PaneBackend for FdPaneBackend {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        (&self.master).write_all(bytes)?;
        (&self.master).flush()
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn poll_output(&mut self) -> Vec<Vec<u8>> {
        match self.output_pipe.poll() {
            PipePoll::Data(batch) => vec![batch],
            PipePoll::Empty => Vec::new(),
            PipePoll::Closed => {
                self.output_channel_open = false;
                Vec::new()
            }
        }
    }

    fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    fn is_closed(&mut self) -> bool {
        if self.exited {
            return true;
        }
        // The child is not this process's child, so try_wait is unavailable;
        // closure is detected via reader EOF/EIO (the channel sender drops).
        if !self.output_channel_open {
            self.exited = true;
            return true;
        }
        false
    }

    fn handoff_master_fd(&self) -> Option<RawFd> {
        Some(self.master.as_raw_fd())
    }

    fn disarm_child_kill(&mut self) {
        self.kill_child_on_drop = false;
    }
}

impl Drop for FdPaneBackend {
    fn drop(&mut self) {
        // Unblock a reader thread waiting on the pipe's byte cap.
        self.output_pipe.close_consumer();
        // Parity with `PtyPaneBackend`: closing a pane kills its process.
        // Skipped once the pane is known-exited (best-effort guard against
        // signalling a recycled pid) or after a handoff disarm.
        if self.kill_child_on_drop
            && !self.exited
            && let Some(pid) = self.child_pid
            && pid > 0
        {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{FdPaneBackend, HandoffPaneFactory, PaneHandoffSource};
    use crate::session::pane::{Pane, PaneBackend};
    use crate::session::pty_backend::{PaneFactory, PaneSpawnConfig};

    fn socket_backend_pair() -> (FdPaneBackend, UnixStream) {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let backend = FdPaneBackend::adopt(OwnedFd::from(ours), None).expect("adopt socketpair fd");
        (backend, theirs)
    }

    fn wait_for_output(pane: &mut Pane, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            pane.poll_output();
            if pane.row_text(0).contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}, row0: {:?}",
                pane.row_text(0)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn adopted_fd_backend_delivers_reads_and_writes() {
        let (backend, mut far_end) = socket_backend_pair();
        let mut pane = Pane::new(40, 5, false, Box::new(backend));

        far_end.write_all(b"from-old-pty").expect("write into fd");
        wait_for_output(&mut pane, "from-old-pty");

        pane.write(b"typed-after-handoff")
            .expect("write via backend");
        let mut buf = [0u8; 64];
        far_end
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let n = std::io::Read::read(&mut far_end, &mut buf).expect("read backend write");
        assert_eq!(&buf[..n], b"typed-after-handoff");
    }

    #[test]
    fn adopted_backend_reports_closed_after_eof_and_replay_restores_screen() {
        let (backend, far_end) = socket_backend_pair();
        let mut pane = Pane::new(40, 5, false, Box::new(backend));
        pane.feed_replay(b"replayed-screen");
        assert!(pane.row_text(0).contains("replayed-screen"));
        assert!(!pane.is_closed());

        drop(far_end);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            pane.poll_output();
            if pane.is_closed() {
                break;
            }
            assert!(Instant::now() < deadline, "pane never reported closed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn armed_drop_kills_the_transferred_child_and_disarmed_drop_does_not() {
        for disarm in [false, true] {
            let mut child = Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sleep");
            let (ours, _theirs) = UnixStream::pair().expect("socketpair");
            let mut backend =
                FdPaneBackend::adopt(OwnedFd::from(ours), Some(child.id())).expect("adopt fd");
            if disarm {
                backend.disarm_child_kill();
            }
            drop(backend);

            let deadline = Instant::now() + Duration::from_secs(5);
            let mut exited = false;
            while Instant::now() < deadline {
                if child.try_wait().expect("try_wait").is_some() {
                    exited = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // Always reap the child before asserting so no code path
            // leaks a zombie.
            let still_running = !exited;
            let _ = child.kill();
            let _ = child.wait();
            if disarm {
                assert!(still_running, "disarmed drop must leave the child running");
            } else {
                assert!(exited, "armed drop must kill the transferred child");
            }
        }
    }

    #[test]
    fn handoff_factory_requires_a_transferred_fd_per_pane() {
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");
        let mut sources = HashMap::new();
        sources.insert(
            ("main-1".to_string(), 1usize),
            PaneHandoffSource {
                master: OwnedFd::from(ours),
                child_pid: None,
            },
        );
        let factory = HandoffPaneFactory::new(sources);

        let mut config = PaneSpawnConfig {
            shell: "/bin/sh".to_string(),
            cwd: None,
            command: vec![],
            suppress_prompt_eol_marker: false,
            cols: 80,
            rows: 24,
            pane_id: 1,
            session_id: "main-1".to_string(),
        };
        factory.spawn(&config).expect("known pane adopts its fd");

        config.pane_id = 2;
        let err = match factory.spawn(&config) {
            Ok(_) => panic!("unknown pane must be refused"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("pane 2"), "got: {err}");
    }
}
