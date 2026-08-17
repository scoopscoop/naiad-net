//! PTR HTTP client (issue #124). Speaks the Hydrus repository read API over the
//! PTR's self-signed TLS. Auth is a session-key cookie: GET /session_key with a
//! `Hydrus-Key` header returns a `session_key` cookie sent on later requests
//! (ClientNetworkingLogin.py / ServerServerResources.py). NETWORK_VERSION=20 →
//! User-Agent "hydrus client/20".
//!
//! Protocol references:
//! - Session: `GET {base}/session_key` with `Hydrus-Key: <access_key hex>` →
//!   `Set-Cookie: session_key=<hex>; max_age=N`. Subsequent requests attach
//!   `Cookie: session_key=<hex>`. Refresh on 401/403/419.
//! - `GET /metadata?since={update_index}` → `zlib(json(<response>))` where
//!   `<response>` is one of two shapes depending on server version:
//!   - **Plain object** (older servers / local testbeds):
//!     `{"metadata_slice": [37, 1, [rows, next_update_due]]}`.
//!   - **SerialisableDictionary envelope** (live PTR, ptr.hydrus.network:45871):
//!     `[21, version, [ [[kflag, "metadata_slice"], [vflag, [37, 1, [rows,
//!     next_update_due]]]] ]]`. Type tag 21 = SerialisableDictionary.
//!
//!   In both shapes the `metadata_slice` value is `[37, 1, [rows,
//!   next_update_due]]` (Metadata type = 37).
//! - `GET /update?update_hash={64-hex sha256}` → raw `application/octet-stream`
//!   bytes (the zlib update file); 404 if unknown.
//! - PTR base `https://ptr.hydrus.network:45871`, self-signed TLS only.
//!
//! # TLS note
//! The dep graph enables both `ring` and `aws-lc-rs` features on rustls 0.23
//! simultaneously (feature unification from multiple crates). `ClientConfig::
//! builder()` would panic in that case; we use `builder_with_provider(ring)`
//! explicitly. The standalone `rustls` dep is declared with
//! `default-features = false, features = ["ring", "tls12", "logging", "std"]`
//! to avoid compiling the unused aws-lc-rs C/C++ dependency.

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, anyhow};

const USER_AGENT: &str = "hydrus client/20";
/// SerialisableDictionary type tag for Metadata (hydrus/core/HydrusSerialisable.py).
const METADATA_TYPE: i64 = 37;

/// Safety cap for `/metadata` responses. PTR metadata is small JSON; 32 MiB is a
/// generous tripwire against a misbehaving or compromised server.
const METADATA_SIZE_CAP: u64 = 32 * 1024 * 1024;
/// Safety cap for `/update` responses. PTR update files are typically a few MiB;
/// 256 MiB is a generous tripwire.
const UPDATE_SIZE_CAP: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One entry in a metadata slice: which update file covers which time range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataEntry {
    pub update_index: u64,
    pub update_hashes: Vec<String>,
    pub begin_ts: i64,
    pub end_ts: i64,
}

/// Decoded metadata slice returned by `/metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub entries: Vec<MetadataEntry>,
    pub next_update_due: u64,
}

// ---------------------------------------------------------------------------
// Dangerous TLS verifier
// ---------------------------------------------------------------------------

