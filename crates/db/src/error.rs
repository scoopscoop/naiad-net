/// Errors produced by the database layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A SQLite-level error.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    /// A schema migration failed.
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),

    /// A relation edge whose endpoints are the same tag (a self-sibling or
    /// self-parent). Never meaningful, so it is rejected rather than stored.
    #[error("a tag cannot be its own sibling or parent")]
    SelfRelation,

    /// An operation targeted a row that does not exist.
    #[error("{0}")]
    NotFound(String),

    /// A caller-supplied value failed validation (e.g. a malformed block-rule
    /// target).
    #[error("{0}")]
    Invalid(String),
}
