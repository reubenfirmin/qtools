use std::io::{stderr, IsTerminal, Write};
use std::time::{Duration, Instant};
use crate::style::human_plain;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR_WIDTH: usize = 20;
const REDRAW_INTERVAL: Duration = Duration::from_millis(80);

/**
 * A live scan indicator drawn on stderr. We never know the total work up front (that's what the
 * scan is measuring), so completion is approximated and the counts are the honest part.
 */
pub struct Progress {
    enabled: bool,
    frame: usize,
    dirs: u64,
    bytes: u64,
    fraction: f64,
    last_draw: Instant,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        Self::new_with_terminal(enabled, stderr().is_terminal())
    }

    fn new_with_terminal(enabled: bool, terminal: bool) -> Self {
        Progress {
            // Only draw when explicitly enabled AND stderr is a terminal, so piping stays clean.
            enabled: enabled && terminal,
            frame: 0,
            dirs: 0,
            bytes: 0,
            fraction: 0.0,
            last_draw: Instant::now(),
        }
    }

    /**
     * Record a completed batch. `pending` is the number of scans still outstanding.
     */
    pub fn update(&mut self, directories: usize, pending: usize, bytes: u64) {
        let redraw = self.enabled && self.last_draw.elapsed() >= REDRAW_INTERVAL;
        self.update_with_redraw(directories, pending, bytes, redraw);
    }

    /// Keep the remaining work visible after traversal has finished.
    pub fn phase(&mut self, description: &str) {
        if !self.enabled { return; }
        let mut out = stderr();
        let _ = write!(out, "\r{description:<70}");
        let _ = out.flush();
    }

    fn update_with_redraw(&mut self, directories: usize, pending: usize, bytes: u64, redraw: bool) {
        self.dirs += directories as u64;
        self.bytes += bytes;

        if !self.enabled {
            return;
        }

        // We can't know the total up front, so approximate completion as
        // processed / (processed + pending), clamped so the bar never runs backwards when a
        // directory reveals many new subdirectories. It reaches 1.0 exactly when pending hits 0.
        let total = self.dirs + pending as u64;
        let fraction = self.dirs as f64 / total as f64;
        if fraction > self.fraction {
            self.fraction = fraction;
        }

        if redraw {
            self.draw();
        }
    }

    fn draw(&mut self) {
        let mut err = stderr();
        self.draw_to(&mut err);
    }

    fn draw_to(&mut self, mut out: impl Write) {
        self.frame = (self.frame + 1) % FRAMES.len();
        self.last_draw = Instant::now();

        let filled = ((self.fraction * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
        let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
        let pct = (self.fraction * 100.0).round() as u64;

        let line = format!(
            "{} [{}] {:>3}%  {} dirs  {}",
            FRAMES[self.frame], bar, pct, self.dirs, human_plain(self.bytes)
        );

        // Left-pad to a fixed width to overwrite any leftover from a previous, longer line.
        let _ = write!(out, "\r{:<70}", line);
        let _ = out.flush();
    }

    /**
     * Clear the indicator line so it doesn't linger above the report.
     */
    pub fn finish(&mut self) {
        let mut err = stderr();
        self.finish_to(&mut err);
    }

    fn finish_to(&mut self, mut out: impl Write) {
        if !self.enabled {
            return;
        }
        let _ = write!(out, "\r{:<70}\r", "");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_progress_counts_without_drawing() {
        let mut progress = Progress::new_with_terminal(true, false);
        progress.update_with_redraw(4, 4, 1024, true);
        assert_eq!(progress.dirs, 4);
        assert_eq!(progress.bytes, 1024);
        assert_eq!(progress.fraction, 0.0);
        let mut output = Vec::new();
        progress.finish_to(&mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn enabled_progress_is_monotonic_draws_frames_and_clears_line() {
        let mut progress = Progress::new_with_terminal(true, true);
        progress.update_with_redraw(1, 3, 1000, false);
        assert_eq!(progress.fraction, 0.25);
        progress.update_with_redraw(1, 20, 2000, false);
        assert_eq!(progress.fraction, 0.25);
        progress.update_with_redraw(1, 0, 3000, false);
        assert_eq!(progress.fraction, 1.0);

        let mut output = Vec::new();
        progress.draw_to(&mut output);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("100%"));
        assert!(text.contains("3 dirs"));
        assert!(text.contains("6K"));
        assert_eq!(progress.frame, 1);

        let mut cleared = Vec::new();
        progress.finish_to(&mut cleared);
        assert_eq!(cleared.first(), Some(&b'\r'));
        assert_eq!(cleared.last(), Some(&b'\r'));
    }

}