/// A rustls `ServerCertVerifier` that accepts any certificate presented by
/// the server.
///
/// # Security tradeoff
/// The PTR presents a self-signed certificate with no CA chain. Hydrus uses
/// the operator-published access key (not PKI) as the trust anchor; the corpus
/// is public tags, so the risk of a MITM receiving your access key is low in
/// practice. This verifier is intentionally scoped to PTR connections only —
/// **never reuse it for naiad-to-naiad connections**, which will use proper
/// PKI or pinned certificates.
///
/// Certificate chain validation is bypassed entirely. Signature verification
/// (verify_tls12/tls13_signature) is delegated to the ring crypto provider so
/// handshake integrity is preserved.
///
/// The ring `WebPkiSupportedAlgorithms` is computed once (OnceLock) rather
/// than reconstructing `default_provider()` on every verify call.
pub(crate) mod dangerous_verifier {
    use std::sync::{Arc, OnceLock};

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::WebPkiSupportedAlgorithms;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};

    /// Cached ring algorithm list — constructed once, reused on every verify call.
    static RING_ALGS: OnceLock<WebPkiSupportedAlgorithms> = OnceLock::new();

    fn ring_algs() -> &'static WebPkiSupportedAlgorithms {
        RING_ALGS.get_or_init(|| {
            rustls::crypto::ring::default_provider().signature_verification_algorithms
        })
    }

    #[derive(Debug)]
    pub(crate) struct AcceptAnyServerCert;

    impl ServerCertVerifier for AcceptAnyServerCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            // Bypass certificate chain validation; see module-level tradeoff comment.
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, ring_algs())
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, ring_algs())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            ring_algs().supported_schemes()
        }
    }

    /// Build a `rustls::ClientConfig` that accepts any server certificate.
    /// Only intended for PTR connections — see module-level safety note.
    ///
    /// Uses `builder_with_provider(ring)` explicitly because the dep graph
    /// enables both `ring` and `aws-lc-rs` features on rustls (feature
    /// unification from multiple crates), making the auto-detect path panic.
    /// Pinning ring here is safe: ureq's `tls` feature already pulls it in.
    pub(crate) fn client_config() -> rustls::ClientConfig {
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
        .with_no_client_auth()
    }
}

// ---------------------------------------------------------------------------
// PtrClient
// ---------------------------------------------------------------------------

pub struct PtrClient {
    agent: ureq::Agent,
    base: String,
    access_key: String,
    session_key: Option<String>,
}

impl PtrClient {
    /// Create a new client.
    ///
    /// `base_url` — e.g. `"https://ptr.hydrus.network:45871"`.
    /// `access_key` — hex-encoded 64-char Hydrus access key.
    ///
    /// The ureq agent is built with a custom rustls config that bypasses
    /// certificate chain validation (see [`dangerous_verifier`]).
    pub fn new(base_url: &str, access_key: &str) -> Self {
        let tls = Arc::new(dangerous_verifier::client_config());
        let agent = ureq::AgentBuilder::new().tls_config(tls).build();
        Self {
            agent,
            base: base_url.trim_end_matches('/').to_string(),
            access_key: access_key.to_string(),
            session_key: None,
        }
    }

    /// Perform the `GET /session_key` handshake and store the resulting cookie.
    ///
    /// Iterates all `Set-Cookie` headers (ureq returns only the first via
    /// `header()`; `all()` is used so a leading unrelated cookie cannot shadow
    /// the `session_key=` one).
    pub fn session(&mut self) -> anyhow::Result<()> {
        let t0 = Instant::now();
        let resp = self
            .agent
            .get(&format!("{}/session_key", self.base))
            .set("User-Agent", USER_AGENT)
            .set("Hydrus-Key", &self.access_key)
            .call()
            .context("GET /session_key")?;
        // Iterate all Set-Cookie headers; Hydrus may send extras before session_key.
        let cookie = resp
            .all("Set-Cookie")
            .iter()
            .flat_map(|h| h.split(';').next())
            .find_map(|kv| kv.strip_prefix("session_key=").map(str::to_string))
            .ok_or_else(|| anyhow!("no session_key in Set-Cookie response"))?;
        self.session_key = Some(cookie);
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        tracing::debug!(target: "bridge", base = %self.base, elapsed_ms, "PTR session established");
        Ok(())
    }

    /// Issue an authenticated GET. Obtains a session first if none exists.
    /// On 401/403/419 performs ONE automatic session refresh and retries.
    fn authed_get(&mut self, url: &str) -> anyhow::Result<ureq::Response> {
        if self.session_key.is_none() {
            self.session()?;
        }
        let sk = self.session_key.clone().unwrap();
        let do_get = |agent: &ureq::Agent, sk: &str| -> Result<ureq::Response, Box<ureq::Error>> {
            agent
                .get(url)
                .set("User-Agent", USER_AGENT)
                .set("Cookie", &format!("session_key={sk}"))
                .call()
                .map_err(Box::new)
        };
        match do_get(&self.agent, &sk) {
            Ok(r) => Ok(r),
            Err(e) if matches!(*e, ureq::Error::Status(401 | 403 | 419, _)) => {
                // Session expired or access denied — refresh once and retry.
                tracing::debug!(target: "bridge", url, "PTR session expired or denied; refreshing once");
                self.session()?;
                let sk2 = self.session_key.clone().unwrap();
                do_get(&self.agent, &sk2)
                    .map_err(|e| anyhow::Error::from(*e))
                    .context("retry after session refresh")
            }
            Err(e) => Err(anyhow::Error::from(*e)).context("authed GET"),
        }
    }

