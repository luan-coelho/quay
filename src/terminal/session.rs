//! `PtySession` — owns a single interactive child process running inside a PTY
//! and the matching `alacritty_terminal::Term` that mirrors its output state.
//!
//! Concurrency model (per the plan):
//!   - The portable-pty `try_clone_reader` handle runs on a dedicated background
//!     thread that pushes chunks into a bounded `crossbeam_channel`.
//!   - The UI thread calls `poll()` on a Slint `Timer` (~60 Hz) which drains
//!     the channel and feeds every pending byte into `vte::ansi::Processor`,
//!     which in turn mutates the owned `Term`.
//!   - This means the `Term` itself lives entirely on the UI thread and needs
//!     no locking. The only shared state is the byte channel.
//!
//! Byte log persistence:
//!   - When a log path is provided to `spawn`, existing bytes in that file are
//!     replayed into the fresh `Term` before the PTY child is started. This
//!     reconstructs the visible scrollback from a previous run of the same
//!     task so that re-opening it feels like resuming.
//!   - New bytes received from the live PTY are appended to the same file
//!     from `poll()`, keeping the log current for the next reopen.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, TryRecvError, bounded};
use portable_pty::{
    Child, CommandBuilder, MasterPty, PtySize, native_pty_system,
};

use super::replay;

/// Single PTY-backed session. Owns the Term — do not share across threads.
pub struct PtySession {
    pub term: Term<VoidListener>,
    pub processor: Processor,
    pub cols: usize,
    pub rows: usize,
    rx: Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    log_writer: Option<BufWriter<File>>,
    _reader_thread: thread::JoinHandle<()>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtySession {
    /// Spawn a child process under a fresh PTY of `cols × rows`.
    ///
    /// `argv` is the full command line, with index 0 being the binary to
    /// execute and the rest being arguments (exactly the shape produced by
    /// [`crate::agents::AgentProvider::argv`]). `env` is a set of
    /// `(key, value)` pairs appended on top of Quay's baseline
    /// `TERM=xterm-256color` / `COLORTERM=truecolor` / `LANG` env.
    ///
    /// If `log_path` is `Some`, existing bytes in that file (from a previous
    /// run of the same task) are replayed into the Term before the new PTY
    /// child is started, and new output is appended to the same file.
    pub fn spawn(
        cols: usize,
        rows: usize,
        argv: &[String],
        env: &[(String, String)],
        cwd: &Path,
        log_path: Option<PathBuf>,
    ) -> Result<Self> {
        if argv.is_empty() {
            anyhow::bail!("PtySession::spawn requires at least a binary path in argv");
        }
        // ── 1. Build a fresh Term and, if a log file exists, replay it. ──
        let mut term: Term<VoidListener> = Term::new(
            Config::default(),
            &TermSize::new(cols, rows),
            VoidListener,
        );
        let mut processor: Processor = Processor::new();

        if let Some(ref path) = log_path {
            match replay::replay_log(path, &mut processor, &mut term) {
                Ok(n) if n > 0 => tracing::info!(bytes = n, "replayed byte log"),
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "byte log replay failed"),
            }
        }

        // ── 2. Open the log file in append mode for new output. ────────
        let log_writer = if let Some(ref path) = log_path {
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(f) => Some(BufWriter::new(f)),
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "could not open session log for append");
                    None
                }
            }
        } else {
            None
        };

        // ── 3. Open the PTY and spawn the child. ────────────────────────
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);

        // Baseline terminal env — agents and shells behave far better when
        // TERM / COLORTERM / LANG are set. Caller-provided `env` overrides
        // these (applied after) so a provider can force a different TERM if
        // needed.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("LANG", std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()));
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn_command failed")?;

        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("try_clone_reader failed")?;
        let writer = pair
            .master
            .take_writer()
            .context("take_writer failed")?;

        let (tx, rx) = bounded::<Vec<u8>>(1024);
        let reader_thread = thread::Builder::new()
            .name("quay-pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .context("failed to spawn PTY reader thread")?;

        Ok(Self {
            term,
            processor,
            cols,
            rows,
            rx,
            master: pair.master,
            writer,
            log_writer,
            _reader_thread: reader_thread,
            child,
        })
    }

    /// Drain any pending bytes from the reader channel into the `Term` and,
    /// if a log file is attached, append the same bytes to it.
    pub fn poll(&mut self) -> bool {
        let mut any = false;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.processor.advance(&mut self.term, &chunk);
                    if let Some(writer) = self.log_writer.as_mut() {
                        if let Err(err) = writer.write_all(&chunk) {
                            tracing::warn!(%err, "session log write failed; dropping log writer");
                            self.log_writer = None;
                        }
                    }
                    any = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        // Flush once per tick so crashes do not lose the last few bytes.
        if any {
            if let Some(writer) = self.log_writer.as_mut() {
                let _ = writer.flush();
            }
        }
        any
    }

    /// Send raw bytes to the PTY slave (stdin of the child process).
    pub fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Err(err) = self.writer.write_all(bytes) {
            tracing::warn!(%err, "PTY write failed");
            return;
        }
        if let Err(err) = self.writer.flush() {
            tracing::warn!(%err, "PTY flush failed");
        }
    }

    /// Resize the PTY and the mirrored `Term`.
    #[allow(dead_code)]
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let _ = self.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.term.resize(TermSize::new(cols, rows));
        self.cols = cols;
        self.rows = rows;
    }

    /// Whether the child process has exited.
    #[allow(dead_code)]
    pub fn is_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// OS-level process id of the spawned child, when portable-pty can
    /// resolve it. Used by the Process Manager to classify processes as
    /// "Tracked" instead of mistakenly flagging them as Orphans.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort: flush the log writer on drop so nothing is lost on
        // clean shutdown. Errors are intentionally swallowed.
        if let Some(writer) = self.log_writer.as_mut() {
            let _ = writer.flush();
        }
    }
}
