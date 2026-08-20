//! Wire-format snapshot tests (README §10): every DTO's exact JSON is pinned
//! with `insta` so a wire change is always an explicit, reviewed snapshot diff.
//! The unit tests in `src/lib.rs` check round-trips; these pin the bytes.

use insta::assert_json_snapshot;
use naiad_api::*;

#[test]
fn file_and_scan_dtos() {
    assert_json_snapshot!(
        "file_dto",
        FileDto {
            hash: "a".repeat(64),
            name: "pic.png".into(),
            size: 1234,
            path: "/lib/pic.png".into(),
            imported_at: 100,
            created_at: Some(80),
            modified_at: Some(90),
            mime: Some("image/png".into()),
        }
    );
    assert_json_snapshot!(
        "scan_req",
        ScanReq {
            folder: "/lib".into()
        }
    );
    assert_json_snapshot!(
        "scan_summary",
        ScanSummary {
            imported: 3,
            marked_missing: 1,
            errors: vec![ScanError {
                path: "/x".into(),
                message: "boom".into()
            }],
        }
    );
    assert_json_snapshot!(
        "scan_error",
        ScanError {
            path: "/x".into(),
            message: "boom".into()
        }
    );
    assert_json_snapshot!(
        "scan_progress",
        ScanProgress {
            imported: 5,
            skipped: 2,
            total: 10
        }
    );
}

#[test]
fn tag_and_relation_dtos() {
    assert_json_snapshot!(
        "tags_req",
        TagsReq {
            file: "a".repeat(64),
            tags: vec!["character:samus".into()]
        }
    );
    assert_json_snapshot!(
        "sibling_dto",
        SiblingDto {
            bad: "samus_aran".into(),
            ideal: "character:samus".into()
        }
    );
    assert_json_snapshot!(
        "sibling_remove_req",
        SiblingRemoveReq {
            bad: "samus_aran".into()
        }
    );
    assert_json_snapshot!(
        "parent_dto",
        ParentDto {
            child: "character:samus".into(),
            parent: "series:metroid".into()
        }
    );
    assert_json_snapshot!(
        "relation_submit_req",
        RelationSubmitReq {
            name: "ptr".into(),
            kind: "sibling".into(),
            from: "samus_aran".into(),
            to: "character:samus".into(),
            op: "add".into(),
        }
    );
    assert_json_snapshot!("relation_pull_req", RelationPullReq { name: "ptr".into() });
    assert_json_snapshot!(
        "relation_pull_summary",
        RelationPullSummary {
            siblings: 3,
            parents: 2
        }
    );
    assert_json_snapshot!(
        "relation_edge_dto",
        RelationEdgeDto {
            kind: "sibling".into(),
            from: "samus".into(),
            to: "character:samus".into(),
            service: "ptr".into(),
            author: Some("aa".repeat(32)),
        }
    );
    assert_json_snapshot!(
        "relation_status_dto",
        RelationStatusDto {
            service: "local".into(),
            siblings: 3,
            parents: 1,
            last_pull: Some(1234)
        }
    );
    assert_json_snapshot!(
        "tag_suggestion_dto",
        TagSuggestionDto {
            namespace: "character".into(),
            subtag: "samus".into(),
            count: 12,
            alias_source: None,
        }
    );
    assert_json_snapshot!(
        "namespace_suggestion_dto",
        NamespaceSuggestionDto {
            namespace: "character".into(),
            tag_count: 40
        }
    );
    assert_json_snapshot!(
        "complete_response",
        CompleteResponse {
            namespaces: vec![NamespaceSuggestionDto {
                namespace: "character".into(),
                tag_count: 40
            }],
            tags: vec![TagSuggestionDto {
                namespace: "character".into(),
                subtag: "samus".into(),
                count: 12,
                alias_source: None,
            }],
        }
    );
}