    /// Fetch the metadata slice for update indexes >= `since`.
    ///
    /// Response is `zlib(json(<envelope>))` where the envelope is either a
    /// plain JSON object or a SerialisableDictionary (type tag 21); both shapes
    /// carry a `"metadata_slice"` value of `[37, 1, [rows, next_update_due]]`.
    /// Capped at [`METADATA_SIZE_CAP`] bytes as a tripwire against runaway responses.
    pub fn metadata(&mut self, since: u64) -> anyhow::Result<Metadata> {
        let t0 = Instant::now();
        let resp = self.authed_get(&format!("{}/metadata?since={since}", self.base))?;
        let buf = read_capped(resp.into_reader(), METADATA_SIZE_CAP, "metadata")?;
        let json = inflate(&buf)?;
        let dict: serde_json::Value =
            serde_json::from_slice(&json).context("metadata response not JSON")?;
        let slice = find_metadata_slice(&dict)?;
        let meta = parse_metadata(slice)?;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        tracing::debug!(target: "bridge", since, entries = meta.entries.len(), next_update_due = meta.next_update_due, elapsed_ms, "fetched PTR metadata");
        Ok(meta)
    }

    /// Fetch the raw bytes of an update file by its SHA-256 hex digest.
    ///
    /// Returns the raw zlib body; pass to `hydrus_wire::decode_update` to decode.
    /// Capped at [`UPDATE_SIZE_CAP`] bytes as a tripwire against runaway responses.
    pub fn fetch_update(&mut self, hash_hex: &str) -> anyhow::Result<Vec<u8>> {
        let t0 = Instant::now();
        let resp = self.authed_get(&format!("{}/update?update_hash={hash_hex}", self.base))?;
        let bytes = read_capped(resp.into_reader(), UPDATE_SIZE_CAP, "update")?;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        tracing::trace!(target: "bridge", hash = hash_hex, bytes = bytes.len(), elapsed_ms, "fetched PTR update");
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read at most `cap` bytes from `reader`, erroring if the response exceeds
/// the cap. Reads `cap + 1` bytes via `Read::take` to distinguish a full read
/// from a truncated one without a separate stat/content-length check.
fn read_capped(reader: impl Read, cap: u64, label: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(anyhow!(
            "{label} response exceeded {cap}-byte safety cap; refusing to continue"
        ));
    }
    Ok(buf)
}

/// zlib-inflate `bytes`. Mirrors `hydrus_wire::inflate`; a local copy avoids
/// making that function `pub(crate)` — DRY tradeoff noted here.
fn inflate(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| anyhow!("zlib inflate failed: {e}"))?;
    Ok(out)
}

/// Locate the `metadata_slice` value in a /metadata response.
///
/// Two shapes exist in the wild:
/// - plain JSON object: `{"metadata_slice": [37, 1, ...]}` (older servers,
///   local snapshot testbeds)
/// - Hydrus SerialisableDictionary envelope (the live PTR):
///   `[21, version, [ [[kflag, "metadata_slice"], [vflag, [37, 1, ...]]] ]]`
fn find_metadata_slice(dict: &serde_json::Value) -> anyhow::Result<&serde_json::Value> {
    if let Some(v) = dict.get("metadata_slice") {
        return Ok(v);
    }
    // Envelope: [21, version, entries]; entries = [[ [kflag, key], [vflag, value] ], ...]
    if let Some(arr) = dict.as_array() {
        if arr.first().and_then(|t| t.as_u64()) == Some(21) {
            let entries = arr
                .get(2)
                .and_then(|e| e.as_array())
                .ok_or_else(|| anyhow!("serialisable dict envelope has no entry list"))?;
            for pair in entries {
                let key = pair.get(0).and_then(|k| k.get(1)).and_then(|k| k.as_str());
                if key == Some("metadata_slice") {
                    return pair
                        .get(1)
                        .and_then(|v| v.get(1))
                        .ok_or_else(|| anyhow!("metadata_slice entry has no value"));
                }
            }
        }
    }
    Err(anyhow!("missing 'metadata_slice' key in metadata response"))
}

