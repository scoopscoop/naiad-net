//! End-to-end: sign + submit a relation to a spawned repo, then bulk-pull it
//! back into the repo's shared service. One test per relation kind.

use std::sync::Mutex;

use naiad_core::Tag;
use naiad_daemon::{CapsCache, pull_relations, submit_relation};
use naiad_db::Db;
use naiad_netproto::{Op, RelKind};

#[tokio::test(flavor = "multi_thread")]
async fn submit_and_pull_a_sibling_round_trips() {
    let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
    let repo = naiad_test_support::spawn_test_repo(repo_store);
    let repo_url = format!("http://{}", repo.addr);

    let db = Db::open_in_memory().unwrap();
    db.add_shared_service("ptr", &repo_url, None).unwrap();
    let svc = db.shared_service_by_name("ptr").unwrap().unwrap().id;
    let db = Mutex::new(db);

    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("naiad.key");

    let cache = CapsCache::new();
    let (stats, db) = tokio::task::spawn_blocking(move || {
        submit_relation(
            &db,
            &cache,
            &key,
            "ptr",
            RelKind::Sibling,
            "character:samus_aran",
            "character:samus",
            Op::Add,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let stats = pull_relations(&db, &cache, "ptr")?;
        anyhow::Ok((stats, db))
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(stats.siblings, 1);
    let db = db.lock().unwrap();
    let sibs = db.list_siblings(svc).unwrap();
    assert_eq!(
        sibs,
        vec![(
            Tag::parse("character:samus_aran").unwrap(),
            Tag::parse("character:samus").unwrap()
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_and_pull_a_parent_round_trips() {
    let repo_store = naiad_server::RepoStore::open_in_memory().unwrap();
    let repo = naiad_test_support::spawn_test_repo(repo_store);
    let repo_url = format!("http://{}", repo.addr);

    let db = Db::open_in_memory().unwrap();
    db.add_shared_service("ptr", &repo_url, None).unwrap();
    let svc = db.shared_service_by_name("ptr").unwrap().unwrap().id;
    let db = Mutex::new(db);

    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("naiad.key");

    let cache = CapsCache::new();
    let (stats, db) = tokio::task::spawn_blocking(move || {
        submit_relation(
            &db,
            &cache,
            &key,
            "ptr",
            RelKind::Parent,
            "character:samus",
            "series:metroid",
            Op::Add,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let stats = pull_relations(&db, &cache, "ptr")?;
        anyhow::Ok((stats, db))
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(stats.parents, 1);
    let db = db.lock().unwrap();
    let pars = db.list_parents(svc).unwrap();
    assert_eq!(
        pars,
        vec![(
            Tag::parse("character:samus").unwrap(),
            Tag::parse("series:metroid").unwrap()
        )]
    );
}