#[test]
fn tag_suggestion_alias_source_is_additive_and_skipped_when_none() {
    let with = naiad_api::TagSuggestionDto {
        namespace: "character".into(),
        subtag: "samus".into(),
        count: 3,
        alias_source: Some("samus_aran".into()),
    };
    let without = naiad_api::TagSuggestionDto {
        namespace: "character".into(),
        subtag: "samus".into(),
        count: 3,
        alias_source: None,
    };
    let with_json = serde_json::to_string(&with).unwrap();
    let without_json = serde_json::to_string(&without).unwrap();
    assert!(
        with_json.contains("\"alias_source\":\"samus_aran\""),
        "Some must be serialised: {with_json}"
    );
    assert!(
        !without_json.contains("alias_source"),
        "None must be absent from the wire: {without_json}"
    );
    // Old JSON (pre-#116, no alias_source field) must still deserialise to None.
    let old: naiad_api::TagSuggestionDto = serde_json::from_str(&without_json).unwrap();
    assert_eq!(old.alias_source, None);
}

#[test]
fn repo_and_submit_dtos() {
    assert_json_snapshot!(
        "repo_dto",
        RepoDto {
            name: "ptr".into(),
            url: "http://127.0.0.1:9090".into(),
            max_query_bits: None,
            min_query_bits: None,
            advertised_bits: None,
            count: None,
        }
    );
    // Both bounds populated (#179): ceiling and floor known.
    assert_json_snapshot!(
        "repo_dto_with_bounds",
        RepoDto {
            name: "ptr".into(),
            url: "http://127.0.0.1:9090".into(),
            max_query_bits: Some(24),
            min_query_bits: Some(16),
            advertised_bits: None,
            count: None,
        }
    );
    assert_json_snapshot!("repo_pull_req", RepoPullReq { name: "ptr".into() });
    assert_json_snapshot!(
        "repo_pull_summary",
        RepoPullSummary {
            matched_files: 3,
            mappings: 7,
            notice: None,
        }
    );
    // With advisory notice (#179).
    assert_json_snapshot!(
        "repo_pull_summary_with_notice",
        RepoPullSummary {
            matched_files: 3,
            mappings: 7,
            notice: Some("repo ptr: privacy ceiling below floor; querying at 16 bits".into()),
        }
    );
    // Per-file pull (#144): `missing_sha256` is part of the wire contract — the
    // caller has no other way to distinguish "upstream has no tags" from "we
    // never asked". Pinned so it cannot quietly disappear again.
    assert_json_snapshot!(
        "file_pull_repo_result",
        FilePullRepoResult {
            repo: "ptr".into(),
            mappings_added: 7,
            missing_sha256: 2,
            error: None,
            notice: None,
        }
    );
    // With advisory notice (#179).
    assert_json_snapshot!(
        "file_pull_repo_result_with_notice",
        FilePullRepoResult {
            repo: "ptr".into(),
            mappings_added: 7,
            missing_sha256: 0,
            error: None,
            notice: Some("repo ptr: privacy ceiling below floor; querying at 16 bits".into()),
        }
    );
    assert_json_snapshot!(
        "pull_summary",
        PullSummary {
            results: vec![PullRepoOutcome {
                repo: "ptr".into(),
                matched_files: 3,
                mappings: 7,
                missing_sha256: 2,
                error: None,
                notice: None,
            }],
            matched_files: 3,
            mappings: 7,
        }
    );
    // Streamed summary row with the #179 floor-clamp advisory (#192): the
    // notice now rides the streamed path, not just the non-streamed one.
    assert_json_snapshot!(
        "pull_summary_with_notice",
        PullSummary {
            results: vec![PullRepoOutcome {
                repo: "ptr".into(),
                matched_files: 3,
                mappings: 7,
                missing_sha256: 0,
                error: None,
                notice: Some("repo ptr: privacy ceiling below floor; querying at 16 bits".into()),
            }],
            matched_files: 3,
            mappings: 7,
        }
    );
    assert_json_snapshot!(
        "pull_stage_chunk",
        PullStage {
            repo: "ptr".into(),
            index: 1,
            total: 2,
            phase: "chunk".into(),
            chunk: 2,
            chunk_total: 3,
            bytes: 1_234_567,
            domain: Some("sha256".into()),
            hashes: 0,
            tags: 0,
            elapsed_ms: 0,
            window: 0,
            retries: 0,
        }
    );
    assert_json_snapshot!(
        "pull_stage_merging",
        PullStage {
            repo: "ptr".into(),
            index: 1,
            total: 2,
            phase: "merging".into(),
            chunk: 0,
            chunk_total: 0,
            bytes: 1_234_567,
            domain: None,
            hashes: 0,
            tags: 0,
            elapsed_ms: 0,
            window: 0,
            retries: 0,
        }
    );
    assert_json_snapshot!(
        "repo_priority_req",
        RepoPriorityReq {
            name: "ptr".into(),
            priority: 2
        }
    );
    assert_json_snapshot!(
        "submit_req",
        SubmitReq {
            name: "ptr".into(),
            file: "a".repeat(64),
            tag: "character:samus".into(),
            op: "add".into(),
        }
    );
    assert_json_snapshot!(
        "account_dto",
        AccountDto {
            public_key: Some("ab".repeat(32)),
            key_path: "/lib/naiad.key".into()
        }
    );
}

