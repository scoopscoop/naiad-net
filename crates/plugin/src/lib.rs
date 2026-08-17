//! `naiad-plugin` — the in-process plugin contract.
//!
//! Capability traits (`Tagger`/`Processor`/`Source`) describe what a plugin can
//! do; a `Registry` indexes plugins by id. A `Sink` is the tier-agnostic output
//! of a bulk `Source` import — the same `Source` feeds a small client library or
//! a large server store depending on which `Sink` the host supplies.

use naiad_core::Tag;

/// Error type for plugin operations.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PluginError(pub String);

/// Convenience alias.
pub type Result<T> = std::result::Result<T, PluginError>;

/// Status of an imported record, mirroring Naiad's `status` column and Hydrus's
/// current/deleted split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    /// A live mapping/relation.
    Current,
    /// A tombstone (explicit removal); wins over `Current` for the same key.
    Deleted,
}

/// Which relation a `RelationRecord` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// `from` (bad/alias) collapses to `to` (ideal).
    Sibling,
    /// `from` (child) implies `to` (parent).
    Parent,
}

/// A file as a plugin sees it for an on-demand lookup.
#[derive(Debug, Clone)]
pub struct FileRef {
    /// BLAKE3 hex (Naiad identity).
    pub blake3: String,
    /// SHA-256 hex interop key, if known.
    pub sha256: Option<String>,
}

/// One normalized file→tag record emitted by a bulk `Source`.
#[derive(Debug, Clone)]
pub struct MappingRecord {
    /// SHA-256 hex of the file the tag belongs to.
    pub sha256: String,
    /// The normalized tag.
    pub tag: Tag,
    pub status: RecordStatus,
}

/// One normalized tag→tag relation emitted by a bulk `Source`.
#[derive(Debug, Clone)]
pub struct RelationRecord {
    pub kind: RelationKind,
    /// Sibling: the alias (bad). Parent: the child.
    pub from: Tag,
    /// Sibling: the ideal. Parent: the parent.
    pub to: Tag,
    pub status: RecordStatus,
}

/// Counters returned by a bulk import.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub mappings: u64,
    pub siblings: u64,
    pub parents: u64,
}

/// The tier-agnostic destination for a bulk `Source` import.
pub trait Sink {
    /// Accept one file→tag record.
    ///
    /// # Errors
    /// Returns an error if the record cannot be stored.
    fn mapping(&mut self, rec: MappingRecord) -> Result<()>;
    /// Accept one tag→tag relation record.
    ///
    /// # Errors
    /// Returns an error if the record cannot be stored.
    fn relation(&mut self, rec: RelationRecord) -> Result<()>;
}

/// Declared capabilities of a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub tagger: bool,
    pub processor: bool,
    pub source: bool,
}

/// Every plugin is identified and declares its capabilities.
pub trait Plugin: Send {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
}

/// On-demand, per-file tag lookup.
pub trait Tagger: Plugin {
    /// Return candidate tags for `file` (no side effects — preview-safe).
    ///
    /// # Errors
    /// Returns an error if the lookup fails.
    fn tags_for(&self, file: &FileRef) -> Result<Vec<Tag>>;
}

/// Side-effecting action over a file and its tags.
pub trait Processor: Plugin {
    /// Perform the processor's action.
    ///
    /// # Errors
    /// Returns an error if the action fails.
    fn process(&self, file: &FileRef, tags: &[Tag]) -> Result<()>;
}

/// Bulk ingest from an external store into a `Sink`.
pub trait Source: Plugin {
    /// Stream all records into `sink`.
    ///
    /// # Errors
    /// Returns an error if extraction fails.
    fn bulk_import(&self, sink: &mut dyn Sink) -> Result<ImportStats>;
}

/// Summary of one registered plugin, for `plugins.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub capabilities: Capabilities,
}

/// An in-process registry of plugins, keyed by id.
#[derive(Default)]
pub struct Registry {
    plugins: Vec<Box<dyn Plugin>>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Register a plugin.
    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// Number of registered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The plugin with `id`, if registered.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&dyn Plugin> {
        self.plugins
            .iter()
            .find(|p| p.id() == id)
            .map(AsRef::as_ref)
    }

    /// Summaries of all registered plugins.
    #[must_use]
    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                id: p.id().to_string(),
                name: p.name().to_string(),
                capabilities: p.capabilities(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;
    impl Plugin for Dummy {
        fn id(&self) -> &str {
            "dummy"
        }
        fn name(&self) -> &str {
            "Dummy"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tagger: true,
                processor: false,
                source: true,
            }
        }
    }

    #[test]
    fn registry_registers_and_lists() {
        let mut reg = Registry::new();
        reg.register(Box::new(Dummy));
        assert_eq!(reg.len(), 1);
        let info = reg.list();
        assert_eq!(info[0].id, "dummy");
        assert!(info[0].capabilities.tagger);
        assert!(info[0].capabilities.source);
        assert!(!info[0].capabilities.processor);
        assert!(reg.get("dummy").is_some());
        assert!(reg.get("missing").is_none());
    }
}
