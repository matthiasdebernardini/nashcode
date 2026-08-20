//! Dialling the public ingester over iroh.
//!
//! One transport, one job: get an HTTP request to the far side of an `iroh-ingress`
//! and bring the answer back. It knows nothing about drains, acks, or registries —
//! [`super::drain`] holds all of that, and this file exists so that it can go on
//! knowing nothing about QUIC.
//!
//! The path is: dial the ingester's EndpointId on ALPN `celld/http/0`, open one
//! bidirectional stream, and speak HTTP/1 over it with hyper. `iroh-ingress` rejects
//! any EndpointId not on its allow-file before it reads a byte and then forwards the
//! stream to celld's loopback listener, so what comes out the far end is an ordinary
//! TCP connection to an ordinary HTTP server. One request per stream: the ingress
//! gives each stream its own TCP connection, so a kept-alive session buys nothing.
//!
//! **This has never talked to a live `iroh-ingress`.** There is not one on the machine
//! it was written on, and nothing here fakes one — a test against a stub would prove
//! only that the stub agreed with itself. The TCP transport is the proven path and the
//! tests use it. What this needs before it can be believed is in `NOTES.md` and in
//! `ingester/README.md`: nashcode's own EndpointId, printed at startup and derived
//! from the persistent key at `NASHCODE_BUGS_DRAIN_KEY`, has to be in the ingester's
//! allow-file before the first dial.

use std::path::Path;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointId, SecretKey};

use crate::bugs::drain::{Answer, BoxFuture, REQUEST_TIMEOUT, Transport, answer_from};
use crate::config::Drain as DrainConfig;

/// celld's HTTP-over-iroh protocol identifier. The ingress and the node agree on it;
/// nothing else here is celld-shaped, and this is the one string that has to be.
pub const ALPN: &[u8] = b"celld/http/0";

/// The `Host` header. The far side routes on the path, not on the name, but HTTP/1.1
/// requires the header to exist and hyper will not invent one.
const HOST: &str = "ingest.invalid";

pub struct IrohTransport {
    endpoint: Endpoint,
    remote: EndpointId,
    token: String,
    /// The live QUIC session, kept between requests.
    ///
    /// The per-request unit is a *stream*, not a connection: `iroh-ingress` gives each
    /// stream its own TCP connection to celld, so one stream per request is what makes
    /// the far side behave. A connection per request is a different thing entirely — a
    /// full QUIC handshake, up to sixty-four of them per project per cycle — and it buys
    /// nothing at all.
    connection: tokio::sync::Mutex<Option<Connection>>,
}

/// Hand-written for the same reason [`super::drain::HttpTransport`]'s is: the struct
/// holds the drain token, `Transport` requires `Debug`, and a derived one would put the
/// secret in the first `?transport` anybody writes.
impl std::fmt::Debug for IrohTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohTransport")
            .field("remote", &self.remote)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Bind an endpoint with the persistent key and point it at the ingester.
pub async fn transport(config: &DrainConfig) -> Result<Arc<dyn Transport>, String> {
    let remote: EndpointId = config.target.parse().map_err(|error| {
        format!(
            "NASHCODE_BUGS_DRAIN is neither an http:// URL nor an iroh EndpointId: {error}"
        )
    })?;
    let secret = key_at(&config.key_path)?;
    let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret)
        .bind()
        .await
        .map_err(|error| format!("cannot bind an iroh endpoint: {error}"))?;
    tracing::info!(
        endpoint_id = %endpoint.id(),
        "bugs: this is the EndpointId the ingester's allow-file has to name"
    );
    Ok(Arc::new(IrohTransport {
        endpoint,
        remote,
        token: config.token.clone(),
        connection: tokio::sync::Mutex::new(None),
    }))
}

impl Transport for IrohTransport {
    fn send<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        body: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Result<Answer, String>> {
        Box::pin(async move {
            match tokio::time::timeout(REQUEST_TIMEOUT, self.round_trip(method, path, body)).await
            {
                Ok(answer) => answer,
                Err(_) => Err(format!(
                    "no answer from {} in {} seconds",
                    self.remote,
                    REQUEST_TIMEOUT.as_secs()
                )),
            }
        })
    }

    fn describe(&self) -> String {
        format!("iroh:{}", self.remote)
    }
}

impl IrohTransport {
    async fn round_trip(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Answer, String> {
        let connection = self.connection().await?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|error| format!("cannot open a stream to {}: {error}", self.remote))?;

        // A QUIC stream is a read half and a write half; hyper wants one thing that is
        // both. `join` is that thing, and `TokioIo` is the adapter between tokio's IO
        // traits and hyper's.
        let io = hyper_util::rt::TokioIo::new(tokio::io::join(recv, send));
        let (mut sender, driver) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|error| format!("HTTP/1 handshake failed: {error}"))?;
        // The driver owns the socket for as long as the exchange lasts. `Pump` aborts
        // it on the way out however this function leaves — including the `?` paths,
        // which used to leak one task per failed request.
        let pump = Pump(tokio::spawn(async move {
            let _ = driver.await;
        }));

