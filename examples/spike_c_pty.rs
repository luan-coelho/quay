//! Spike C — portable-pty + alacritty_terminal round-trip.
//!
//! Purpose: confirm that bytes produced by a real child process running under a
//! PTY can be fed into an `alacritty_terminal::term::Term` via `vte::ansi::Processor`
//! and that the resulting grid cells carry the expected characters with the
//! expected foreground colours (i.e. ANSI escape parsing actually works in our
//! embedded setup).
//!
//! Why it matters: the TerminalWidget (Task 4) depends on this exact pipeline —
//! PTY master -> background reader thread -> Term fed via Processor::advance.
//! If this spike fails, TerminalWidget has no foundation to build on.
//!
//! How to run:
//!     cargo run --release --example spike_c_pty
//!
//! Expected outcome: the spike prints "PASS" and exits 0. On failure it prints
//! which assertion went wrong and returns a non-zero exit code.

use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::{Term, vte};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use vte::ansi::{Color, NamedColor, Processor};

const COLS: usize = 80;
const ROWS: usize = 24;

// The test script prints three coloured words on three separate lines.
// Choosing Red/Green/Blue because they map to unambiguous NamedColor variants.
const TEST_SCRIPT: &str = concat!(
    "printf '\\033[31mred\\033[0m\\n';",
    "printf '\\033[32mgreen\\033[0m\\n';",
    "printf '\\033[34mblue\\033[0m\\n';",
);

fn main() {
    match run() {
        Ok(()) => {
            println!("PASS — portable-pty + alacritty_terminal round-trip works");
            std::process::exit(0);
        }
        Err(msg) => {
            eprintln!("FAIL — {msg}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    // 1. Open a PTY at 80x24.
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS as u16,
            cols: COLS as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty failed: {e}"))?;

    // 2. Spawn `sh -c <TEST_SCRIPT>` in the slave side of the PTY.
    //    Using `sh` instead of `bash` so this runs on more minimal environments.
    let mut cmd = CommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg(TEST_SCRIPT);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn_command failed: {e}"))?;

    // 3. Drop the slave end so the master sees EOF when the child exits.
    //    (If we keep the slave alive here the reader thread blocks forever.)
    drop(pair.slave);

    // 4. Clone a reader for the master and pump bytes into a channel from a
    //    dedicated thread — mirrors the real TerminalWidget architecture.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("try_clone_reader failed: {e}"))?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // 5. Construct the Term with default Config and a minimal TermSize.
    let config = Config::default();
    let size = TermSize::new(COLS, ROWS);
    let mut term: Term<VoidListener> = Term::new(config, &size, VoidListener);

    // 6. Drive a vte::ansi::Processor with bytes from the reader channel, with
    //    a generous timeout so a slow spawn on cold CI machines does not flake.
    let mut processor: Processor = Processor::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(chunk) => processor.advance(&mut term, &chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Peek at the child: if it has exited and the channel is empty,
                // we're done.
                if let Ok(Some(_status)) = child.try_wait() {
                    // Drain any remaining bytes the reader pushed.
                    while let Ok(extra) = rx.try_recv() {
                        processor.advance(&mut term, &extra);
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = child.wait();
    let _ = reader_thread.join();
    drop(pair.master);

    // 7. Assert the grid content row by row.
    assert_cell_line(&term, 0, "red", NamedColor::Red)?;
    assert_cell_line(&term, 1, "green", NamedColor::Green)?;
    assert_cell_line(&term, 2, "blue", NamedColor::Blue)?;

    // 8. Bonus: dump the first few rows to stderr so a human running the spike
    //    can eyeball the result.
    eprintln!("=== grid dump (rows 0..4) ===");
    dump_rows(&term, 0..4);

    Ok(())
}

/// Read cells 0..expected.len() from `row` and assert that their characters
/// spell `expected` and their foreground colour is `expected_fg`.
fn assert_cell_line(
    term: &Term<VoidListener>,
    row: usize,
    expected: &str,
    expected_fg: NamedColor,
) -> Result<(), String> {
    let cells = row_cells(term, row);
    let actual_str: String = cells
        .iter()
        .take(expected.len())
        .map(|c| c.c)
        .collect();

    if actual_str != expected {
        return Err(format!(
            "row {row}: expected text {expected:?}, got {actual_str:?}"
        ));
    }

    for (idx, cell) in cells.iter().take(expected.len()).enumerate() {
        match cell.fg {
            Color::Named(named) if named == expected_fg => {}
            other => {
                return Err(format!(
                    "row {row} col {idx}: expected fg {expected_fg:?}, got {other:?}"
                ));
            }
        }
    }

    Ok(())
}

/// Collect all cells of a given visible row into a Vec.
fn row_cells(term: &Term<VoidListener>, row: usize) -> Vec<Cell> {
    use alacritty_terminal::index::{Column, Line};
    let grid = term.grid();
    let line = Line(row as i32);
    (0..COLS)
        .map(|col| grid[line][Column(col)].clone())
        .collect()
}

fn dump_rows(term: &Term<VoidListener>, range: std::ops::Range<usize>) {
    for row in range {
        let cells = row_cells(term, row);
        let text: String = cells
            .iter()
            .map(|c| if c.c == '\0' { ' ' } else { c.c })
            .collect::<String>()
            .trim_end()
            .to_string();
        eprintln!("  row {row:>2}: {text:?}");
    }
}
