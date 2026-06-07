// Copyright 2026 VaultPlane Contributors
// SPDX-License-Identifier: Apache-2.0

//! Inbound TLS for the proxy listener.
//!
//! When `listen.tls` is set in the configuration, the proxy serves HTTPS via
//! [`axum_server`] using rustls. The admin listener stays plain HTTP: it is
//! intended for cluster-internal traffic, and most operators front it with
//! their own ingress.
//!
//! Cert rotation is hot: a config reload (SIGHUP or
//! `POST /admin/config/reload`) re-reads the configured cert and key paths
//! and atomically swaps the live rustls config via
//! [`RustlsConfig::reload_from_pem_file`]. In-flight connections keep the
//! cert they handshook with; new handshakes use the new cert.
//!
//! Toggling TLS on (or off) at runtime is NOT supported: adding TLS to a
//! gateway started without it would require binding a new listener, and
//! removing TLS would require closing the existing one. Those structural
//! changes are restart-only and are logged on reload when detected.

use std::sync::Arc;

use anyhow::{Context, anyhow};
use axum_server::tls_rustls::RustlsConfig;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use vaultplane_core::config::TlsConfig;

/// Install a process-level rustls crypto provider once. Both `aws-lc-rs` and
/// `ring` end up in the dependency graph (the latter via the test client's
/// rustls backend), so rustls cannot auto-select one; we pick `aws-lc-rs`
/// explicitly. Idempotent: a second call (or one after another crate installed
/// a provider) is ignored.
pub(crate) fn ensure_default_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Load the cert and key from disk and build a rustls server config. When
/// `client_ca_path` is set, the config requires mutual TLS.
pub async fn build_rustls_config(tls: &TlsConfig) -> anyhow::Result<RustlsConfig> {
    ensure_default_provider();
    match tls.client_ca_path.as_deref() {
        None => RustlsConfig::from_pem_file(&tls.cert_path, &tls.key_path)
            .await
            .with_context(|| {
                format!(
                    "failed to load TLS material (cert: {}, key: {})",
                    tls.cert_path, tls.key_path
                )
            }),
        Some(ca) => {
            let config = mtls_server_config(&tls.cert_path, &tls.key_path, ca)?;
            Ok(RustlsConfig::from_config(Arc::new(config)))
        }
    }
}

/// Re-read the TLS material from disk and swap it into the live rustls config in
/// place. New TLS handshakes use the new material; connections already
/// established keep their negotiated config. Both paths parse all files before
/// swapping, so malformed material leaves the old config active and surfaces an
/// error. Toggling mutual TLS on or off (adding or removing `client_ca_path`)
/// within an existing `tls:` block takes effect here too.
pub async fn reload_certs(rustls: &RustlsConfig, tls: &TlsConfig) -> anyhow::Result<()> {
    ensure_default_provider();
    match tls.client_ca_path.as_deref() {
        None => rustls
            .reload_from_pem_file(&tls.cert_path, &tls.key_path)
            .await
            .with_context(|| {
                format!(
                    "failed to reload TLS material (cert: {}, key: {})",
                    tls.cert_path, tls.key_path
                )
            }),
        Some(ca) => {
            let config = mtls_server_config(&tls.cert_path, &tls.key_path, ca)?;
            rustls.reload_from_config(Arc::new(config));
            Ok(())
        }
    }
}

