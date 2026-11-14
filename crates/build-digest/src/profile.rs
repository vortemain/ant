//! Opt-in phase timing for performance work (`ANT_PROFILE=1`).
//!
//! Each [`mark`] prints the time since the previous mark and since the
//! process started measuring, on stderr, so a developer (or an agent) can see
//! where an `ant build` spends its time without a profiler. Off by default;
//! when off a mark costs one atomic load.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

struct State {
    start: Instant,
    last: Instant,
}

fn state() -> Option<&'static Mutex<State>> {
    static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            std::env::var_os("ANT_PROFILE")
                .filter(|value| !value.is_empty() && value != "0")
                .map(|_| {
                    let now = Instant::now();
                    Mutex::new(State {
                        start: now,
                        last: now,
                    })
                })
        })
        .as_ref()
}

/// Whether `ANT_PROFILE` is on for this process.
pub fn enabled() -> bool {
    state().is_some()
}

/// Record that the phase `label` just finished.
pub fn mark(label: &str) {
    if let Some(state) = state() {
        let mut state = state.lock().expect("profile state");
        let now = Instant::now();
        eprintln!(
            "[profile] {:>7.3}s  (+{:.3}s)  {label}",
            now.duration_since(state.start).as_secs_f64(),
            now.duration_since(state.last).as_secs_f64()
        );
        state.last = now;
    }
}
