//! Seeded shuffle order shared by feeds and Up next.

pub use fp_core::shuffle::key;

/// A seed for a new shuffle.
pub fn new_seed() -> u64 {
    fp_core::playback::now_ms() as u64
}