/// Build a rustls [`ServerConfig`] that presents the server cert and requires a
/// client certificate chaining to one of the CAs in `client_ca_path`.
fn mtls_server_config(
    cert_path: &str,
    key_path: &str,
    client_ca_path: &str,
) -> anyhow::Result<ServerConfig> {
    let certs =
        load_certs(cert_path).with_context(|| format!("failed to load TLS cert {cert_path}"))?;
    let key = load_key(key_path).with_context(|| format!("failed to load TLS key {key_path}"))?;

    let mut roots = rustls::RootCertStore::empty();
    for ca in load_certs(client_ca_path)
        .with_context(|| format!("failed to load client CA bundle {client_ca_path}"))?
    {
        roots
            .add(ca)
            .context("failed to add a client CA certificate to the trust store")?;
    }
    if roots.is_empty() {
        return Err(anyhow!(
            "client CA bundle {client_ca_path} contained no certificates"
        ));
    }

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .context("failed to build the client certificate verifier")?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("failed to select TLS protocol versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("failed to build the mTLS server config")?;
    // Match the ALPN that axum-server's PEM path advertises.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// Parse a PEM file into a chain of DER certificates.
fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path)?;
    rustls_pemfile::certs(&mut &data[..])
        .collect::<Result<Vec<_>, _>>()
        .context("failed to parse PEM certificates")
}

