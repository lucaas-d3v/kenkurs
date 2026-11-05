//! A silent output driven by a real-time clock.
//!
//! It renders the mix at exactly the pace a device would and discards it, so playback timing,
//! progress and end-of-track events behave identically without any audio hardware.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::super::CHANNELS;
use super::super::mixer::Mixer;

/// 10 ms at 48 kHz.
const BLOCK_FRAMES: usize = 480;

pub(crate) struct NullOutput {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NullOutput {
    pub(crate) fn start(mut mixer: Mixer) -> std::io::Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&running);
        let period = Duration::from_secs_f64(BLOCK_FRAMES as f64 / f64::from(mixer.sample_rate()));
        let thread = thread::Builder::new().name("kenkurs-null-out".into()).spawn(move || {
            let mut buffer = vec![0.0f32; BLOCK_FRAMES * CHANNELS];
            let mut deadline = Instant::now();
            while flag.load(Ordering::Relaxed) {
                mixer.render(&mut buffer);
                deadline += period;
                let now = Instant::now();
                if let Some(wait) = deadline.checked_duration_since(now) {
                    thread::sleep(wait);
                } else if now - deadline > period * 50 {
                    // Fell far behind (e.g. the machine was suspended): resync instead of bursting.
                    deadline = now;
                }
            }
        })?;
        Ok(Self { running, thread: Some(thread) })
    }
}

impl Drop for NullOutput {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
