//! Poison-recovery re-export.
//!
//! The canonical `LockRecover` implementation was lifted from this module into
//! [`naiad_core::LockRecover`] in issue #137 so that the server crate can share
//! the same helper without duplicating it. This module re-exports it with
//! `pub(crate)` so all existing `use crate::lock::LockRecover` sites in this
//! crate continue to work without change.
//!
//! See also: `crates/server/src/http.rs` for the parallel fix on the server
//! side (#137).

pub(crate) use naiad_core::LockRecover;