/// Parse the first private key from a PEM file (PKCS#8, PKCS#1, or SEC1).
fn load_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path)?;
    rustls_pemfile::private_key(&mut &data[..])
        .context("failed to parse PEM private key")?
        .ok_or_else(|| anyhow!("no private key found in {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a freshly generated self-signed cert+key pair to a temp dir.
    /// Returns `(cert_path, key_path, _dir)`; the TempDir guard must stay
    /// alive for the duration of the test so the files survive.
    fn write_self_signed(dnsnames: &[&str]) -> (String, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> = dnsnames.iter().map(|s| (*s).to_string()).collect();
        let signed = rcgen::generate_simple_self_signed(names).unwrap();
        let cert_pem = signed.cert.pem();
        let key_pem = signed.key_pair.serialize_pem();

        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::File::create(&cert_path)
            .unwrap()
            .write_all(cert_pem.as_bytes())
            .unwrap();
        std::fs::File::create(&key_path)
            .unwrap()
            .write_all(key_pem.as_bytes())
            .unwrap();

        (
            cert_path.to_string_lossy().to_string(),
            key_path.to_string_lossy().to_string(),
            dir,
        )
    }

    #[tokio::test]
    async fn build_rustls_config_accepts_a_valid_pem_pair() {
        let (cert, key, _dir) = write_self_signed(&["localhost"]);
        let tls = TlsConfig {
            cert_path: cert,
            key_path: key,
            client_ca_path: None,
        };
        build_rustls_config(&tls)
            .await
            .expect("valid self-signed pair should load");
    }

    #[tokio::test]
    async fn build_rustls_config_reports_the_paths_in_the_error() {
        let tls = TlsConfig {
            cert_path: "/no/such/cert.pem".to_string(),
            key_path: "/no/such/key.pem".to_string(),
            client_ca_path: None,
        };
        let err = build_rustls_config(&tls).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("/no/such/cert.pem"),
            "error should name the cert path: {message}"
        );
    }

    /// `reload_certs` accepts a fresh PEM pair and returns Ok.
    #[tokio::test]
    async fn reload_certs_accepts_a_valid_pem_pair() {
        let (cert_a, key_a, _dir_a) = write_self_signed(&["localhost"]);
        let rustls = build_rustls_config(&TlsConfig {
            cert_path: cert_a,
            key_path: key_a,
            client_ca_path: None,
        })
        .await
        .unwrap();

        let (cert_b, key_b, _dir_b) = write_self_signed(&["localhost"]);
        reload_certs(
            &rustls,
            &TlsConfig {
                cert_path: cert_b,
                key_path: key_b,
                client_ca_path: None,
            },
        )
        .await
        .expect("reload should accept a fresh self-signed pair");
    }

    /// `reload_certs` reports the offending path in the error when the new
    /// material cannot be loaded.
    #[tokio::test]
    async fn reload_certs_reports_bad_paths_in_the_error() {
        let (cert_a, key_a, _dir_a) = write_self_signed(&["localhost"]);
        let rustls = build_rustls_config(&TlsConfig {
            cert_path: cert_a,
            key_path: key_a,
            client_ca_path: None,
        })
        .await
        .unwrap();

        let err = reload_certs(
            &rustls,
            &TlsConfig {
                cert_path: "/no/such/cert.pem".to_string(),
                key_path: "/no/such/key.pem".to_string(),
                client_ca_path: None,
            },
        )
        .await
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("/no/such/cert.pem"), "{message}");
    }

    /// End-to-end proof that `reload_certs` rotates the live cert: a server
    /// boots with cert A; reload_certs swaps in cert B; new handshakes only
    /// succeed with clients that trust B (not A).
    #[tokio::test]
    async fn reload_certs_rotates_the_live_cert_for_new_handshakes() {
        use axum::Router;
        use axum::routing::get;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let (cert_a_path, key_a_path, _dir_a) = write_self_signed(&["localhost"]);
        let cert_a_pem = std::fs::read(&cert_a_path).unwrap();
        let rustls = build_rustls_config(&TlsConfig {
            cert_path: cert_a_path,
            key_path: key_a_path,
            client_ca_path: None,
        })
        .await
        .unwrap();

        let listener =
            std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/ping", get(|| async { "pong" }));

        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let rustls_for_server = rustls.clone();
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, rustls_for_server)
                .handle(server_handle)
                .serve(app.into_make_service())
                .await
        });

        let url = format!("https://localhost:{}/ping", addr.port());

        // Cert A is live: clients trusting A succeed.
        let client_a = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&cert_a_pem).unwrap())
            .build()
            .unwrap();
        let response = client_a.get(&url).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);

        // Rotate to cert B.
        let (cert_b_path, key_b_path, _dir_b) = write_self_signed(&["localhost"]);
        let cert_b_pem = std::fs::read(&cert_b_path).unwrap();
        reload_certs(
            &rustls,
            &TlsConfig {
                cert_path: cert_b_path,
                key_path: key_b_path,
                client_ca_path: None,
            },
        )
        .await
        .unwrap();

        // New connections trusting B succeed; new connections trusting only A
        // fail at the TLS handshake (cert chain not anchored in their root
        // store). Each call uses a fresh client to force a fresh handshake.
        let client_b = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&cert_b_pem).unwrap())
            .build()
            .unwrap();
        let response = client_b.get(&url).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let client_a_only = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(&cert_a_pem).unwrap())
            .build()
            .unwrap();
        let result = client_a_only.get(&url).send().await;
        assert!(
            result.is_err(),
            "client that only trusts the old cert should fail after rotation, got {result:?}"
        );

        handle.graceful_shutdown(Some(std::time::Duration::from_millis(100)));
        let _ = server.await;
    }

    /// End-to-end check that a self-signed cert built through
    /// [`build_rustls_config`] actually serves HTTPS that a reqwest client
    /// (with the same cert pinned as a root) can reach.
    #[tokio::test]
    async fn served_listener_accepts_https_with_pinned_cert() {
        use axum::Router;
        use axum::routing::get;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let (cert_path, key_path, _dir) = write_self_signed(&["localhost"]);
        let pem = std::fs::read(&cert_path).unwrap();
        let rustls = build_rustls_config(&TlsConfig {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
            client_ca_path: None,
        })
        .await
        .unwrap();

        // Bind to an ephemeral port and recover the actual address so we can
        // dial it back deterministically.
        let listener =
            std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/ping", get(|| async { "pong" }));

        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, rustls)
                .handle(server_handle)
                .serve(app.into_make_service())
                .await
        });

        // Build a client that trusts the self-signed cert (no danger flag).
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap())
            .build()
            .unwrap();
        let url = format!("https://localhost:{}/ping", addr.port());
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.text().await.unwrap(), "pong");

        handle.graceful_shutdown(Some(std::time::Duration::from_millis(100)));
        let _ = server.await;
    }

    // -- mTLS ---------------------------------------------------------------

    /// Generate a self-signed CA certificate and its key.
    fn make_ca() -> (rcgen::Certificate, rcgen::KeyPair) {
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "VaultPlane Test CA");
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert, key)
    }

    /// Generate an end-entity cert (PEM cert, PEM key) signed by the given CA.
    /// `sans` are DNS names; `client_auth` chooses the extended key usage.
    fn make_leaf(
        ca_cert: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
        sans: Vec<String>,
        client_auth: bool,
    ) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(sans).unwrap();
        params.extended_key_usages = vec![if client_auth {
            rcgen::ExtendedKeyUsagePurpose::ClientAuth
        } else {
            rcgen::ExtendedKeyUsagePurpose::ServerAuth
        }];
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, ca_cert, ca_key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// Write `contents` to `dir/name` and return the path as a string.
    fn write_file(dir: &std::path::Path, name: &str, contents: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn build_rustls_config_accepts_a_client_ca_bundle() {
        let (ca_cert, ca_key) = make_ca();
        let (server_cert, server_key) =
            make_leaf(&ca_cert, &ca_key, vec!["localhost".to_string()], false);
        let dir = tempfile::tempdir().unwrap();
        let tls = TlsConfig {
            cert_path: write_file(dir.path(), "cert.pem", &server_cert),
            key_path: write_file(dir.path(), "key.pem", &server_key),
            client_ca_path: Some(write_file(dir.path(), "ca.pem", &ca_cert.pem())),
        };
        build_rustls_config(&tls)
            .await
            .expect("a valid mTLS config should build");
    }

    #[tokio::test]
    async fn build_rustls_config_reports_a_missing_client_ca() {
        let (cert, key, _dir) = write_self_signed(&["localhost"]);
        let tls = TlsConfig {
            cert_path: cert,
            key_path: key,
            client_ca_path: Some("/no/such/ca.pem".to_string()),
        };
        let err = build_rustls_config(&tls).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("/no/such/ca.pem"),
            "error should name the client CA path: {message}"
        );
    }

    /// End-to-end: a listener configured for mTLS accepts a client presenting a
    /// CA-signed certificate and rejects one that presents none.
    #[tokio::test]
    async fn mtls_requires_a_client_certificate() {
        use axum::Router;
        use axum::routing::get;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let (ca_cert, ca_key) = make_ca();
        let ca_pem = ca_cert.pem();
        let (server_cert, server_key) =
            make_leaf(&ca_cert, &ca_key, vec!["localhost".to_string()], false);
        let (client_cert, client_key) = make_leaf(&ca_cert, &ca_key, Vec::new(), true);

        let dir = tempfile::tempdir().unwrap();
        let rustls = build_rustls_config(&TlsConfig {
            cert_path: write_file(dir.path(), "server.pem", &server_cert),
            key_path: write_file(dir.path(), "server.key", &server_key),
            client_ca_path: Some(write_file(dir.path(), "ca.pem", &ca_pem)),
        })
        .await
        .unwrap();

        let listener =
            std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/ping", get(|| async { "pong" }));
        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, rustls)
                .handle(server_handle)
                .serve(app.into_make_service())
                .await
        });

        let url = format!("https://localhost:{}/ping", addr.port());
        let ca = reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap();

        // A client that presents a CA-signed identity is accepted.
        let identity =
            reqwest::Identity::from_pem(format!("{client_cert}{client_key}").as_bytes()).unwrap();
        let good = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca.clone())
            .identity(identity)
            .build()
            .unwrap();
        let response = good.get(&url).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);

        // A client that presents no certificate is rejected at the handshake.
        let anonymous = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca)
            .build()
            .unwrap();
        let result = anonymous.get(&url).send().await;
        assert!(
            result.is_err(),
            "a client with no certificate should be rejected, got {result:?}"
        );

        handle.graceful_shutdown(Some(std::time::Duration::from_millis(100)));
        let _ = server.await;
    }
}