/// Parse `[37, 1, [rows, next_update_due]]` into [`Metadata`].
fn parse_metadata(slice: &serde_json::Value) -> anyhow::Result<Metadata> {
    let arr = slice
        .as_array()
        .ok_or_else(|| anyhow!("metadata_slice is not an array"))?;
    let ty = arr
        .first()
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("metadata_slice: missing type tag"))?;
    if ty != METADATA_TYPE {
        return Err(anyhow!(
            "metadata_slice: expected type {METADATA_TYPE}, got {ty}"
        ));
    }
    // Envelope: [type, version, info] where info = [rows, next_update_due].
    let info = arr
        .get(2)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("metadata_slice: missing info array"))?;
    let rows = info
        .first()
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("metadata_slice: info[0] (rows) is not an array"))?;
    let next_update_due = info
        .get(1)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("metadata_slice: info[1] (next_update_due) is not u64"))?;

    let mut entries = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let r = row
            .as_array()
            .ok_or_else(|| anyhow!("metadata_slice: row[{i}] is not an array"))?;
        let update_index = r
            .first()
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow!("metadata_slice: row[{i}][0] (update_index) is not u64"))?;
        let hashes_arr = r
            .get(1)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow!("metadata_slice: row[{i}][1] (hashes) is not an array"))?;
        let update_hashes = hashes_arr
            .iter()
            .enumerate()
            .map(|(j, h)| {
                h.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("metadata_slice: row[{i}] hash[{j}] is not a string"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let begin_ts = r
            .get(2)
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow!("metadata_slice: row[{i}][2] (begin_ts) is not i64"))?;
        let end_ts = r
            .get(3)
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| anyhow!("metadata_slice: row[{i}][3] (end_ts) is not i64"))?;
        entries.push(MetadataEntry {
            update_index,
            update_hashes,
            begin_ts,
            end_ts,
        });
    }
    Ok(Metadata {
        entries,
        next_update_due,
    })
}

// ---------------------------------------------------------------------------
// Test support (crate-visible for Task 9's test fixtures)
// ---------------------------------------------------------------------------