        let request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, HOST)
            .header(hyper::header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|error| format!("cannot build the request: {error}"))?;

        let response = sender
            .send_request(request)
            .await
            .map_err(|error| format!("no answer from {}: {error}", self.remote))?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_owned)
        };
        let last_seq = header("x-ingest-last-seq");
        let remaining = header("x-ingest-remaining");
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("cannot read the answer: {error}"))?
            .to_bytes()
            .to_vec();

        drop(pump);
        Ok(answer_from(status, last_seq, remaining, bytes))
    }

    /// The cached connection, redialled when it has gone.
    ///
    /// `close_reason` is the whole test: a QUIC session that has been closed, timed out,
    /// or reset reports one, and a session that is merely idle does not.
    async fn connection(&self) -> Result<Connection, String> {
        let mut held = self.connection.lock().await;
        if let Some(live) = held.as_ref()
            && live.close_reason().is_none()
        {
            return Ok(live.clone());
        }
        let fresh = self
            .endpoint
            .connect(self.remote, ALPN)
            .await
            .map_err(|error| format!("cannot dial {}: {error}", self.remote))?;
        *held = Some(fresh.clone());
        Ok(fresh)
    }
}

/// A spawned connection driver that is aborted whichever way its scope is left.
struct Pump(tokio::task::JoinHandle<()>);

impl Drop for Pump {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The persistent secret key, made once and then read for ever.
///
/// It has to persist: the EndpointId derived from it is what the ingester allowlists,
/// and a key regenerated on every restart would lock the drainer out on the first
/// restart. Stored as 64 hex characters, mode 0600.
///
/// **Only a file that is not there gets a new key.** The first version of this took any
/// read failure as "no key yet", so a mode change, a bad mount, or an `EIO` minted a new
/// identity and wrote it over the old one — a new EndpointId, permanently outside the
/// ingress allow-file, and the old key gone. Every other error is a configuration error
/// and says so. The write is `create_new`, so even a race that gets past the read cannot
/// clobber a key, and it is created 0600 rather than created-then-chmodded, which closes
/// the window where the umask decided.
fn key_at(path: &Path) -> Result<SecretKey, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Err(format!(
                    "{} is empty. Delete it if you mean to mint a new identity — and \
                     know that the new EndpointId has to go in the ingester's allow-file",
                    path.display()
                ));
            }
            let bytes = decode_hex(raw).ok_or_else(|| {
                format!("{} is not 64 hex characters of iroh secret key", path.display())
            })?;
            warn_if_readable(path);
            return Ok(SecretKey::from_bytes(&bytes));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot read {}: {error}. Refusing to mint a new key over it: the \
                 EndpointId is what the ingester allowlists, and a new one locks this \
                 drainer out",
                path.display()
            ));
        }
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    let key = SecretKey::generate();
    let hex: String = key.to_bytes().iter().map(|byte| format!("{byte:02x}")).collect();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    std::io::Write::write_all(&mut file, format!("{hex}\n").as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    tracing::info!(
        path = %path.display(),
        "bugs: minted a new iroh secret key; the ingester's allow-file needs the \
         EndpointId logged next to this line"
    );
    Ok(key)
}

/// Say so if the key file is readable by anyone else. Not fixed silently: a mode that
/// is not what this wrote means something else has been at the file, and quietly
/// changing it back would hide that.
fn warn_if_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{mode:o}"),
                    "bugs: the iroh secret key is not 0600"
                );
            }
        }
    }
}

fn decode_hex(raw: &str) -> Option<[u8; 32]> {
    if raw.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(raw.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_file_is_made_once_and_then_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys").join("bugs-drain.key");

        let first = key_at(&path).expect("mints one");
        let second = key_at(&path).expect("reads it back");
        assert_eq!(
            first.to_bytes(),
            second.to_bytes(),
            "a key that changed on restart would lock the drainer out of the allow-file"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_key_file_full_of_nonsense_is_a_configuration_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bugs-drain.key");
        std::fs::write(&path, "this is not a key\n").unwrap();
        assert!(key_at(&path).is_err(), "silently minting a new one would change the EndpointId");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is not a key\n",
            "and the file it could not read is still there"
        );
    }

    /// The bug this replaces: any read error at all — a mode change, a bad mount —
    /// minted a new key and wrote it over the old one. A new EndpointId is permanent
    /// exile from the ingress allow-file.
    #[cfg(unix)]
    #[test]
    fn a_key_file_that_cannot_be_read_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bugs-drain.key");
        let good = key_at(&path).expect("mints one");
        let before = std::fs::read_to_string(&path).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = key_at(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Running as root, an unreadable file is still readable; then the only thing
        // worth asserting is that the key did not change, which it must not either way.
        if refused.is_err() {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
        assert_eq!(key_at(&path).unwrap().to_bytes(), good.to_bytes());
    }

    #[test]
    fn a_key_that_is_not_sixty_four_hex_characters_decodes_to_nothing() {
        assert!(decode_hex("nope").is_none());
        assert!(decode_hex(&"z".repeat(64)).is_none(), "the right length, the wrong alphabet");
        assert!(decode_hex(&"0".repeat(63)).is_none());
        assert_eq!(decode_hex(&"0".repeat(64)), Some([0u8; 32]));
    }
}
