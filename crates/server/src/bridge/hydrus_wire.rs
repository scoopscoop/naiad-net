//! Decode Hydrus update files (issue #124). Wire format (hydrus/core/
//! HydrusSerialisable.py, hydrus/core/networking/HydrusNetwork.py):
//! each update file is `zlib(json([type, version, info]))`. Types:
//! ContentUpdate=34, DefinitionsUpdate=36, Metadata=37. v1 handles 34 and 36.
//! Within a ContentUpdate, CONTENT_TYPE_MAPPINGS=0, CONTENT_TYPE_SIBLINGS=1 and
//! CONTENT_TYPE_PARENTS=2 are all decoded into structured rows: mappings feed the
//! sha256-keyed store, siblings/parents feed naiad's tag-keyed relations table
//! via the bridge author (#225). All three ship as id pairs resolved
//! through the same `DefinitionsUpdate` dictionary.

use std::io::Read;

use anyhow::{Context, anyhow, bail};
use serde_json::Value;

const TYPE_CONTENT_UPDATE: i64 = 34;
const TYPE_DEFINITIONS_UPDATE: i64 = 36;

const DEFINITIONS_TYPE_HASHES: i64 = 0;
const DEFINITIONS_TYPE_TAGS: i64 = 1;

const CONTENT_TYPE_MAPPINGS: i64 = 0;
const CONTENT_TYPE_SIBLINGS: i64 = 1;
const CONTENT_TYPE_PARENTS: i64 = 2;
const ACTION_ADD: i64 = 0;
const ACTION_DELETE: i64 = 1;

/// Per-update tally of content the decoder could not classify — the
/// upstream-drift tripwire. Siblings and parents are no longer counted here:
/// they are decoded into [`ContentUpdate::siblings`]/[`ContentUpdate::parents`]
/// and applied to the relations table (#225), so their "applied" counts come
/// from those vectors' lengths. Only the unknown-kind counters remain, so a
/// non-zero `SkipCounts` still means "PTR format we did not recognise".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkipCounts {
    pub unknown_content_type: usize,
    pub unknown_def_kind: usize,
}

