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

use anyhow::Context;
use axum_server::tls_rustls::RustlsConfig;
use vaultplane_core::config::TlsConfig;

/// Load the cert and key from disk and build a rustls server config.
pub async fn build_rustls_config(tls: &TlsConfig) -> anyhow::Result<RustlsConfig> {
    RustlsConfig::from_pem_file(&tls.cert_path, &tls.key_path)
        .await
        .with_context(|| {
            format!(
                "failed to load TLS material (cert: {}, key: {})",
                tls.cert_path, tls.key_path
            )
        })
}

/// Re-read the cert and key from disk and swap them into the live rustls
/// config in place. New TLS handshakes use the new material; connections
/// already established keep their negotiated cert.
///
/// `reload_from_pem_file` is itself transactional: it parses both files
/// before swapping, so a malformed cert leaves the old material active and
/// surfaces an error.
pub async fn reload_certs(rustls: &RustlsConfig, tls: &TlsConfig) -> anyhow::Result<()> {
    rustls
        .reload_from_pem_file(&tls.cert_path, &tls.key_path)
        .await
        .with_context(|| {
            format!(
                "failed to reload TLS material (cert: {}, key: {})",
                tls.cert_path, tls.key_path
            )
        })
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
        })
        .await
        .unwrap();

        let (cert_b, key_b, _dir_b) = write_self_signed(&["localhost"]);
        reload_certs(
            &rustls,
            &TlsConfig {
                cert_path: cert_b,
                key_path: key_b,
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
        })
        .await
        .unwrap();

        let err = reload_certs(
            &rustls,
            &TlsConfig {
                cert_path: "/no/such/cert.pem".to_string(),
                key_path: "/no/such/key.pem".to_string(),
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
}
