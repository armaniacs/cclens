//! A single-line in-place activity indicator for long-running launches — the
//! headless `optimize` advisor. Like Claude Code's spinner: a turning frame
//! proves the process is alive, the status names what it is doing now, and
//! the elapsed counter makes waiting measurable. On a terminal the line
//! rewrites in place (ANSI) instead of scrolling; when stderr is piped the
//! same beats fall back to newline logs, so a pipe never receives escape
//! codes. Every raw event still goes to the run log — this only decides what
//! the terminal shows.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Braille spinner frames — one step per 100ms, a full turn per second.
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The spinner frame for `elapsed` — turning fast enough to read as alive,
/// slow enough that a screenshot still shows a clean glyph.
pub fn frame(elapsed: Duration) -> &'static str {
    FRAMES[(elapsed.as_millis() / 100) as usize % FRAMES.len()]
}

/// What the advisor is doing now, from its last tool call — the contextual
/// status ("Reading...", "Running shell..."). Unmapped tools read as
/// "running <name>", never a guess about something else.
pub fn activity_label(tool: &str) -> String {
    match tool {
        "shell" => "running shell".to_string(),
        "read" => "reading".to_string(),
        "edit" => "editing".to_string(),
        "write" => "writing".to_string(),
        "glob" | "grep" => "searching".to_string(),
        "webfetch" => "fetching".to_string(),
        "task" => "spawning subagent".to_string(),
        "todowrite" => "planning".to_string(),
        other => format!("running {other}"),
    }
}

/// One status line: the frame, the fixed role, the current activity, elapsed.
pub fn line(elapsed: Duration, activity: &str) -> String {
    format!(
        "{} advisor working — {} ({}s)",
        frame(elapsed),
        activity,
        elapsed.as_secs()
    )
}

/// Whether in-place ANSI redraws and dim painting are welcome here.
fn color_enabled(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// The live indicator. On a terminal it rewrites one stderr line in place
/// (~7 turns a second); when stderr is piped it logs a newline beat every
/// 10s instead. Progress events name the current activity; a beat covers
/// quiet stretches (model thinking, a long tool call) so silence always
/// reads as waiting, never as a hang. Dropping the indicator stops it.
pub struct Indicator {
    stop: Arc<AtomicBool>,
    events: Arc<AtomicUsize>,
    activity: Arc<Mutex<String>>,
    draw_lock: Arc<Mutex<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
    tty: bool,
}

impl Indicator {
    /// Start the beats on stderr.
    pub fn start() -> Self {
        let tty = std::io::stderr().is_terminal();
        let color = color_enabled(tty);
        let stop = Arc::new(AtomicBool::new(false));
        let events = Arc::new(AtomicUsize::new(0));
        let activity = Arc::new(Mutex::new("thinking".to_string()));
        let draw_lock = Arc::new(Mutex::new(()));
        let handle = {
            let stop = Arc::clone(&stop);
            let events = Arc::clone(&events);
            let activity = Arc::clone(&activity);
            let draw_lock = Arc::clone(&draw_lock);
            let start = Instant::now();
            std::thread::spawn(move || {
                if tty {
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(150));
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let text = {
                            let act = activity
                                .lock()
                                .map(|a| a.clone())
                                .unwrap_or_else(|_| "thinking".to_string());
                            line(start.elapsed(), &act)
                        };
                        let painted = if color {
                            format!("\x1b[2m{text}\x1b[0m")
                        } else {
                            text
                        };
                        let _draw = draw_lock.lock();
                        let mut err = std::io::stderr();
                        let _ = write!(err, "\r\x1b[2K{painted}");
                        let _ = err.flush();
                    }
                } else {
                    let mut next = 10u64;
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let elapsed = start.elapsed().as_secs();
                        if elapsed >= next {
                            next += 10;
                            let n = events.load(Ordering::Relaxed);
                            let act = activity
                                .lock()
                                .map(|a| a.clone())
                                .unwrap_or_else(|_| "thinking".to_string());
                            eprintln!("  … advisor working — {act} ({elapsed}s, {n} events)");
                        }
                    }
                }
            })
        };
        Self {
            stop,
            events,
            activity,
            draw_lock,
            handle: Some(handle),
            tty,
        }
    }

    /// The advisor ran a tool — the status names it. When stderr is piped the
    /// marker is a newline line, the only place it can be seen.
    pub fn progress(&self, tool: &str) {
        self.activity
            .lock()
            .map(|mut a| *a = activity_label(tool))
            .ok();
        self.events.fetch_add(1, Ordering::Relaxed);
        if !self.tty {
            eprintln!("  → {tool}");
        }
    }

    /// The advisor produced text — it is thinking about what comes next.
    pub fn text_seen(&self) {
        self.activity
            .lock()
            .map(|mut a| *a = "thinking".to_string())
            .ok();
        self.events.fetch_add(1, Ordering::Relaxed);
    }

    /// An event with nothing to show still counts toward liveness.
    pub fn event(&self) {
        self.events.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear the in-place line so stdout can print without interleaving.
    pub fn clear(&self) {
        if self.tty {
            let _draw = self.draw_lock.lock();
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
    }

    /// Stop the beats and clear the line — the run is over.
    pub fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
        self.clear();
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        // Error paths may leave without an explicit finish; the beat thread
        // sees the flag within one tick and the line is cleared so whatever
        // prints next starts clean.
        self.stop.store(true, Ordering::Relaxed);
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spinner_frame_turns_with_elapsed_time() {
        assert_eq!(frame(Duration::from_millis(0)), "⠋");
        assert_eq!(frame(Duration::from_millis(150)), "⠙");
        assert_eq!(frame(Duration::from_millis(950)), "⠏");
        // A full turn wraps back to the first frame.
        assert_eq!(frame(Duration::from_millis(1000)), "⠋");
    }

    #[test]
    fn the_activity_label_names_the_common_tools() {
        assert_eq!(activity_label("shell"), "running shell");
        assert_eq!(activity_label("read"), "reading");
        assert_eq!(activity_label("grep"), "searching");
        assert_eq!(activity_label("task"), "spawning subagent");
        // Unmapped tools stay honest rather than guessed.
        assert_eq!(activity_label("mcp__docs"), "running mcp__docs");
    }

    #[test]
    fn the_status_line_names_the_activity_and_the_elapsed() {
        assert_eq!(
            line(Duration::from_secs(42), "reading"),
            "⠋ advisor working — reading (42s)"
        );
    }
}