impl SkipCounts {
    /// Field-wise sum, for aggregating across the files of one update index.
    pub fn merge(&mut self, other: SkipCounts) {
        self.unknown_content_type += other.unknown_content_type;
        self.unknown_def_kind += other.unknown_def_kind;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Add,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionsUpdate {
    /// (service_hash_id, sha256_hex)
    pub hashes: Vec<(u64, String)>,
    /// (service_tag_id, tag_string)
    pub tags: Vec<(u64, String)>,
    /// Count of definition entries with an unrecognised kind (upstream-drift tripwire).
    pub unknown_def_kind: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingRow {
    pub tag_id: u64,
    pub hash_ids: Vec<u64>,
    pub action: Action,
}

/// One decoded sibling/parent relation row: a `(from_tag_id, to_tag_id)` integer
/// pair under a group `Action`, resolved through the definitions dictionary
/// exactly like a mapping row's `tag_id` (confirmed against the live PTR, §4).
/// For a sibling, `from_id` is the alias (`bad_tag_id`) and `to_id` the ideal
/// (`good_tag_id`); for a parent, `from_id` is the child and `to_id` the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationRow {
    pub from_id: u64,
    pub to_id: u64,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentUpdate {
    pub mappings: Vec<MappingRow>,
    /// Decoded `CONTENT_TYPE_SIBLINGS` rows (bad → good), applied as `Sibling`
    /// relations by the bridge author (#225).
    pub siblings: Vec<RelationRow>,
    /// Decoded `CONTENT_TYPE_PARENTS` rows (child → parent), applied as `Parent`
    /// relations by the bridge author (#225).
    pub parents: Vec<RelationRow>,
    /// Structured counts of rows the decoder could not classify (drift tripwire).
    pub skips: SkipCounts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Definitions(DefinitionsUpdate),
    Content(ContentUpdate),
}

/// zlib-inflate `bytes`. Hydrus writes zlib level 9; a client may instead write
/// lz4.block (HydrusCompression.py tries zlib then lz4). v1 supports zlib only
/// and names lz4 in the error so the failure is actionable.
fn inflate(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| {
            anyhow!("zlib inflate failed ({e}); lz4.block bodies are unsupported in v1")
        })?;
    Ok(out)
}

/// Decode a raw Hydrus update file into a typed [`Update`].
///
/// # Errors
/// Returns an error if the body is not zlib, is not the `[type, version, info]`
/// envelope, or carries an unsupported top-level type.
pub fn decode_update(bytes: &[u8]) -> anyhow::Result<Update> {
    let json = inflate(bytes)?;
    let root: Value = serde_json::from_slice(&json).context("update is not JSON")?;
    let arr = root
        .as_array()
        .ok_or_else(|| anyhow!("update envelope is not an array"))?;
    let ty = arr
        .first()
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("missing type"))?;
    let info = arr.get(2).ok_or_else(|| anyhow!("missing info payload"))?;
    match ty {
        TYPE_DEFINITIONS_UPDATE => Ok(Update::Definitions(decode_definitions(info)?)),
        TYPE_CONTENT_UPDATE => Ok(Update::Content(decode_content(info)?)),
        other => {
            bail!("unsupported Hydrus update type {other} (v1 handles 34 content, 36 definitions)")
        }
    }
}

/// Hydrus serialises some small enum keys as JSON ints and some as numeric
/// strings (e.g. live PTR DefinitionsUpdate sections use "0"/"1"). Accept both.
fn as_enum_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn decode_definitions(info: &Value) -> anyhow::Result<DefinitionsUpdate> {
    let mut hashes = Vec::new();
    let mut tags = Vec::new();
    let mut unknown_def_kind = 0usize;
    for section in info
        .as_array()
        .ok_or_else(|| anyhow!("definitions info not an array"))?
    {
        let s = section
            .as_array()
            .ok_or_else(|| anyhow!("definitions section not an array"))?;
        let kind = s
            .first()
            .and_then(as_enum_i64)
            .ok_or_else(|| anyhow!("definitions section kind"))?;
        let pairs = s
            .get(1)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("definitions pairs"))?;
        if kind != DEFINITIONS_TYPE_HASHES && kind != DEFINITIONS_TYPE_TAGS {
            unknown_def_kind += pairs.len(); // unknown kind: count, do not parse
            continue;
        }
        for pair in pairs {
            let p = pair
                .as_array()
                .ok_or_else(|| anyhow!("definitions pair not an array"))?;
            let id = p
                .first()
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("definitions id"))?;
            let s_val = p
                .get(1)
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("definitions string"))?;
            match kind {
                DEFINITIONS_TYPE_HASHES => hashes.push((id, s_val.to_string())),
                DEFINITIONS_TYPE_TAGS => tags.push((id, s_val.to_string())),
                _ => unreachable!(),
            }
        }
    }
    Ok(DefinitionsUpdate {
        hashes,
        tags,
        unknown_def_kind,
    })
}

