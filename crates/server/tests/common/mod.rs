//! Shared test helpers for naiad-server integration tests.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix timestamp in seconds (i64). Used to build auth headers whose
/// `x-naiad-ts` value must be within ±300 s of the server's own clock.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
