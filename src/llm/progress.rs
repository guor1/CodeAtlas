//! Progress reporting for the one command that is slow: `catlas deepen`.
//!
//! A domain dossier is a single large request that can run for minutes, and the
//! command used to print nothing until every domain was done — indistinguishable
//! from a hang, which is the worst failure mode for something that spends money.
//!
//! Everything here goes to **stderr**, so piping stdout to a file still captures
//! the report alone.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Don't start ticking until a call has run this long: a cache hit returns in
/// milliseconds, and a line that flashes by is just noise.
const QUIET_PERIOD: u64 = 3;

/// A "still working" ticker covering one model call.
///
/// Ticks only when stderr is a terminal — it redraws one line with `\r`, which
/// would otherwise fill a log file with thousands of near-identical lines. In a
/// pipe the surrounding step lines carry the information instead.
pub struct Heartbeat {
    done: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
}

impl Heartbeat {
    pub fn start() -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let ticker = std::io::stderr().is_terminal().then(|| {
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let started = Instant::now();
                let mut shown = 0;
                while !done.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                    let secs = started.elapsed().as_secs();
                    if secs >= QUIET_PERIOD && secs != shown {
                        shown = secs;
                        let mut err = std::io::stderr().lock();
                        let _ = write!(err, "\r        等待模型响应 {secs}s …");
                        let _ = err.flush();
                    }
                }
            })
        });
        Self { done, ticker }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
            // Wipe the ticker line so the completion line starts clean.
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r{:40}\r", "");
            let _ = err.flush();
        }
    }
}

/// Token counts, abbreviated the way an operator reads them.
pub fn tokens(n: usize) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_abbreviate_past_a_thousand() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1000), "1.0k");
        assert_eq!(tokens(12_432), "12.4k");
    }

    #[test]
    fn dropping_a_heartbeat_stops_its_ticker() {
        let hb = Heartbeat::start();
        thread::sleep(Duration::from_millis(30));
        drop(hb); // must return promptly: Drop signals, then joins the ticker
    }
}
