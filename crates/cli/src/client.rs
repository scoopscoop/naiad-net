//! Blocking HTTP client for the daemon's local API. Synchronous (`ureq`), so the
//! CLI needs no async runtime. Every method maps a non-2xx response to an error
//! carrying the daemon's message, and a transport failure to a "is the daemon
//! running?" hint.

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Result, anyhow};
use naiad_api::{
    AccountDto, BackupReq, BackupSummary, BlockAddReq, BlockRuleDto, FileDto, ParentDto,
    RelationEdgeDto, RelationStatusDto, RepoAddReq, RepoDto, RepoPriorityReq, RepoPullReq,
    RepoPullSummary, ScanReq, ScanSummary, SiblingDto, SiblingRemoveReq, SubmitReq, TagsReq,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A typed client bound to one daemon address.
pub struct Client {
    base: String,
    agent: ureq::Agent,
}

// ureq::Error is 272 bytes (third-party type; cannot be shrunk). Every closure
// passed to `send` infers `Result<_, ureq::Error>`, triggering result_large_err
// on all 20+ call sites. Boxing at each closure would be noisier than a single
// block-level suppression; errors here are transient I/O paths, not hot loops.
#[allow(clippy::result_large_err)]
impl Client {
    /// Build a client targeting the daemon at `addr`.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            base: format!("http://{addr}"),
            agent: ureq::AgentBuilder::new().build(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Parse a JSON response, or map the failure to a useful error.
    fn parse<T: DeserializeOwned>(&self, out: Result<ureq::Response, ureq::Error>) -> Result<T> {
        match out {
            Ok(resp) => resp
                .into_json::<T>()
                .map_err(|e| anyhow!("decoding daemon response: {e}")),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(anyhow!("daemon error {code}: {}", body.trim()))
            }
            Err(ureq::Error::Transport(t)) => Err(anyhow!(
                "could not reach daemon at {}; is `naiad daemon` running? ({t})",
                self.base
            )),
        }
    }

    /// Discard a successful (empty-body) response, or map the failure.
    fn check(&self, out: Result<ureq::Response, ureq::Error>) -> Result<()> {
        match out {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(anyhow!("daemon error {code}: {}", body.trim()))
            }
            Err(ureq::Error::Transport(t)) => Err(anyhow!(
                "could not reach daemon at {}; is `naiad daemon` running? ({t})",
                self.base
            )),
        }
    }

    /// Issue a request via `f`, timing it and emitting one `cli`-target
    /// round-trip line (method + path + status + `elapsed_ms`). Off by default
    /// (the CLI subscriber's default filter is `warn`); surfaced with
    /// `RUST_LOG=cli=debug`.
    fn send(
        &self,
        method: &str,
        path: &str,
        f: impl FnOnce() -> Result<ureq::Response, ureq::Error>,
    ) -> Result<ureq::Response, ureq::Error> {
        let start = Instant::now();
        let out = f();
        let elapsed_ms = start.elapsed().as_millis();
        match &out {
            Ok(resp) => tracing::debug!(
                target: "cli",
                method,
                path,
                status = resp.status(),
                elapsed_ms,
                "daemon request"
            ),
            Err(ureq::Error::Status(code, _)) => tracing::debug!(
                target: "cli",
                method,
                path,
                status = code,
                elapsed_ms,
                "daemon request failed"
            ),
            Err(ureq::Error::Transport(_)) => tracing::debug!(
                target: "cli",
                method,
                path,
                elapsed_ms,
                "daemon unreachable"
            ),
        }
        out
    }

    fn post_empty(&self, path: &str, body: impl Serialize) -> Result<()> {
        self.check(self.send("POST", path, || {
            self.agent.post(&self.url(path)).send_json(body)
        }))
    }

    /// `POST /api/scan`.
    pub fn scan(&self, folder: &str) -> Result<ScanSummary> {
        self.parse(self.send("POST", naiad_api::API_SCAN, || {
            self.agent
                .post(&self.url(naiad_api::API_SCAN))
                .send_json(ScanReq {
                    folder: folder.to_string(),
                })
        }))
    }

    /// `GET /api/files`.
    pub fn list(&self) -> Result<Vec<FileDto>> {
        self.parse(self.send("GET", naiad_api::API_FILES, || {
            self.agent.get(&self.url(naiad_api::API_FILES)).call()
        }))
    }

    /// `GET /api/search?q=&local_only=&raw=`.
    pub fn search(&self, q: &str, local_only: bool, raw: bool) -> Result<Vec<FileDto>> {
        self.parse(self.send("GET", naiad_api::API_SEARCH, || {
            self.agent
                .get(&self.url(naiad_api::API_SEARCH))
                .query("q", q)
                .query("local_only", &local_only.to_string())
                .query("raw", &raw.to_string())
                .call()
        }))
    }

    /// `GET /api/tags?file=&raw=&local_only=`.
    pub fn tags(&self, file: &str, raw: bool, local_only: bool) -> Result<Vec<String>> {
        self.parse(self.send("GET", naiad_api::API_TAGS, || {
            self.agent
                .get(&self.url(naiad_api::API_TAGS))
                .query("file", file)
                .query("raw", &raw.to_string())
                .query("local_only", &local_only.to_string())
                .call()
        }))
    }

    /// `POST /api/repos/priority`.
    pub fn repo_priority(&self, name: &str, priority: i64) -> Result<()> {
        self.post_empty(
            naiad_api::API_REPOS_PRIORITY,
            RepoPriorityReq {
                name: name.to_string(),
                priority,
            },
        )
    }

    /// `POST /api/tags/add`.
    pub fn tags_add(&self, file: &str, tags: &[String]) -> Result<()> {
        self.post_empty(
            naiad_api::API_TAGS_ADD,
            TagsReq {
                file: file.to_string(),
                tags: tags.to_vec(),
            },
        )
    }

    /// `POST /api/tags/remove`.
    pub fn tags_remove(&self, file: &str, tags: &[String]) -> Result<()> {
        self.post_empty(
            naiad_api::API_TAGS_REMOVE,
            TagsReq {
                file: file.to_string(),
                tags: tags.to_vec(),
            },
        )
    }

    /// `GET /api/siblings`.
    pub fn siblings(&self) -> Result<Vec<SiblingDto>> {
        self.parse(self.send("GET", naiad_api::API_SIBLINGS, || {
            self.agent.get(&self.url(naiad_api::API_SIBLINGS)).call()
        }))
    }

    /// `POST /api/siblings/add`.
    pub fn sibling_add(&self, bad: &str, ideal: &str) -> Result<()> {
        self.post_empty(
            naiad_api::API_SIBLINGS_ADD,
            SiblingDto {
                bad: bad.to_string(),
                ideal: ideal.to_string(),
            },
        )
    }

    /// `POST /api/siblings/remove`.
    pub fn sibling_remove(&self, bad: &str) -> Result<()> {
        self.post_empty(
            naiad_api::API_SIBLINGS_REMOVE,
            SiblingRemoveReq {
                bad: bad.to_string(),
            },
        )
    }

    /// `GET /api/parents`.
    pub fn parents(&self) -> Result<Vec<ParentDto>> {
        self.parse(self.send("GET", naiad_api::API_PARENTS, || {
            self.agent.get(&self.url(naiad_api::API_PARENTS)).call()
        }))
    }

    /// `POST /api/parents/add`.
    pub fn parent_add(&self, child: &str, parent: &str) -> Result<()> {
        self.post_empty(
            naiad_api::API_PARENTS_ADD,
            ParentDto {
                child: child.to_string(),
                parent: parent.to_string(),
            },
        )
    }

    /// `POST /api/parents/remove`.
    pub fn parent_remove(&self, child: &str, parent: &str) -> Result<()> {
        self.post_empty(
            naiad_api::API_PARENTS_REMOVE,
            ParentDto {
                child: child.to_string(),
                parent: parent.to_string(),
            },
        )
    }

    /// `GET /api/roots`.
    pub fn roots(&self) -> Result<Vec<String>> {
        self.parse(self.send("GET", naiad_api::API_ROOTS, || {
            self.agent.get(&self.url(naiad_api::API_ROOTS)).call()
        }))
    }

    /// `DELETE /api/roots?path=`.
    pub fn root_remove(&self, folder: &str) -> Result<()> {
        self.check(self.send("DELETE", naiad_api::API_ROOTS, || {
            self.agent
                .delete(&self.url(naiad_api::API_ROOTS))
                .query("path", folder)
                .call()
        }))
    }

    /// `POST /api/repos`.
    pub fn repo_add(&self, url: &str) -> Result<RepoDto> {
        self.parse(self.send("POST", naiad_api::API_REPOS, || {
            self.agent
                .post(&self.url(naiad_api::API_REPOS))
                .send_json(RepoAddReq {
                    url: url.to_string(),
                    name: None,
                })
        }))
    }

    /// `GET /api/repos`.
    pub fn repos(&self) -> Result<Vec<RepoDto>> {
        self.parse(self.send("GET", naiad_api::API_REPOS, || {
            self.agent.get(&self.url(naiad_api::API_REPOS)).call()
        }))
    }

    /// `POST /api/repos/pull`.
    pub fn repo_pull(&self, name: &str) -> Result<RepoPullSummary> {
        self.parse(self.send("POST", naiad_api::API_REPOS_PULL, || {
            self.agent
                .post(&self.url(naiad_api::API_REPOS_PULL))
                .send_json(RepoPullReq {
                    name: name.to_string(),
                })
        }))
    }

    /// `DELETE /api/repos?name=[&purge=true]`.
    pub fn repo_remove(&self, name: &str, purge: bool) -> Result<()> {
        let mut req = self
            .agent
            .delete(&self.url(naiad_api::API_REPOS))
            .query("name", name);
        if purge {
            req = req.query("purge", "true");
        }
        self.check(self.send("DELETE", naiad_api::API_REPOS, || req.call()))
    }

    /// `POST /api/repos/submit`.
    pub fn repo_submit(&self, name: &str, file: &str, tag: &str, op: &str) -> Result<()> {
        self.post_empty(
            naiad_api::API_REPOS_SUBMIT,
            SubmitReq {
                name: name.to_string(),
                file: file.to_string(),
                tag: tag.to_string(),
                op: op.to_string(),
            },
        )
    }

    /// `POST /api/relations/submit`.
    pub fn relation_submit(
        &self,
        repo: &str,
        kind: &str,
        from: &str,
        to: &str,
        op: &str,
    ) -> Result<()> {
        self.post_empty(
            naiad_api::API_RELATIONS_SUBMIT,
            naiad_api::RelationSubmitReq {
                name: repo.to_string(),
                kind: kind.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                op: op.to_string(),
            },
        )
    }

    /// `POST /api/relations/pull`.
    pub fn relation_pull(&self, repo: &str) -> Result<naiad_api::RelationPullSummary> {
        self.parse(self.send("POST", naiad_api::API_RELATIONS_PULL, || {
            self.agent
                .post(&self.url(naiad_api::API_RELATIONS_PULL))
                .send_json(naiad_api::RelationPullReq {
                    name: repo.to_string(),
                })
        }))
    }

    /// `GET /api/relations`.
    pub fn relations(&self) -> Result<Vec<RelationEdgeDto>> {
        self.parse(self.send("GET", naiad_api::API_RELATIONS, || {
            self.agent.get(&self.url(naiad_api::API_RELATIONS)).call()
        }))
    }

    /// `GET /api/relations/status`.
    pub fn relation_status(&self) -> Result<Vec<RelationStatusDto>> {
        self.parse(self.send("GET", naiad_api::API_RELATIONS_STATUS, || {
            self.agent
                .get(&self.url(naiad_api::API_RELATIONS_STATUS))
                .call()
        }))
    }

    /// `GET /api/blocks`.
    pub fn blocks(&self) -> Result<Vec<BlockRuleDto>> {
        self.parse(self.send("GET", naiad_api::API_BLOCKS, || {
            self.agent.get(&self.url(naiad_api::API_BLOCKS)).call()
        }))
    }

    /// `POST /api/blocks`.
    pub fn block_add(&self, kind: &str, target: &str, note: Option<&str>) -> Result<()> {
        self.post_empty(
            naiad_api::API_BLOCKS,
            BlockAddReq {
                kind: kind.to_string(),
                target: target.to_string(),
                note: note.map(str::to_string),
            },
        )
    }

    /// `DELETE /api/blocks?id=`.
    pub fn block_remove(&self, id: i64) -> Result<()> {
        self.check(self.send("DELETE", naiad_api::API_BLOCKS, || {
            self.agent
                .delete(&self.url(naiad_api::API_BLOCKS))
                .query("id", &id.to_string())
                .call()
        }))
    }

    /// `GET /api/account`.
    pub fn account(&self) -> Result<AccountDto> {
        self.parse(self.send("GET", naiad_api::API_ACCOUNT, || {
            self.agent.get(&self.url(naiad_api::API_ACCOUNT)).call()
        }))
    }

    /// `POST /api/backup` — create a database snapshot via `VACUUM INTO`.
    ///
    /// Uses a dedicated agent with an explicit no-read-timeout configuration
    /// because `VACUUM INTO` on a large database can run for many minutes. The
    /// default ureq agent has no read timeout, but the explicit agent makes the
    /// intent clear and guards against a future ureq default change.
    pub fn backup(&self, dest: Option<&str>) -> Result<BackupSummary> {
        // Build a per-request agent with no read timeout so large backups do
        // not abort mid-vacuum. Connection timeout is kept short (10 s) so a
        // missing daemon is still reported promptly.
        let long_agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(u64::MAX / 2))
            .build();
        let body = BackupReq {
            dest: dest.map(str::to_string),
        };
        let out = self.send("POST", naiad_api::API_BACKUP, || {
            long_agent
                .post(&self.url(naiad_api::API_BACKUP))
                .send_json(body)
        });
        // Reuse `parse` logic inline (long_agent not stored on self).
        match out {
            Ok(resp) => resp
                .into_json::<BackupSummary>()
                .map_err(|e| anyhow!("decoding daemon response: {e}")),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(anyhow!("daemon error {code}: {}", body.trim()))
            }
            Err(ureq::Error::Transport(t)) => Err(anyhow!(
                "could not reach daemon at {}; is `naiad daemon` running? ({t})",
                self.base
            )),
        }
    }
}
