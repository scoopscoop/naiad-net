//! End-to-end: cross-service display + search merge over a real sync
//! round-trip. Exercises `display_tags` and `search` with `ReadScope::Merged`
//! vs `ReadScope::LocalOnly` after a pulled mapping and a pulled sibling.

use std::sync::Mutex;

use naiad_core::{FileRecord, Tag, hash_bytes};
use naiad_daemon::{
    CapsCache, display_tags, display_tags_detailed, list_relations, pull_relations, pull_repo,
    relation_status, search, submit_relation, submit_to_repo,
};
use naiad_db::{Db, Expansion, ReadScope};
use naiad_netproto::{Account, Op, RelKind};

fn toks(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn display_and_search_merge_across_sync() {
    // ── 1. Repo servers + subscribed client ─────────────────────────────────
    // The file the client owns. Defined early so we can seed repos before
    // spawning them (spawn_test_repo takes ownership of the store).
    let file_bytes: &[u8] = b"samus-test-file";
    let file_hash = hash_bytes(file_bytes);
    let file_hex = file_hash.to_hex();

    // Repo A: pre-seed two extra accounts so char:samus ends up with 3 total
    // supporters once the client's own key (added via submit_to_repo) joins.
    let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
    {
        let tag = Tag::parse("char:samus").unwrap();
        repo_store
            .apply_submission(&Account::generate().sign(Op::Add, &file_hash, &tag))
            .unwrap();
        repo_store
            .apply_submission(&Account::generate().sign(Op::Add, &file_hash, &tag))
            .unwrap();
    }
    let repo = naiad_test_support::spawn_test_repo(repo_store);
    let repo_url = format!("http://{}", repo.addr);

    // Repo B: a single independent supporter for char:samus on the same file.
    // Used later to verify that pulling from B does not discard A's supporters.
    let repo_b_store = naiad_server::RepoStore::open_in_memory().unwrap();
    {
        let tag = Tag::parse("char:samus").unwrap();
        repo_b_store
            .apply_submission(&Account::generate().sign(Op::Add, &file_hash, &tag))
            .unwrap();
    }
    let repo_b = naiad_test_support::spawn_test_repo(repo_b_store);
    let repo_b_url = format!("http://{}", repo_b.addr);

    // Client db: insert the file so the library owns it, then subscribe.
    let client_db = Db::open_in_memory().unwrap();
    client_db
        .insert_file(
            &FileRecord::new(
                file_hash,
                "/lib/samus.txt".into(),
                file_bytes.len() as u64,
                None,
            ),
            1,
        )
        .unwrap();
    client_db
        .add_shared_service("ptr", &repo_url, None)
        .unwrap();
    // Subscribe to repo B so we can pull from it later.
    client_db
        .add_shared_service("ptr-b", &repo_b_url, None)
        .unwrap();
    let client_db = Mutex::new(client_db);

    let key_dir = tempfile::tempdir().unwrap();
    let key = key_dir.path().join("naiad.key");

    // ── 2. Submit `char:samus` to the repo and pull mappings ────────────────
    let key_clone = key.clone();
    let file_hex_clone = file_hex.clone();
    let cache = CapsCache::new();
    tokio::task::spawn_blocking(move || {
        submit_to_repo(
            &client_db,
            &cache,
            &key_clone,
            "ptr",
            &file_hex_clone,
            "char:samus",
            Op::Add,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

        let stats = pull_repo(&client_db, &cache, "ptr", 256, None)?;
        assert_eq!(stats.mappings, 1, "one pulled mapping expected");

        // ── 3. display_tags: Merged has `*char:samus`; LocalOnly does not ──
        let db = client_db.lock().unwrap();
        let merged = display_tags(&db, &file_hex_clone, ReadScope::Merged)?;
        assert!(
            merged.contains(&"*char:samus".to_string()),
            "Merged should contain pulled tag with * prefix; got {merged:?}"
        );

        let local_only = display_tags(&db, &file_hex_clone, ReadScope::LocalOnly)?;
        assert!(
            !local_only.contains(&"*char:samus".to_string()),
            "LocalOnly must not contain pulled tag; got {local_only:?}"
        );
        assert!(
            !local_only.contains(&"char:samus".to_string()),
            "LocalOnly must not contain the pulled tag at all; got {local_only:?}"
        );
        drop(db);

        // ── 4. search: Merged finds the file; LocalOnly finds nothing ───────
        let db = client_db.lock().unwrap();
        let merged_results = search(
            &db,
            &toks(&["char:samus"]),
            ReadScope::Merged,
            Expansion::Expanded)?;
        assert_eq!(
            merged_results.len(),
            1,
            "Merged search should find the file; got {merged_results:?}"
        );

        let local_results = search(
            &db,
            &toks(&["char:samus"]),
            ReadScope::LocalOnly,
            Expansion::Expanded)?;
        assert!(
            local_results.is_empty(),
            "LocalOnly search must find nothing; got {local_results:?}"
        );
        drop(db);

        // ── 5. Submit + pull a sibling, then search by alias ────────────────
        submit_relation(
            &client_db,
            &cache,
            &key_clone,
            "ptr",
            RelKind::Sibling,
            "char:samus_aran",
            "char:samus",
            Op::Add,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

        let rel_stats = pull_relations(&client_db, &cache, "ptr")?;
        assert_eq!(rel_stats.siblings, 1, "one pulled sibling expected");

        // Searching by alias `char:samus_aran` should resolve to `char:samus`
        // (via the pulled sibling) and return the file under Merged.
        let db = client_db.lock().unwrap();
        let alias_merged = search(
            &db,
            &toks(&["char:samus_aran"]),
            ReadScope::Merged,
            Expansion::Expanded)?;
        assert_eq!(
            alias_merged.len(),
            1,
            "Merged alias search should find the file; got {alias_merged:?}"
        );

        let alias_local = search(
            &db,
            &toks(&["char:samus_aran"]),
            ReadScope::LocalOnly,
            Expansion::Expanded)?;
        assert!(
            alias_local.is_empty(),
            "LocalOnly alias search must find nothing (sibling is pulled, mapping is pulled); got {alias_local:?}"
        );

        // ── 6. raw mode: literal only, no sibling/parent inference ──────────
        let raw_alias = search(
            &db,
            &toks(&["char:samus_aran"]),
            ReadScope::Merged,
            Expansion::Raw)?;
        assert!(
            raw_alias.is_empty(),
            "raw search must not follow the pulled sibling; got {raw_alias:?}"
        );

        let raw_literal = search(
            &db,
            &toks(&["char:samus"]),
            ReadScope::Merged,
            Expansion::Raw)?;
        assert_eq!(
            raw_literal.len(),
            1,
            "raw search matches the literally-mapped tag; got {raw_literal:?}"
        );

        // ── 7. read-side: relation list shows provenance; status shows last pull ──
        let edges = list_relations(&db)?;
        let pulled = edges
            .iter()
            .find(|e| e.service == "ptr" && e.kind == naiad_db::EdgeKind::Sibling)
            .expect("pulled sibling should be listed");
        assert_eq!(pulled.from.to_string(), "char:samus_aran");
        assert_eq!(pulled.to.to_string(), "char:samus");
        assert!(
            pulled.author.is_some(),
            "a pulled edge carries the submitter's author; got {pulled:?}"
        );

        let status = relation_status(&db)?;
        let ptr_status = status.iter().find(|s| s.service == "ptr").unwrap();
        assert_eq!(ptr_status.siblings, 1);
        assert!(
            ptr_status.last_pull.is_some(),
            "ptr was relation-pulled, so last_pull is set; got {ptr_status:?}"
        );

        drop(db);

        // ── 8. v6: no supporter metadata — just assert the tag is visible ────
        let db = client_db.lock().unwrap();
        let samus_details = display_tags_detailed(
            &db,
            &file_hex_clone,
            ReadScope::Merged
        )?;
        assert!(
            samus_details.iter().any(|d| d.tag.to_string() == "char:samus"),
            "char:samus must be visible after pull in detail view; got {:?}",
            samus_details
        );
        drop(db);

        // ── 9. Union across two repos: tag still visible after pulling repo B ──
        pull_repo(&client_db, &cache, "ptr-b", 256, None)?;
        let db = client_db.lock().unwrap();
        let details_after_b = display_tags_detailed(
            &db,
            &file_hex_clone,
            ReadScope::Merged
        )?;
        assert!(
            details_after_b.iter().any(|d| d.tag.to_string() == "char:samus"),
            "char:samus must still be visible after pulling ptr-b; got {:?}",
            details_after_b
        );
        drop(db);

        anyhow::Ok(())
    })
    .await
    .unwrap()
    .unwrap();
}
