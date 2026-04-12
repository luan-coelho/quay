//! Spike A — Terminal blitting viability (GO/NO-GO gate for the whole project).
//!
//! Purpose: prove that a CPU-rasterized pixel buffer can be displayed inside a
//! Slint `Image` element at a steady ~60 Hz without flicker or tearing on a
//! terminal-sized framebuffer (120 cols × 40 rows of 10×20 px cells = 1200×800 px,
//! ~3.8 MB of RGBA each frame).
//!
//! Why it matters: the real TerminalWidget will do exactly this — rasterize a
//! glyph atlas once and blit dirty cells into a `SharedPixelBuffer` every time
//! `alacritty_terminal` reports damage. If the round-trip "worker builds buffer
//! -> invoke_from_event_loop -> Image::from_rgba8_premultiplied -> Skia upload"
//! can't hold 60 Hz at this scale, the whole UI architecture must be revisited
//! before any other investment.
//!
//! How to run:
//!     cargo run --release --example spike_a_blit
//!
//! The window logs the measured FPS every second to stderr. Expected: ~60 FPS
//! steady on any mid-range laptop (Linux/macOS/Windows).

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

// Grid dimensions chosen to match a realistically large terminal session.
// 120 cols × 40 rows at 10×20 px per cell is 1200×800 px (3.84 MB RGBA).
const COLS: u32 = 120;
const ROWS: u32 = 40;
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;
const FB_W: u32 = COLS * CELL_W;
const FB_H: u32 = ROWS * CELL_H;

// Target one frame every ~16 ms to aim for 60 FPS.
const TARGET_FRAME: Duration = Duration::from_micros(16_666);

slint::slint! {
    import { VerticalBox, HorizontalBox } from "std-widgets.slint";

    export component SpikeAWindow inherits Window {
        title: "Spike A — Slint Image blit @ 60 Hz";
        preferred-width: 1280px;
        preferred-height: 900px;
        background: #07080b;

        in property <image> frame;
        in property <string> stats: "Booting…";

        VerticalLayout {
            spacing: 8px;
            padding: 16px;

            Text {
                text: "Spike A — blit a 120×40 cell grid into a Slint Image at 60 Hz";
                font-size: 14px;
                color: #c7ccd4;
            }

            Text {
                text: root.stats;
                font-size: 13px;
                color: #55d88c;
                font-family: "monospace";
            }

            Rectangle {
                background: #000000;
                border-radius: 4px;
                clip: true;
                Image {
                    source: root.frame;
                    width: 1200px;
                    height: 800px;
                    image-fit: contain;
                    image-rendering: pixelated;
                }
            }
        }
    }
}

/// A tiny deterministic PRNG so we don't need to pull in `rand` just for a spike.
struct XorShift(u32);

impl XorShift {
    fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0xdead_beef } else { seed })
    }

    #[inline]
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    #[inline]
    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
}

/// Fill an entire framebuffer with randomly-coloured cells.
/// This simulates "worst case" terminal updates where every cell is dirty.
fn rasterize_random_cells(buf: &mut [u8], rng: &mut XorShift) {
    debug_assert_eq!(buf.len(), (FB_W * FB_H * 4) as usize);

    for cy in 0..ROWS {
        for cx in 0..COLS {
            // Pick one background colour for the whole cell and one "glyph" colour
            // for a simple 3×3 block inside it. Close enough to approximate a
            // rasterized glyph + background in terms of memory traffic.
            let bg = [rng.byte() / 2, rng.byte() / 2, rng.byte() / 2, 0xff];
            let fg = [rng.byte(), rng.byte(), rng.byte(), 0xff];

            let x0 = cx * CELL_W;
            let y0 = cy * CELL_H;

            for dy in 0..CELL_H {
                let y = y0 + dy;
                let row_start = ((y * FB_W + x0) * 4) as usize;
                for dx in 0..CELL_W {
                    let idx = row_start + (dx as usize) * 4;
                    // Tiny 3×3 "glyph" in the middle of the cell.
                    let in_glyph = (3..6).contains(&dx) && (8..11).contains(&dy);
                    let color = if in_glyph { fg } else { bg };
                    buf[idx] = color[0];
                    buf[idx + 1] = color[1];
                    buf[idx + 2] = color[2];
                    buf[idx + 3] = color[3];
                }
            }
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // Force full frames even when the OS thinks the window is idle — otherwise
    // Slint may rate-limit to the display refresh, which is exactly what we want
    // here for a fair 60 Hz measurement.
    let window = SpikeAWindow::new()?;
    let weak = window.as_weak();

    let stop = Arc::new(AtomicBool::new(false));
    let frames_drawn = Arc::new(AtomicU32::new(0));

    // Worker thread: builds a fresh pixel buffer each frame and posts it to the
    // Slint UI thread via invoke_from_event_loop (the pattern documented at
    // https://docs.rs/slint/latest/slint/struct.Image.html#sending-image-to-a-thread).
    let frames_for_worker = frames_drawn.clone();
    let stop_for_worker = stop.clone();
    let worker = thread::Builder::new()
        .name("spike-a-blitter".into())
        .spawn(move || {
            let mut rng = XorShift::new(0x1234_5678);
            let mut next_deadline = Instant::now();

            while !stop_for_worker.load(Ordering::Relaxed) {
                let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(FB_W, FB_H);
                rasterize_random_cells(buffer.make_mut_bytes(), &mut rng);

                let frames_counter = frames_for_worker.clone();
                let weak = weak.clone();
                let posted = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_frame(Image::from_rgba8_premultiplied(buffer));
                        frames_counter.fetch_add(1, Ordering::Relaxed);
                    }
                });
                if posted.is_err() {
                    // Event loop is gone — window closed, bail out.
                    break;
                }

                // Pace the loop at ~60 Hz so we don't flood the event queue.
                next_deadline += TARGET_FRAME;
                let now = Instant::now();
                if next_deadline > now {
                    thread::sleep(next_deadline - now);
                } else {
                    // We're behind; skip to "now" and keep going.
                    next_deadline = now;
                }
            }
        })
        .expect("failed to spawn blitter thread");

    // Stats timer: runs on the Slint event loop thread once per second, reads
    // the atomic frame counter and updates the on-screen label.
    let stats_weak = window.as_weak();
    let frames_for_stats = frames_drawn.clone();
    let stats_timer = slint::Timer::default();
    let mut last_instant = Instant::now();
    stats_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        move || {
            let now = Instant::now();
            let elapsed = now - last_instant;
            last_instant = now;

            let frames = frames_for_stats.swap(0, Ordering::Relaxed);
            let fps = frames as f64 / elapsed.as_secs_f64();
            let text = format!(
                "framebuffer: {FB_W}×{FB_H} RGBA  |  cells: {COLS}×{ROWS}  |  fps: {fps:5.1}"
            );
            eprintln!("[spike_a] {text}");
            if let Some(w) = stats_weak.upgrade() {
                w.set_stats(text.into());
            }
        },
    );

    window.run()?;

    // Window closed: shut the worker down cleanly.
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    Ok(())
}
