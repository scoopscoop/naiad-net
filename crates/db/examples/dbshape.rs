//! Report the shape of a real naiad library DB — file, tag and mapping counts,
//! per-service mapping totals, and per-domain pull state — so a benchmark
//! fixture can be sized against real data rather than a guess (#151).
//!
//! Point it at a **copy**: it opens the file directly, so running it against a
//! live daemon's database is asking for a lock fight.
//!
//! Read-only, and tolerant of older schemas: a table that does not exist yet is
//! reported as absent rather than aborting the run.

use rusqlite::Connection;

fn main() {
    let path = std::env::args().nth(1).expect("usage: dbshape <naiad.db>");
    let conn = Connection::open(&path).expect("open db");

    let scalar = |sql: &str| -> i64 {
        conn.query_row(sql, [], |r| r.get::<_, i64>(0))
            .unwrap_or(-1)
    };

    println!(
        "files                : {}",
        scalar("SELECT COUNT(*) FROM files")
    );
    println!(
        "files active         : {}",
        scalar("SELECT COUNT(*) FROM files WHERE state = 'active'")
    );
    println!(
        "files with sha256    : {}",
        scalar("SELECT COUNT(*) FROM files WHERE sha256 IS NOT NULL")
    );
    println!(
        "tags                 : {}",
        scalar("SELECT COUNT(*) FROM tags")
    );
    println!(
        "mappings             : {}",
        scalar("SELECT COUNT(*) FROM mappings")
    );
    println!(
        "mappings current     : {}",
        scalar("SELECT COUNT(*) FROM mappings WHERE status = 'current'")
    );

    println!("\n-- mappings per service --");
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.name, s.scope, s.url, COUNT(m.file_id)
               FROM services s LEFT JOIN mappings m ON m.service_id = s.id
              GROUP BY s.id ORDER BY s.id",
        )
        .expect("prepare services");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .expect("query services");
    for row in rows {
        let (id, name, scope, url, n) = row.expect("row");
        println!(
            "  svc {id:>2} {name:<20} scope={scope:<8} mappings={n:>9}  url={}",
            url.unwrap_or_else(|| "-".into())
        );
    }

    println!("\n-- distinct files carrying at least one mapping --");
    println!(
        "  {}",
        scalar("SELECT COUNT(DISTINCT file_id) FROM mappings")
    );
    println!("-- avg tags per tagged file --");
    println!(
        "  {:.2}",
        scalar("SELECT COUNT(*) FROM mappings") as f64
            / scalar("SELECT COUNT(DISTINCT file_id) FROM mappings").max(1) as f64
    );

    // Added by migration 0033. A library that predates it is a normal thing to
    // point this at, so report its absence instead of aborting the whole run
    // after the interesting numbers have already been printed.
    println!("\n-- pull state (migration 0033) --");
    match conn.prepare(
        "SELECT service_id, domain, mapping_cursor, last_pull_file_marker
           FROM service_domain_pull_state",
    ) {
        Err(_) => println!("  (no service_domain_pull_state table — pre-0033 schema)"),
        Ok(mut stmt) => {
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                })
                .expect("query pull state");
            let mut any = false;
            for row in rows {
                let (svc, domain, cursor, marker) = row.expect("row");
                println!("  svc {svc} domain={domain} cursor={cursor:?} marker={marker:?}");
                any = true;
            }
            if !any {
                println!("  (empty — no subscription has pulled yet)");
            }
        }
    }

    // Added by migration 0034 (#151): per-domain provenance for pulled rows.
    println!("\n-- mapping provenance (migration 0034) --");
    match conn.prepare("SELECT domains, COUNT(*) FROM mappings GROUP BY domains ORDER BY domains") {
        Err(_) => println!("  (no `domains` column — pre-0034 schema)"),
        Ok(mut stmt) => {
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .expect("query provenance");
            for row in rows {
                let (mask, n) = row.expect("row");
                let label = match mask {
                    1 => "blake3 / local",
                    2 => "sha256",
                    3 => "both",
                    _ => "unknown",
                };
                println!("  domains={mask} ({label:<14}) rows={n}");
            }
        }
    }
}