fn decode_content(info: &Value) -> anyhow::Result<ContentUpdate> {
    let mut mappings = Vec::new();
    let mut siblings = Vec::new();
    let mut parents = Vec::new();
    let mut skips = SkipCounts::default();
    for block in info
        .as_array()
        .ok_or_else(|| anyhow!("content info not an array"))?
    {
        let b = block
            .as_array()
            .ok_or_else(|| anyhow!("content block not an array"))?;
        let content_type = b
            .first()
            .and_then(as_enum_i64)
            .ok_or_else(|| anyhow!("content_type"))?;
        let action_groups = b
            .get(1)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("action groups"))?;
        for group in action_groups {
            let g = group
                .as_array()
                .ok_or_else(|| anyhow!("action group not an array"))?;
            let action = g
                .first()
                .and_then(as_enum_i64)
                .ok_or_else(|| anyhow!("action"))?;
            let rows = g
                .get(1)
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("rows"))?;
            // An unknown content type is counted (drift tripwire), never parsed.
            if content_type != CONTENT_TYPE_MAPPINGS
                && content_type != CONTENT_TYPE_SIBLINGS
                && content_type != CONTENT_TYPE_PARENTS
            {
                skips.unknown_content_type += rows.len();
                continue;
            }
            let act = match action {
                ACTION_ADD => Action::Add,
                ACTION_DELETE => Action::Delete,
                // PEND/PETITION etc. are client->server only; never in a server update.
                other => bail!("unexpected content action {other} in a server ContentUpdate"),
            };
            match content_type {
                CONTENT_TYPE_MAPPINGS => {
                    for row in rows {
                        let r = row
                            .as_array()
                            .ok_or_else(|| anyhow!("mappings row not an array"))?;
                        let tag_id = r
                            .first()
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow!("mappings tag_id"))?;
                        let hash_ids = r
                            .get(1)
                            .and_then(Value::as_array)
                            .ok_or_else(|| anyhow!("mappings hash_ids"))?
                            .iter()
                            .map(|h| h.as_u64().ok_or_else(|| anyhow!("hash id not u64")))
                            .collect::<anyhow::Result<Vec<u64>>>()?;
                        mappings.push(MappingRow {
                            tag_id,
                            hash_ids,
                            action: act,
                        });
                    }
                }
                // Siblings/parents are `(from_tag_id, to_tag_id)` id pairs (§4),
                // decoded identically; the destination vector differs by kind.
                CONTENT_TYPE_SIBLINGS | CONTENT_TYPE_PARENTS => {
                    let dst = if content_type == CONTENT_TYPE_SIBLINGS {
                        &mut siblings
                    } else {
                        &mut parents
                    };
                    for row in rows {
                        let r = row
                            .as_array()
                            .ok_or_else(|| anyhow!("relation row not an array"))?;
                        let from_id = r
                            .first()
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow!("relation from_tag_id"))?;
                        let to_id = r
                            .get(1)
                            .and_then(Value::as_u64)
                            .ok_or_else(|| anyhow!("relation to_tag_id"))?;
                        dst.push(RelationRow {
                            from_id,
                            to_id,
                            action: act,
                        });
                    }
                }
                _ => unreachable!("content_type filtered above"),
            }
        }
    }
    Ok(ContentUpdate {
        mappings,
        siblings,
        parents,
        skips,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    fn zlib(v: &serde_json::Value) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::new(9));
        e.write_all(serde_json::to_string(v).unwrap().as_bytes())
            .unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn decodes_definitions_update() {
        // [36, 1, [[0, [[hid, sha_hex]...]], [1, [[tid, tag]...]]]]
        let sha = "ab".repeat(32);
        let v = serde_json::json!([36, 1, [[0, [[7, sha]]], [1, [[9, "character:samus"]]]]]);
        let update = decode_update(&zlib(&v)).unwrap();
        match update {
            Update::Definitions(d) => {
                assert_eq!(d.hashes, vec![(7u64, "ab".repeat(32))]);
                assert_eq!(d.tags, vec![(9u64, "character:samus".to_string())]);
            }
            other => panic!("expected Definitions, got {other:?}"),
        }
    }

    #[test]
    fn decodes_content_update_mappings_add_and_delete() {
        // [34, 1, [[0, [[0, [[tid, [hid,hid]]]], [1, [[tid2, [hid3]]]]]]]]
        let v = serde_json::json!([34, 1, [[0, [[0, [[5, [100, 101]]]], [1, [[6, [102]]]]]]]]);
        let update = decode_update(&zlib(&v)).unwrap();
        match update {
            Update::Content(c) => {
                assert_eq!(c.mappings.len(), 2);
                let add = c.mappings.iter().find(|m| m.action == Action::Add).unwrap();
                assert_eq!(add.tag_id, 5);
                assert_eq!(add.hash_ids, vec![100, 101]);
                let del = c
                    .mappings
                    .iter()
                    .find(|m| m.action == Action::Delete)
                    .unwrap();
                assert_eq!(del.tag_id, 6);
                assert_eq!(del.hash_ids, vec![102]);
                assert_eq!(c.skips, SkipCounts::default());
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn decodes_sibling_rows() {
        // content_type 1 (TAG_SIBLINGS): one ADD group and one DELETE group, each
        // carrying `(from_tag_id, to_tag_id)` integer pairs (live PTR shape, §4).
        let v = serde_json::json!([
            34,
            1,
            [[1, [[0, [[2828489, 2551185]]], [1, [[8103245, 2551185]]]]]]
        ]);
        match decode_update(&zlib(&v)).unwrap() {
            Update::Content(c) => {
                assert!(c.mappings.is_empty());
                assert!(c.parents.is_empty());
                assert_eq!(
                    c.siblings,
                    vec![
                        RelationRow {
                            from_id: 2828489,
                            to_id: 2551185,
                            action: Action::Add,
                        },
                        RelationRow {
                            from_id: 8103245,
                            to_id: 2551185,
                            action: Action::Delete,
                        },
                    ]
                );
                assert_eq!(c.skips, SkipCounts::default(), "skips is unknown-only");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn decodes_parent_rows() {
        // content_type 2 (TAG_PARENTS): one ADD and one DELETE `(child, parent)` pair.
        let v = serde_json::json!([34, 1, [[2, [[0, [[10, 20]]], [1, [[30, 40]]]]]]]);
        match decode_update(&zlib(&v)).unwrap() {
            Update::Content(c) => {
                assert!(c.mappings.is_empty());
                assert!(c.siblings.is_empty());
                assert_eq!(
                    c.parents,
                    vec![
                        RelationRow {
                            from_id: 10,
                            to_id: 20,
                            action: Action::Add,
                        },
                        RelationRow {
                            from_id: 30,
                            to_id: 40,
                            action: Action::Delete,
                        },
                    ]
                );
                assert_eq!(c.skips, SkipCounts::default(), "skips is unknown-only");
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_hard_error() {
        let v = serde_json::json!([99, 1, []]);
        let err = decode_update(&zlib(&v)).unwrap_err();
        assert!(
            format!("{err:#}").contains("99"),
            "names the unknown type: {err:#}"
        );
    }

    #[test]
    fn non_zlib_body_names_lz4() {
        let err = decode_update(b"not-zlib-at-all").unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("lz4"));
    }

    #[test]
    fn unknown_content_type_is_counted_not_applied() {
        // A sibling block (populated siblings vector) alongside an unknown
        // content_type 7 block: the unknown rows are counted, and they do not
        // leak into the decoded siblings/parents.
        let v = serde_json::json!([
            34,
            1,
            [[1, [[0, [[5, 6]]]]], [7, [[0, [["x", "y"], ["p", "q"]]]]]]
        ]);
        match decode_update(&zlib(&v)).unwrap() {
            Update::Content(c) => {
                assert!(c.mappings.is_empty());
                assert_eq!(c.siblings.len(), 1, "the real sibling row decoded");
                assert!(c.parents.is_empty());
                assert_eq!(c.skips.unknown_content_type, 2);
            }
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[test]
    fn unknown_definitions_kind_is_counted() {
        // definitions kind 9 (unknown) with two pairs.
        let v = serde_json::json!([36, 1, [[9, [[1, "a"], [2, "b"]]]]]);
        match decode_update(&zlib(&v)).unwrap() {
            Update::Definitions(d) => {
                assert!(d.hashes.is_empty() && d.tags.is_empty());
                assert_eq!(d.unknown_def_kind, 2);
            }
            other => panic!("expected Definitions, got {other:?}"),
        }
    }

    #[test]
    fn decodes_definitions_update_string_section_keys() {
        // Live PTR defs files use STRING section keys: ["0", ...], ["1", ...].
        let v = serde_json::json!([36, 1, [["0", [[7, "aa"]]], ["1", [[9, "character:samus"]]]]]);
        match decode_update(&zlib(&v)).unwrap() {
            Update::Definitions(d) => {
                assert_eq!(d.hashes, vec![(7u64, "aa".to_string())]);
                assert_eq!(d.tags, vec![(9u64, "character:samus".to_string())]);
                assert_eq!(d.unknown_def_kind, 0);
            }
            other => panic!("expected Definitions, got {other:?}"),
        }
    }

    #[test]
    fn type_35_is_rejected_with_message() {
        // 35 was previously the wrong constant; it must now produce an error
        // naming the type, not silently succeed.
        let v = serde_json::json!([35, 1, []]);
        let err = decode_update(&zlib(&v)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("35"),
            "error should name the rejected type: {msg}"
        );
        assert!(
            msg.contains("unsupported"),
            "error should say unsupported: {msg}"
        );
    }
}
