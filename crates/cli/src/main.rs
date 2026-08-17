//! `naiad` — the command-line front end. A pure client of the daemon's local
//! API: every data command is an HTTP call. The `daemon` subcommand boots the
//! server in-process (the only command that opens the library directly).

mod client;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use naiad_bootstrap::resolve_db_path_from_process;

use crate::client::Client;

/// Worked examples appended to `naiad --help` (and to the bare-invocation help).
/// Every data command talks to a running daemon on `--addr`, so `naiad daemon`
/// comes first in any session.
const ROOT_EXAMPLES: &str = r#"Examples:
  naiad daemon                              serve the library and web UI on --addr
  naiad scan D:\pictures                    index a folder, then watch it live
  naiad list                                hash, size and path of every file
  naiad tag add photo.jpg character:samus series:metroid
  naiad tag list photo.jpg                  the computed (expanded) tag set
  naiad search character:samus -meta:wip    predicates are AND'd; `-` negates
  naiad search samus or ridley              `or` joins the tags around it
  naiad search 'system:size>2mb'            filter on intrinsic metadata
  naiad repo add http://127.0.0.1:9090      subscribe (the repo names itself)
  naiad repo pull                           pull tags from every subscription
  naiad backup                              snapshot the db to <db_dir>/backups/

Environment:
  NAIAD_DB     library path for `daemon` when --db is omitted
  RUST_LOG     client-side log filter (default: warn)

Every command except `daemon` and `export-mappings` is an HTTP client of a
running daemon. Each has its own examples: `naiad help <command>`."#;

/// A fast, local-first media organizer.
#[derive(Parser)]
#[command(name = "naiad", version, about, after_help = ROOT_EXAMPLES)]
struct Cli {
    /// Address of the daemon to talk to (and to bind when running `daemon`).
    #[arg(long, global = true, default_value = "127.0.0.1:8080")]
    addr: SocketAddr,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: own the library at `--db` and serve the API + gallery.
    ///
    /// The database location is resolved using the first-set-wins ladder:
    /// `--db` flag → `NAIAD_DB` environment variable → `<exe dir>/naiad.db`.
    /// `NAIAD_DB` is now honored (previously the CLI ignored it).
    #[command(after_help = r#"Examples:
  naiad daemon                            <exe dir>/naiad.db on 127.0.0.1:8080
  naiad daemon --db D:\naiad\naiad.db     an explicit library path
  naiad daemon --addr 0.0.0.0:8080        reachable from other machines
  naiad daemon --ui-dir ui/dist           serve a web UI built on disk
  naiad daemon --thumb-size 256           smaller cached thumbnails
  naiad daemon --no-watch                 do not reindex on file changes

The chosen path and any overridden tier are printed to stderr at startup."#)]
    Daemon {
        /// Path to the library database (created if absent).
        /// When omitted, falls through to `NAIAD_DB`, then `<exe dir>/naiad.db`.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Square thumbnail edge length in pixels (cached per size in thumbs.db).
        #[arg(long, default_value_t = 360)]
        thumb_size: u32,
        /// Serve a built web UI from this directory (e.g. `ui/dist`) instead of
        /// the embedded gallery.
        #[arg(long)]
        ui_dir: Option<PathBuf>,
        /// Do not watch scanned folders for changes (disable live reindexing).
        #[arg(long)]
        no_watch: bool,
    },
    /// Recursively index a folder: hash each file and add it to the library.
    #[command(after_help = r#"Examples:
  naiad scan D:\pictures            index the folder and start watching it
  naiad scan .                      index the current directory
  naiad roots list                  see every folder the daemon watches

Re-scanning is idempotent and cheap: files already indexed are recognised by
hash, and files that have since vanished are marked missing (never deleted —
a later scan that finds them again brings them back)."#)]
    Scan {
        /// Folder to scan.
        folder: PathBuf,
    },
    /// List indexed files.
    #[command(after_help = r#"Example:
  naiad list                        every indexed file, then a total

One file per line as `<64-char hash>  <size>  <path>`, so the hash column can
be fed straight to `naiad tag list`. To filter, use `naiad search`."#)]
    List,
    /// Add, remove, or list a file's tags (file given by path or 64-char hash).
    #[command(after_help = r#"Examples:
  naiad tag add photo.jpg character:samus series:metroid
  naiad tag remove photo.jpg series:metroid
  naiad tag list photo.jpg                 computed set: siblings and parents applied
  naiad tag list photo.jpg --raw           only what is literally stored
  naiad tag list <64-char-hash> --local-only    ignore tags pulled from repos

