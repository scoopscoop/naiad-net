//! Relation sync DTOs: signed sibling/parent submissions and the bulk
//! relation graph. Tag-keyed analogues of `Submission` / `Snapshot`.

use serde::{Deserialize, Serialize};

/// Which kind of directed tag relation an edge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelKind {
    /// `from` (the alias / "bad" tag) collapses to `to` (the ideal).
    Sibling,
    /// `from` (the child) implies `to` (the parent).
    Parent,
}

impl RelKind {
    /// The canonical lowercase token used in the signed bytes and on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RelKind::Sibling => "sibling",
            RelKind::Parent => "parent",
        }
    }
}

/// Whether a delta edge is a live edge or a tombstone. The incremental analogue
/// of the repo's `status` column; serialized lowercase to match `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeStatus {
    /// A live edge.
    Current,
    /// A retracted edge (tombstone). Shipped explicitly so an incremental pull
    /// can reconstruct a retraction that full-graph replace got "for free".
    Deleted,
}

/// One signed relation operation submitted to a repository. `from`/`to` are the
/// normalized tag strings the signature covers; `author`/`signature` are hex.
/// For a sibling, `from` is the alias and `to` the ideal; for a parent, `from`
/// is the child and `to` the parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationSubmission {
    pub version: u32,
    pub op: crate::Op,
    pub kind: RelKind,
    pub from: String,
    pub to: String,
    pub author: String,
    pub signature: String,
    /// Which repo's corpus this row belongs to: a 64-hex genesis identity key
    /// (#84 §1). UNSIGNED carrier-asserted metadata — never part of the
    /// canonical bytes, so signatures are unaffected. Absent on pre-#84 wires;
    /// the ingester then assigns the peer's origin (keyed) or `local` (unkeyed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// One directed relation edge with the account that asserted it (pubkey hex).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoredEdge {
    pub from: String,
    pub to: String,
    pub author: String,
}

/// A repository's whole relation graph: current sibling and parent edges, one
/// deterministic author per edge. The bulk-read analogue of `Snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationGraph {
    pub version: u32,
    /// The repo's current high-watermark (`MAX(seq)`, or 0 when empty). New in the
    /// incremental design; absent on pre-incremental repos → `#[serde(default)]`.
    #[serde(default)]
    pub cursor: u64,
    pub siblings: Vec<AuthoredEdge>,
    pub parents: Vec<AuthoredEdge>,
}

/// One changed relation edge in an incremental pull: the full key, the author
/// that asserted it, its current `status`, and the `seq` it was assigned. Unlike
/// `AuthoredEdge` (one deduped winner per edge), a delta carries every author's
/// row and includes tombstones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaEdge {
    pub kind: RelKind,
    pub from: String,
    pub to: String,
    pub author: String,
    pub status: EdgeStatus,
    pub seq: u64,
}

/// The response to `GET /repo/relations?since=N`: every edge with `seq > N`
/// (ordered by `seq`, tombstones included) plus the new high-watermark `cursor`.
/// `since=0` yields the full set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationDelta {
    pub version: u32,
    pub cursor: u64,
    pub edges: Vec<DeltaEdge>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Op, PROTOCOL_VERSION};

    #[test]
    fn relkind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(RelKind::Sibling).unwrap(),
            serde_json::json!("sibling")
        );
        assert_eq!(
            serde_json::to_value(RelKind::Parent).unwrap(),
            serde_json::json!("parent")
        );
    }

    #[test]
    fn relkind_as_str_matches_serde() {
        for k in [RelKind::Sibling, RelKind::Parent] {
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
    }

    #[test]
    fn submission_round_trips() {
        let s = RelationSubmission {
            version: PROTOCOL_VERSION,
            op: Op::Add,
            kind: RelKind::Sibling,
            from: "character:samus_aran".into(),
            to: "character:samus".into(),
            author: "ab".repeat(32),
            signature: "cd".repeat(64),
            origin: None,
        };
        let back: RelationSubmission =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn graph_round_trips() {
        let g = RelationGraph {
            version: PROTOCOL_VERSION,
            cursor: 0,
            siblings: vec![AuthoredEdge {
                from: "character:samus_aran".into(),
                to: "character:samus".into(),
                author: "ab".repeat(32),
            }],
            parents: vec![AuthoredEdge {
                from: "character:samus".into(),
                to: "series:metroid".into(),
                author: "ab".repeat(32),
            }],
        };
        let back: RelationGraph =
            serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(g, back);
    }

    #[test]
    fn edge_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(EdgeStatus::Current).unwrap(),
            serde_json::json!("current")
        );
        assert_eq!(
            serde_json::to_value(EdgeStatus::Deleted).unwrap(),
            serde_json::json!("deleted")
        );
    }

    #[test]
    fn relation_delta_round_trips() {
        let d = RelationDelta {
            version: PROTOCOL_VERSION,
            cursor: 42,
            edges: vec![DeltaEdge {
                kind: RelKind::Sibling,
                from: "character:samus_aran".into(),
                to: "character:samus".into(),
                author: "ab".repeat(32),
                status: EdgeStatus::Deleted,
                seq: 42,
            }],
        };
        let back: RelationDelta =
            serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn relation_graph_carries_cursor_and_tolerates_its_absence() {
        let g = RelationGraph {
            version: PROTOCOL_VERSION,
            cursor: 7,
            siblings: Vec::new(),
            parents: Vec::new(),
        };
        let back: RelationGraph =
            serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(g, back);
        let old = r#"{"version":3,"siblings":[],"parents":[]}"#;
        let parsed: RelationGraph = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.cursor, 0);
    }
}