#[test]
fn tag_detail_and_block_dtos() {
    // Trust DTOs (TrustRuleDto, AutoTrustDto, TrustSetReq, TrustFloorReq,
    // TrustFloorDto) were removed in the v6 pivot (migration 0030).
    // TagAuthorDto and the authors field were removed post-pivot (v0.2.0).
    assert_json_snapshot!(
        "tag_detail_dto",
        TagDetailDto {
            tag: "character:samus".into(),
            presence: "pulled".into(),
            services: vec!["ptr".into()],
            relations: true,
            origin: None,
        }
    );
    // With a Some origin: the field appears in the wire JSON (not skipped).
    assert_json_snapshot!(
        "tag_detail_dto_with_origin",
        TagDetailDto {
            tag: "creator:botpic".into(),
            presence: "pulled".into(),
            services: vec!["ptr".into()],
            relations: false,
            origin: Some("wd14-tagger".into()),
        }
    );
    assert_json_snapshot!(
        "relation_tag_dto",
        RelationTagDto {
            tag: "samus_aran".into(),
            count: 7
        }
    );
    assert_json_snapshot!(
        "relation_section_dto",
        RelationSectionDto {
            items: vec![RelationTagDto {
                tag: "samus_aran".into(),
                count: 7
            }],
            total: 3
        }
    );
    assert_json_snapshot!(
        "tag_relations_dto",
        TagRelationsDto {
            canonical: "character:samus".into(),
            count: 51,
            via_alias: true,
            aliases: RelationSectionDto {
                items: vec![RelationTagDto {
                    tag: "samus_aran".into(),
                    count: 7
                }],
                total: 3
            },
            parents: RelationSectionDto {
                items: vec![RelationTagDto {
                    tag: "series:metroid".into(),
                    count: 40
                }],
                total: 1
            },
            children: RelationSectionDto {
                items: vec![],
                total: 0
            },
        }
    );
    assert_json_snapshot!(
        "block_rule_dto",
        BlockRuleDto {
            id: 3,
            kind: "tag_pattern".into(),
            target: "meme:*".into(),
            note: Some("noise".into()),
            created_at: 1234,
        }
    );
    assert_json_snapshot!(
        "block_add_req",
        BlockAddReq {
            kind: "author".into(),
            target: "ab".repeat(32),
            note: None
        }
    );
}

