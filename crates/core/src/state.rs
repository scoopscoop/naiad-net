//! Lifecycle state of a file's *content* (the `files.state` column).
//!
//! Phase 1 only ever writes [`FileState::Active`]; the other variants exist so
//! later passes (archive/trash) have the vocabulary without a schema change.

use std::str::FromStr;

use crate::Error;

/// The lifecycle state stored in `files.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// Live content in the active library.
    Active,
    /// Hidden from default views but retained.
    Archived,
    /// Marked for deletion but not yet purged.
    Trashed,
}

impl FileState {
    /// The canonical lowercase string stored in SQLite.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FileState::Active => "active",
            FileState::Archived => "archived",
            FileState::Trashed => "trashed",
        }
    }
}

impl FromStr for FileState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(FileState::Active),
            "archived" => Ok(FileState::Archived),
            "trashed" => Ok(FileState::Trashed),
            other => Err(Error::InvalidFileState(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        for state in [FileState::Active, FileState::Archived, FileState::Trashed] {
            assert_eq!(state.as_str().parse::<FileState>().unwrap(), state);
        }
    }

    #[test]
    fn canonical_strings_are_stable() {
        assert_eq!(FileState::Active.as_str(), "active");
        assert_eq!(FileState::Archived.as_str(), "archived");
        assert_eq!(FileState::Trashed.as_str(), "trashed");
    }

    #[test]
    fn unknown_string_is_rejected() {
        let err = "bogus".parse::<FileState>().unwrap_err();
        assert!(matches!(err, Error::InvalidFileState(s) if s == "bogus"));
    }
}