  naiad tag sibling add samus character:samus   alias a loose tag to its ideal
  naiad tag sibling list                        every alias, as `bad -> ideal`
  naiad tag parent add character:samus series:metroid   samus implies metroid
  naiad tag parent list                         every implication, `child -> parent`

Siblings and parents set here live on your local service. To publish them to a
repository instead, use `naiad relation submit`."#)]
    Tag {
        #[command(subcommand)]
        action: TagAction,
    },
    /// Search for files by tag predicates (AND'd; `-tag` negates; `a or b` groups).
    #[command(after_help = r#"Syntax:
  a b        both must match (predicates are AND'd)
  -a         must not match
  a or b     either one; `or` is a bareword, not a flag
  =a         literal — do not expand siblings or parents for this term
  a*         wildcard on the subtag: `sam*`, `*samus`, `sam*us`, `ns:*`
  system:…   filter on intrinsic metadata rather than tags

Examples:
  naiad search character:samus                     one tag
  naiad search character:samus series:metroid      both
  naiad search character:samus -meta:wip           the first, not the second
  naiad search samus or ridley                     either
  naiad search 'character:*' '-rating:*'           any character tag, no rating
  naiad search 'character:sam*'                    subtag wildcard
  naiad search =character:samus_aran               skip alias expansion
  naiad search -=meta:wip                          exclude only the literal tag
  naiad search 'character:"zero mission"'          a tag containing spaces
  naiad search 'system:size>2mb'                   bigger than 2 MiB
  naiad search 'system:width>=1920' 'system:height>=1080'
  naiad search 'system:duration>30s'               longer than 30 seconds
  naiad search system:type=image/png               exact filetype
  naiad search 'system:origin=wd14-tagger'         tags made by a given tool
  naiad search system:origin=manual                only hand/unattested tags
  naiad search character:samus --local-only        ignore repository tags
  naiad search character:samus --raw               no relation expansion at all

Quote anything containing `*`, `>`, `<` or `"` so the shell does not eat it.
`system:` and wildcard terms are standalone — they cannot join an `or` group."#)]
    Search {
        /// Query tokens, e.g. `character:samus series:metroid -meta:wip`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        tokens: Vec<String>,
        /// Restrict to your local service (exclude pulled repositories).
        #[arg(long)]
        local_only: bool,
        /// Match tags literally — disable sibling/parent relation expansion.
        #[arg(long)]
        raw: bool,
    },
    /// List or stop watching folders (roots) the daemon reindexes live.
    #[command(after_help = r#"Examples:
  naiad roots list                      every folder being watched
  naiad roots remove D:\pictures\old    stop watching that folder

Removing a root only stops the live watch — files already indexed under it stay
in the library, tags and all. Add a root back with `naiad scan <folder>`."#)]
    Roots {
        #[command(subcommand)]
        action: RootsAction,
    },
    /// Subscribe to, list, pull from, or remove sync tag repositories.
    #[command(after_help = r#"Examples:
  naiad repo add http://127.0.0.1:9090          subscribe (the repo names itself)
  naiad repo list                               name and url of each subscription
  naiad repo pull ptr                           pull one repo
  naiad repo pull                               pull every subscribed repo
  naiad repo priority ptr 500                   outrank other repos on conflicts
  naiad repo submit ptr photo.jpg character:samus         sign and send a tag
  naiad repo submit ptr photo.jpg meme:bad --remove       retract one
  naiad repo remove ptr                         unsubscribe, keep the pulled tags
  naiad repo remove ptr --purge                 unsubscribe and delete them

Pulling is idempotent — a second pull adds 0 mappings. Your local service has
priority 1000 by default and repos 0, so local edits win unless you raise one.
Submitting creates your signing key on first use; see `naiad account`."#)]
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// Sync tag relations: submit a signed sibling/parent, or bulk-pull a repo's graph.
    #[command(after_help = r#"Examples:
  naiad relation submit ptr sibling samus character:samus       alias the loose tag
  naiad relation submit ptr parent character:samus series:metroid
  naiad relation submit ptr sibling samus character:samus --remove
  naiad relation pull ptr                    bulk-pull that repo's whole graph
  naiad relation list                        every edge: KIND, FROM → TO, SERVICE, AUTHOR
  naiad relation list --kind sibling         only aliases
  naiad relation list --service ptr          only edges from one repo
  naiad relation status                      per-service counts and last pull time

For a sibling the arguments read `<alias> <ideal>`; for a parent, `<child>
<parent>`. To keep a relation to yourself, use `naiad tag sibling/parent` —
those stay on the local service and are never submitted."#)]
    Relation {
        #[command(subcommand)]
        action: RelationAction,
    },
    /// Suppress pulled tags locally: block by exact tag, glob pattern, or author key.
    #[command(after_help = r#"Examples:
  naiad block add --tag meme:bad                one exact tag
  naiad block add --pattern 'spam:*'            a whole namespace
  naiad block add --author <64-char-hex-key> --note "spammer"
  naiad block list                              rules with their ids
  naiad block remove 3                          lift a block

Exactly one of --tag, --pattern or --author per rule. Blocking only hides tags
in your own library — nothing is reported to the repository, and lifting a
block brings the tags straight back without re-pulling."#)]
    Block {
        #[command(subcommand)]
        action: BlockAction,
    },
    /// Show this client's signing account (public key + key-file path).
    #[command(after_help = r#"Example:
  naiad account       your public key, or a note that no key exists yet

The Ed25519 key is created on your first `repo submit` or `relation submit`
and lives in the file shown. Back it up: it is your identity at every
repository you contribute to, and it cannot be regenerated."#)]
    Account,
    /// Back up the library database to a consistent snapshot file.
    ///
    /// Uses SQLite `VACUUM INTO` to produce one self-contained, compacted copy
    /// while the daemon keeps running. Other writes pause for the duration.
    #[command(after_help = r#"Examples:
  naiad backup                                  timestamped, in <db_dir>/backups/
  naiad backup D:\backups\naiad-pre-upgrade.db  an explicit destination

The destination must not already exist and its parent directory must. The
result is one compacted file — copy it anywhere, or point a daemon at it with
`naiad daemon --db <that file>` to inspect the snapshot."#)]
    Backup {
        /// Destination path for the backup file. The parent directory must
        /// already exist and the file must not exist. Omit to write a
        /// timestamped file to `<db_dir>/backups/`.
        dest: Option<PathBuf>,
    },
    /// Export current hash→tag mappings for active files as JSONL.
    ///
    /// Offline and read-only: opens the client library database directly
    /// without connecting to the daemon. Each output line is a compact JSON
    /// object `{"hash":"<64-char lowercase blake3 hex>","tag":"<canonical tag>"}`.
    #[command(after_help = r#"Examples:
  naiad export-mappings --db naiad.db --out mappings.jsonl
  naiad export-mappings --db naiad.db --out -         write to stdout

Runs offline against the database file — no daemon required, nothing is
modified. The output is exactly the format `naiad-repo seed --from-file`
expects, so the pair is the supported way to bootstrap a repository:

  naiad export-mappings --db naiad.db --out mappings.jsonl
  naiad-repo --db repo.db seed --from-file mappings.jsonl"#)]
    ExportMappings {
        /// Path to the client library database (naiad.db).
        #[arg(long)]
        db: PathBuf,
        /// Output file path, or `-` to write to stdout.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum BlockAction {
    /// Add a block rule (exactly one of --tag/--pattern/--author).
    Add {
        /// Block an exact tag, e.g. `meme:bad`.
        #[arg(long, group = "selector")]
        tag: Option<String>,
        /// Block a tag glob, e.g. `meme:*`.
        #[arg(long, group = "selector")]
        pattern: Option<String>,
        /// Block a contributor by 64-char public-key hex.
        #[arg(long, group = "selector")]
        author: Option<String>,
        /// Optional human-readable reason.
        #[arg(long)]
        note: Option<String>,
    },
    /// List all block rules with their ids.
    List,
    /// Remove a block rule by id (as shown by `block list`).
    Remove {
        /// The rule id to remove.
        id: i64,
    },
}

#[derive(Subcommand)]
enum RelationAction {
    /// Sign and submit one relation edge to a repository.
    Submit {
        /// The repository to submit to.
        repo: String,
        /// Relation kind.
        #[arg(value_enum)]
        kind: RelKindArg,
        /// The `from` tag (sibling: the alias; parent: the child).
        from: String,
        /// The `to` tag (sibling: the ideal; parent: the parent).
        to: String,
        /// Retract the edge instead of adding it.
        #[arg(long)]
        remove: bool,
    },
    /// Bulk-pull a repository's whole relation graph into its shared service.
    Pull {
        /// The repository to pull relations from.
        repo: String,
    },
    /// List every stored relation edge across all services, with provenance.
    List {
        /// Only show this kind of edge.
        #[arg(long, value_enum)]
        kind: Option<RelKindArg>,
        /// Only show edges from this service (by local name).
        #[arg(long)]
        service: Option<String>,
    },
    /// Per-service edge counts and last relation-pull time.
    Status,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum RelKindArg {
    Sibling,
    Parent,
}

impl RelKindArg {
    fn as_str(self) -> &'static str {
        match self {
            RelKindArg::Sibling => "sibling",
            RelKindArg::Parent => "parent",
        }
    }
}

#[derive(Subcommand)]
enum RepoAction {
    /// Subscribe to a repository. The repository advertises its own name
    /// during the handshake; older servers fall back to the URL host.
    Add {
        /// The repository base URL, e.g. `http://127.0.0.1:9090`.
        url: String,
    },
    /// List subscribed repositories.
    List,
    /// Pull tags from one repository (or all, if no name is given).
    Pull {
        /// The repository to pull; omit to pull every subscribed repo.
        name: Option<String>,
    },
    /// Unsubscribe from a repository. Its pulled tags are KEPT (they still
    /// display, marked by source); pass --purge to delete them too.
    Remove {
        /// The repository to remove.
        name: String,
        /// Also delete every tag this repository contributed. Irreversible.
        #[arg(long)]
        purge: bool,
    },
    /// Sign and submit one tag operation for a file you own.
    Submit {
        /// The repository to submit to.
        name: String,
        /// File path or 64-char BLAKE3 hash.
        file: String,
        /// Tag, e.g. `character:samus`.
        tag: String,
        /// Retract the tag instead of adding it.
        #[arg(long)]
        remove: bool,
    },
    /// Set a repository's priority (higher wins on tag/relation conflicts).
    Priority {
        /// The repository name.
        name: String,
        /// Priority value (local defaults to 1000; repos default to 0).
        value: i64,
    },
}

#[derive(Subcommand)]
enum RootsAction {
    /// List watched roots.
    List,
    /// Stop watching a folder.
    Remove {
        /// Folder to stop watching (as shown by `roots list`).
        folder: PathBuf,
    },
}

#[derive(Subcommand)]
enum TagAction {
    /// Add one or more tags to a file.
    Add {
        /// File path or 64-char BLAKE3 hash.
        file: String,
        /// Tags to add (e.g. `character:samus`).
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Remove one or more tags from a file.
    Remove {
        /// File path or 64-char BLAKE3 hash.
        file: String,
        /// Tags to remove.
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// List a file's tags (computed effective set by default).
    List {
        /// File path or 64-char BLAKE3 hash.
        file: String,
        /// Show literal stored mappings instead of the computed set.
        #[arg(long)]
        raw: bool,
        /// Restrict to your local service (exclude pulled repositories).
        #[arg(long)]
        local_only: bool,
    },
    /// Manage tag siblings (aliases) on the local service.
    Sibling {
        #[command(subcommand)]
        action: SiblingAction,
    },
    /// Manage tag parents (implications) on the local service.
    Parent {
        #[command(subcommand)]
        action: ParentAction,
    },
}

#[derive(Subcommand)]
enum SiblingAction {
    /// Alias a bad tag to its ideal form.
    Add {
        /// The bad tag (alias).
        bad: String,
        /// The ideal tag it collapses to.
        ideal: String,
    },
    /// Remove a bad tag's alias.
    Remove {
        /// The bad tag whose alias to remove.
        bad: String,
    },
    /// List all aliases as `bad -> ideal`.
    List,
}

#[derive(Subcommand)]
enum ParentAction {
    /// Imply a parent tag from a child tag.
    Add {
        /// The child tag.
        child: String,
        /// The parent tag it implies.
        parent: String,
    },
    /// Remove a child -> parent implication.
    Remove {
        /// The child tag.
        child: String,
        /// The parent tag.
        parent: String,
    },
    /// List all implications as `child -> parent`.
    List,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // The daemon subcommand boots the server; everything else is a client call.
    if let Command::Daemon {
        db,
        thumb_size,
        ui_dir,
        no_watch,
    } = &cli.command
    {
        // Resolve the database path via the bootstrap ladder:
        // --db flag → NAIAD_DB env → <exe dir>/naiad.db.
        let flag_str: Option<&str> = db
            .as_ref()
            .map(|p| {
                p.to_str()
                    .ok_or_else(|| anyhow::anyhow!("--db path is not valid UTF-8"))
            })
            .transpose()?;
        let resolution =
            resolve_db_path_from_process(flag_str).map_err(|e| anyhow::anyhow!("{e}"))?;

        // Log the resolved path and source.
        eprintln!(
            "naiad: db path resolved via {}: {}",
            resolution.source.name(),
            resolution.path
        );
        // Warn about overridden tiers (set but not used).
        for (loser, val) in &resolution.overridden {
            eprintln!(
                "naiad: db path: {} ({}) overrides {} ({})",
                resolution.source.name(),
                resolution.path,
                loser.name(),
                val
            );
        }

        let db_path = PathBuf::from(&resolution.path);
        return naiad_daemon::run_from_path(
            &db_path,
            cli.addr,
            *thumb_size,
            ui_dir.clone(),
            !no_watch,
        )
        .context("running daemon");
    }

    // Offline subcommands that open the database directly without a daemon.
    if let Command::ExportMappings { db, out } = &cli.command {
        return run_export(db, out).context("exporting mappings");
    }

    // Client subcommands only (the daemon branch returned above). A stderr fmt
    // subscriber gated on RUST_LOG surfaces client-side library logs; default is
    // quiet ("warn"). try_init so this never panics if something already set a
    // global subscriber. Deliberately NOT installed in the Daemon branch: the
    // daemon's own init_tracing must own the global subscriber there.
    {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init();
    }

    let client = Client::new(cli.addr);
    match cli.command {
        Command::Daemon { .. } => unreachable!("handled above"),
        Command::ExportMappings { .. } => unreachable!("handled above"),
        Command::Scan { folder } => {
            let folder = folder.to_str().context("folder path is not valid UTF-8")?;
            let summary = client.scan(folder).context("scanning folder")?;
            println!(
                "indexed {} file(s){}{}",
                summary.imported,
                if summary.errors.is_empty() {
                    String::new()
                } else {
                    format!(", {} skipped", summary.errors.len())
                },
                if summary.marked_missing > 0 {
                    format!(", {} marked missing", summary.marked_missing)
                } else {
                    String::new()
                },
            );
            for e in &summary.errors {
                eprintln!("  skipped {}: {}", e.path, e.message);
            }
        }
        Command::List => {
            let files = client.list().context("listing files")?;
            for f in &files {
                println!("{}  {:>12}  {}", f.hash, f.size, f.path);
            }
            println!("{} file(s)", files.len());
        }
        Command::Tag { action } => match action {
            TagAction::Add { file, tags } => {
                client.tags_add(&file, &tags).context("adding tags")?;
            }
            TagAction::Remove { file, tags } => {
                client.tags_remove(&file, &tags).context("removing tags")?;
            }
            TagAction::List {
                file,
                raw,
                local_only,
            } => {
                for tag in client
                    .tags(&file, raw, local_only)
                    .context("listing tags")?
                {
                    println!("{tag}");
                }
            }
            TagAction::Sibling { action } => match action {
                SiblingAction::Add { bad, ideal } => {
                    client.sibling_add(&bad, &ideal).context("adding sibling")?;
                }
                SiblingAction::Remove { bad } => {
                    client.sibling_remove(&bad).context("removing sibling")?;
                }
                SiblingAction::List => {
                    for s in client.siblings().context("listing siblings")? {
                        println!("{} -> {}", s.bad, s.ideal);
                    }
                }
            },
            TagAction::Parent { action } => match action {
                ParentAction::Add { child, parent } => {
                    client
                        .parent_add(&child, &parent)
                        .context("adding parent")?;
                }
                ParentAction::Remove { child, parent } => {
                    client
                        .parent_remove(&child, &parent)
                        .context("removing parent")?;
                }
                ParentAction::List => {
                    for p in client.parents().context("listing parents")? {
                        println!("{} -> {}", p.child, p.parent);
                    }
                }
            },
        },
        Command::Search {
            tokens,
            local_only,
            raw,
        } => {
            let q = tokens.join(" ");
            let files = client.search(&q, local_only, raw).context("searching")?;
            for f in &files {
                println!("{}  {:>12}  {}", f.hash, f.size, f.path);
            }
            println!("{} file(s)", files.len());
        }
        Command::Roots { action } => match action {
            RootsAction::List => {
                for r in client.roots().context("listing roots")? {
                    println!("{r}");
                }
            }
            RootsAction::Remove { folder } => {
                let folder = folder.to_str().context("folder path is not valid UTF-8")?;
                client.root_remove(folder).context("removing root")?;
            }
        },
        Command::Repo { action } => match action {
            RepoAction::Add { url } => {
                let repo = client.repo_add(&url).context("subscribing to repo")?;
                println!("subscribed to {} as {}", repo.url, repo.name);
            }
            RepoAction::List => {
                for r in client.repos().context("listing repos")? {
                    println!("{}  {}", r.name, r.url);
                }
            }
            RepoAction::Pull { name } => {
                let names = match name {
                    Some(n) => vec![n],
                    None => client
                        .repos()
                        .context("listing repos")?
                        .into_iter()
                        .map(|r| r.name)
                        .collect(),
                };
                for n in names {
                    let s = client
                        .repo_pull(&n)
                        .with_context(|| format!("pulling {n}"))?;
                    println!(
                        "{n}: {} file(s) tagged, {} mapping(s)",
                        s.matched_files, s.mappings
                    );
                }
            }
            RepoAction::Remove { name, purge } => {
                client.repo_remove(&name, purge).context("removing repo")?;
                if purge {
                    println!("removed {name} and purged its tags");
                } else {
                    println!("removed {name} (tags kept; --purge deletes them)");
                }
            }
            RepoAction::Priority { name, value } => {
                client
                    .repo_priority(&name, value)
                    .context("setting repo priority")?;
                println!("set priority of {name} to {value}");
            }
            RepoAction::Submit {
                name,
                file,
                tag,
                remove,
            } => {
                let op = if remove { "remove" } else { "add" };
                client
                    .repo_submit(&name, &file, &tag, op)
                    .context("submitting tag")?;
                println!("submitted {op} {tag} for {file} to {name}");
            }
        },
        Command::Relation { action } => match action {
            RelationAction::Submit {
                repo,
                kind,
                from,
                to,
                remove,
            } => {
                let op = if remove { "remove" } else { "add" };
                client
                    .relation_submit(&repo, kind.as_str(), &from, &to, op)
                    .context("submitting relation")?;
                println!("submitted {op} {} {from} -> {to} to {repo}", kind.as_str());
            }
            RelationAction::Pull { repo } => {
                let s = client
                    .relation_pull(&repo)
                    .with_context(|| format!("pulling relations from {repo}"))?;
                println!("{repo}: {} sibling(s), {} parent(s)", s.siblings, s.parents);
            }
            RelationAction::List { kind, service } => {
                let mut edges = client.relations().context("listing relations")?;
                if let Some(k) = kind {
                    let ks = k.as_str();
                    edges.retain(|e| e.kind == ks);
                }
                if let Some(sv) = &service {
                    edges.retain(|e| &e.service == sv);
                }
                if edges.is_empty() {
                    println!("no relations");
                } else {
                    let fw = edges.iter().map(|e| e.from.len()).max().unwrap_or(0);
                    let tw = edges.iter().map(|e| e.to.len()).max().unwrap_or(0);
                    let sw = edges
                        .iter()
                        .map(|e| e.service.len())
                        .max()
                        .unwrap_or(0)
                        .max("SERVICE".len());
                    println!(
                        "{:<7}  {:<fw$}     {:<tw$}  {:<sw$}  AUTHOR",
                        "KIND", "FROM", "TO", "SERVICE"
                    );
                    for e in &edges {
                        let author = e
                            .author
                            .as_deref()
                            .map(short_author)
                            .unwrap_or_else(|| "(local)".to_string());
                        println!(
                            "{:<7}  {:<fw$}  →  {:<tw$}  {:<sw$}  {author}",
                            e.kind, e.from, e.to, e.service
                        );
                    }
                }
            }
            RelationAction::Status => {
                let rows = client
                    .relation_status()
                    .context("reading relation status")?;
                let nw = rows
                    .iter()
                    .map(|r| r.service.len())
                    .max()
                    .unwrap_or(0)
                    .max("total".len());
                let (mut ts, mut tp) = (0u64, 0u64);
                for r in &rows {
                    let last = r
                        .last_pull
                        .map(fmt_unix_utc)
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{:<nw$}  siblings: {:<4} parents: {:<4} last pull: {last}",
                        r.service, r.siblings, r.parents
                    );
                    ts += r.siblings;
                    tp += r.parents;
                }
                println!("{:<nw$}  siblings: {:<4} parents: {:<4}", "total", ts, tp);
            }
        },
        Command::Block { action } => match action {
            BlockAction::Add {
                tag,
                pattern,
                author,
                note,
            } => {
                let (kind, target) = match (tag, pattern, author) {
                    (Some(t), None, None) => ("tag", t),
                    (None, Some(p), None) => ("tag_pattern", p),
                    (None, None, Some(a)) => ("author", a),
                    _ => anyhow::bail!("provide exactly one of --tag, --pattern, --author"),
                };
                client
                    .block_add(kind, &target, note.as_deref())
                    .context("adding block rule")?;
                println!("blocked");
            }
            BlockAction::List => {
                let rules = client.blocks().context("listing block rules")?;
                if rules.is_empty() {
                    println!("no block rules");
                } else {
                    for r in rules {
                        println!(
                            "{}\t{}\t{}\t{}",
                            r.id,
                            r.kind,
                            r.target,
                            r.note.as_deref().unwrap_or("")
                        );
                    }
                }
            }
            BlockAction::Remove { id } => {
                client.block_remove(id).context("removing block rule")?;
                println!("removed block rule {id}");
            }
        },
        Command::Account => {
            let a = client.account().context("reading account")?;
            match a.public_key {
                Some(pk) => println!("account {pk}\nkey: {}", a.key_path),
                None => println!(
                    "no account yet (created on first submit)\nkey: {}",
                    a.key_path
                ),
            }
        }
        Command::Backup { dest } => {
            let dest_str = dest
                .as_ref()
                .map(|p| p.to_str().context("destination path is not valid UTF-8"))
                .transpose()?;
            let s = client.backup(dest_str).context("backing up database")?;
            let mb = s.bytes as f64 / 1_048_576.0;
            let secs = s.duration_ms as f64 / 1000.0;
            println!("backed up to {} ({:.1} MB in {:.1}s)", s.dest, mb, secs);
        }
    }
    Ok(())
}

/// Export current hash→tag mappings for active files as JSONL.
///
/// Opens the client library at `db_path` read-only (no daemon required),
/// runs the repo-db preflight guard, then streams every active-file current
/// local mapping to `out` (or stdout when `out == Path::new("-")`).
///
/// # Errors
/// Returns an error if the database cannot be opened, the guard fails,
/// or any I/O or serialization error occurs while writing.
fn run_export(db_path: &Path, out: &Path) -> anyhow::Result<()> {
    use std::io::{BufWriter, Write as _};

    let db = naiad_db::Db::open_readonly(db_path).context("opening database")?;
    db.assert_client_library(db_path)
        .context("preflight check")?;

    #[derive(serde::Serialize)]
    struct MappingLine<'a> {
        hash: &'a str,
        tag: &'a str,
    }

    let mut count: u64 = 0;
    let stdout_path = Path::new("-");
    let out_label = if out == stdout_path {
        "stdout".to_owned()
    } else {
        out.display().to_string()
    };

    if out == stdout_path {
        let stdout = std::io::stdout();
        let mut w = BufWriter::new(stdout.lock());
        db.for_each_active_local_mapping(|hash, tag| {
            serde_json::to_writer(&mut w, &MappingLine { hash, tag })
                .map_err(|e| naiad_db::Error::Invalid(e.to_string()))?;
            w.write_all(b"\n")
                .map_err(|e| naiad_db::Error::Invalid(e.to_string()))?;
            count += 1;
            Ok(())
        })?;
        w.flush()
            .map_err(|e| naiad_db::Error::Invalid(e.to_string()))?;
    } else {
        let file = std::fs::File::create(out).context("creating output file")?;
        let mut w = BufWriter::new(file);
        db.for_each_active_local_mapping(|hash, tag| {
            serde_json::to_writer(&mut w, &MappingLine { hash, tag })
                .map_err(|e| naiad_db::Error::Invalid(e.to_string()))?;
            w.write_all(b"\n")
                .map_err(|e| naiad_db::Error::Invalid(e.to_string()))?;
            count += 1;
            Ok(())
        })?;
        w.flush().context("flushing output")?;
    }

    eprintln!("exported {count} mapping(s) to {out_label}");
    Ok(())
}

/// First 8 chars of an author public-key hex (provenance at a glance).
fn short_author(hex: &str) -> String {
    hex.chars().take(8).collect()
}

/// Format a unix timestamp (seconds) as `YYYY-MM-DD HH:MM UTC`. Dependency-free
/// so the lean build needs no date crate for one status line.
fn fmt_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, min) = (rem / 3600, (rem % 3600) / 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02} UTC")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, fmt_unix_utc, short_author};

    #[test]
    fn fmt_unix_utc_epoch_and_known() {
        assert_eq!(fmt_unix_utc(0), "1970-01-01 00:00 UTC");
        // 1_700_000_000 == 2023-11-14T22:13:20Z
        assert_eq!(fmt_unix_utc(1_700_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn civil_from_days_boundaries() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
    }

    #[test]
    fn short_author_truncates() {
        assert_eq!(short_author(&"a".repeat(64)), "aaaaaaaa");
        assert_eq!(short_author("abc"), "abc");
    }
}