#[test]
fn plugin_and_import_dtos() {
    assert_json_snapshot!(
        "plugin_dto",
        PluginDto {
            id: "hydrus".into(),
            name: "Hydrus importer".into(),
            tagger: true,
            processor: false,
            source: true
        }
    );
    assert_json_snapshot!(
        "hydrus_config_req",
        HydrusConfigReq {
            dir: "/hydrus/db".into(),
            tag_services: vec![1, 2]
        }
    );
    assert_json_snapshot!(
        "hydrus_config_dto",
        HydrusConfigDto {
            dir: Some("/hydrus/db".into()),
            tag_services: vec![]
        }
    );
    assert_json_snapshot!(
        "tagger_lookup_req",
        TaggerLookupReq {
            plugin_id: "hydrus".into(),
            files: vec!["a".repeat(64)],
            apply: false
        }
    );
    assert_json_snapshot!(
        "tagger_lookup_item",
        TaggerLookupItem {
            file: "a".repeat(64),
            tags: vec!["character:samus".into()]
        }
    );
    // `library_only` has serde(default) — pin both shapes.
    assert_json_snapshot!(
        "source_import_req_full",
        SourceImportReq {
            plugin_id: "hydrus".into(),
            library_only: false
        }
    );
    assert_json_snapshot!(
        "source_import_req_library_only",
        SourceImportReq {
            plugin_id: "hydrus".into(),
            library_only: true
        }
    );
    assert_json_snapshot!(
        "import_progress",
        ImportProgress {
            files: 10,
            total: 100,
            mappings: 55
        }
    );
    assert_json_snapshot!(
        "source_import_summary",
        SourceImportSummary {
            mappings_staged: 3,
            mappings_resolved: 2,
            siblings: 1,
            parents: 0,
            sha256_backfilled: 5,
        }
    );
    assert_json_snapshot!(
        "relations_import_summary",
        RelationsImportSummary {
            siblings: 2,
            parents: 1
        }
    );
    assert_json_snapshot!(
        "relations_progress",
        RelationsProgress {
            edges_done: 4096,
            edges_total: 614_000,
            siblings: 4000,
            parents: 96
        }
    );
    assert_json_snapshot!(
        "gallery_sort_dto",
        GallerySortDto {
            key: "imported_at".into(),
            direction: "desc".into()
        }
    );
}

#[test]
fn pull_stage_enriched_fields() {
    // 1. Enriched chunk-phase: all four new fields populated (#174).
    assert_json_snapshot!(
        "pull_stage_chunk_enriched",
        PullStage {
            repo: "ptr".into(),
            index: 1,
            total: 2,
            phase: "chunk".into(),
            chunk: 3,
            chunk_total: 10,
            bytes: 4_567_890,
            domain: Some("blake3".into()),
            hashes: 50_000,
            tags: 1_200_000,
            elapsed_ms: 3_456,
            window: 8,
            retries: 0,
        }
    );
    // 2. Merging/done phase: new fields all zero (pinning the default shape).
    assert_json_snapshot!(
        "pull_stage_done_zeros",
        PullStage {
            repo: "ptr".into(),
            index: 2,
            total: 2,
            phase: "done".into(),
            chunk: 0,
            chunk_total: 0,
            bytes: 5_000_000,
            domain: None,
            hashes: 0,
            tags: 0,
            elapsed_ms: 0,
            window: 0,
            retries: 0,
        }
    );
    // 3. Old-frame compat: a hand-written #172-shape JSON (new fields absent)
    //    must deserialise with the new fields defaulting to zero.
    let old_frame = r#"{
        "repo": "ptr",
        "index": 1,
        "total": 2,
        "phase": "chunk",
        "chunk": 2,
        "chunk_total": 3,
        "bytes": 1234567,
        "domain": "sha256"
    }"#;
    let stage: naiad_api::PullStage = serde_json::from_str(old_frame).unwrap();
    assert_eq!(stage.hashes, 0, "hashes must default to 0 for old frames");
    assert_eq!(stage.tags, 0, "tags must default to 0 for old frames");
    assert_eq!(
        stage.elapsed_ms, 0,
        "elapsed_ms must default to 0 for old frames"
    );
    assert_eq!(stage.window, 0, "window must default to 0 for old frames");
}
