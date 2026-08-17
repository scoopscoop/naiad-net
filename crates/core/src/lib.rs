//! Core domain types shared across Naiad crates.
//!
//! This crate is dependency-light on purpose: it holds the vocabulary (content
//! [`Hash`], [`FileRecord`]) that the indexer, database, and CLI all speak. Keep
//! cross-crate types here rather than duplicating them.

mod bucket;
mod error;
mod hash;
mod lock;
mod parity;
mod pathenc;
mod query;
mod record;
mod relations;
mod state;
mod tag;

pub use bucket::{bucket_key, bucket_upper};
pub use error::{
    BUCKET_ROW_OVERHEAD, BudgetExceeded, Error, RESPONSE_ENVELOPE_OVERHEAD, approx_row_cost,
    json_escaped_len,
};
pub use hash::{Hash, hash_bytes, hash_reader, hash_reader_dual};
pub use lock::LockRecover;
pub use parity::PairDigest;
pub use pathenc::{path_from_bytes, path_to_bytes};
pub use query::{
    CmpOp, MatchMode, Predicate, Query, SysField, SystemPredicate, TagPattern, parse_query,
    tokenize,
};
pub use record::{FileContent, FileMetadata, FileRecord, Location};
pub use relations::{
    ParentEdges, RelationCapped, RelationGraph, RelationSections, SiblingEdges, canonicalize,
    effective_tags, match_set,
};
pub use state::FileState;
pub use tag::Tag;

/// Normalise a tag component: trim, collapse internal whitespace to single
/// spaces, and lowercase. Shared between [`Tag::parse`] and the DB
/// completion-token splitter so both agree on the canonical form.
pub fn tag_normalize(s: &str) -> String {
    tag::normalize(s)
}

/// Convenience result type for this crate.
pub type Result<T> = std::result::Result<T, Error>;