/// Compress a JSON value with zlib (level 9), as Hydrus does for update files
/// and metadata responses. Available in test builds only.
#[cfg(test)]
pub(crate) fn zlib_json(v: &serde_json::Value) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
    enc.write_all(serde_json::to_string(v).unwrap().as_bytes())
        .unwrap();
    enc.finish().unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;

    use super::*;

    // Local axum stub replaces the PTR (plain HTTP; the TLS verifier is
    // exercised in a separate unit test below).
    async fn stub() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{Router, http::header, routing::get};
        let uh = "cd".repeat(32);
        let meta = zlib_json(&serde_json::json!({
            "metadata_slice": [37, 1, [[[0, [uh], 10, 20]], 1]]
        }));
        let app = Router::new()
            .route(
                "/session_key",
                get(|| async {
                    (
                        [(header::SET_COOKIE, "session_key=deadbeef; Max-Age=600")],
                        "",
                    )
                }),
            )
            .route(
                "/metadata",
                get(move || {
                    let meta = meta.clone();
                    async move { meta }
                }),
            )
            .route("/update", get(|| async { b"UPDATE-BYTES".to_vec() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), h)
    }

    #[tokio::test]
    async fn session_metadata_and_update_roundtrip() {
        let (base, _h) = stub().await;
        tokio::task::spawn_blocking(move || {
            let mut c = PtrClient::new(
                &base,
                "4a285629721ca442541ef2c15ea17d1f7f7578b0c3f4f5f2a05f8f0ab297786f",
            );
            c.session().unwrap();
            let md = c.metadata(0).unwrap();
            assert_eq!(md.next_update_due, 1);
            assert_eq!(md.entries.len(), 1);
            assert_eq!(md.entries[0].update_index, 0);
            assert_eq!(md.entries[0].update_hashes.len(), 1);
            let bytes = c.fetch_update(&md.entries[0].update_hashes[0]).unwrap();
            assert_eq!(bytes, b"UPDATE-BYTES");
        })
        .await
        .unwrap();
    }

    /// Verify the dangerous verifier itself: it must accept a trivially invalid
    /// certificate (rustls 0.23.42 / ring provider).
    #[test]
    fn dangerous_verifier_accepts_any_cert() {
        use dangerous_verifier::AcceptAnyServerCert;
        use rustls::client::danger::ServerCertVerifier;
        let v = AcceptAnyServerCert;
        let cert = rustls::pki_types::CertificateDer::from(vec![0u8; 4]);
        let now = rustls::pki_types::UnixTime::now();
        let res = v.verify_server_cert(
            &cert,
            &[],
            &rustls::pki_types::ServerName::try_from("ptr.hydrus.network").unwrap(),
            &[],
            now,
        );
        assert!(res.is_ok(), "dangerous verifier rejected cert: {res:?}");
    }

    /// 401-refresh path: a stub that returns 401 on the first data request,
    /// then requires a second session before serving.
    ///
    /// Implementation: use an atomic counter shared across axum handlers.
    /// First /metadata call (before extra session) → 401; after a session
    /// refresh the counter advances and metadata succeeds.
    #[tokio::test]
    async fn session_refresh_on_401() {
        use axum::{Router, extract::Query, http::StatusCode, routing::get};
        use std::collections::HashMap;
        use std::sync::{
            Arc as SArc,
            atomic::{AtomicU32, Ordering},
        };

        let call_count = SArc::new(AtomicU32::new(0));
        let cc = call_count.clone();
        let uh = "cd".repeat(32);
        let meta = zlib_json(&serde_json::json!({
            "metadata_slice": [37, 1, [[[0, [uh], 10, 20]], 1]]
        }));

        let app = Router::new()
            .route(
                "/session_key",
                get(|| async {
                    (
                        [(
                            axum::http::header::SET_COOKIE,
                            "session_key=newtoken; Max-Age=600",
                        )],
                        "",
                    )
                }),
            )
            .route(
                "/metadata",
                get(move |Query(_): Query<HashMap<String, String>>| {
                    let cc = cc.clone();
                    let meta = meta.clone();
                    async move {
                        // First call returns 401; subsequent calls succeed.
                        let n = cc.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            StatusCode::UNAUTHORIZED.into_response()
                        } else {
                            (StatusCode::OK, meta).into_response()
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let base = format!("http://{addr}");
        tokio::task::spawn_blocking(move || {
            let mut c = PtrClient::new(&base, "deadbeef");
            // Manually set a session key so authed_get doesn't fetch one first.
            c.session_key = Some("stale".to_string());
            // metadata() → 401 → automatic session refresh → retry → Ok
            let md = c.metadata(0).unwrap();
            assert_eq!(md.entries.len(), 1);
        })
        .await
        .unwrap();
    }

    // ------------------------------------------------------------------
    // find_metadata_slice unit tests
    // ------------------------------------------------------------------

    #[test]
    fn find_metadata_slice_plain_dict() {
        let v = serde_json::json!({"metadata_slice": [37, 1, [[], 0]]});
        let slice = find_metadata_slice(&v).expect("plain dict must succeed");
        // Full parse path must also work.
        let md = parse_metadata(slice).expect("parse_metadata on plain slice");
        assert_eq!(md.entries.len(), 0);
        assert_eq!(md.next_update_due, 0);
    }

    #[test]
    fn find_metadata_slice_envelope_shape() {
        // Real PTR envelope: [21, version, [[[kflag, key], [vflag, value]]]]
        // Row: [update_index, [hash_hex...], begin_ts, end_ts]
        let v = serde_json::json!([
            21,
            2,
            [[
                [0, "metadata_slice"],
                [2, [37, 1, [[[0, ["aa"], 100, 200]], 12345]]]
            ]]
        ]);
        let slice = find_metadata_slice(&v).expect("envelope must succeed");
        let md = parse_metadata(slice).expect("parse_metadata on envelope slice");
        assert_eq!(md.next_update_due, 12345);
        assert_eq!(md.entries.len(), 1);
        let e = &md.entries[0];
        assert_eq!(e.update_index, 0);
        assert_eq!(e.update_hashes, vec!["aa".to_string()]);
        assert_eq!(e.begin_ts, 100);
        assert_eq!(e.end_ts, 200);
    }

    #[test]
    fn find_metadata_slice_envelope_no_key_errors() {
        // Envelope with no matching key.
        let v = serde_json::json!([21, 2, [[[0, "other_key"], [2, 42]]]]);
        let err = find_metadata_slice(&v).unwrap_err();
        assert!(
            err.to_string().contains("missing 'metadata_slice'"),
            "unexpected error: {err}"
        );
    }
}
