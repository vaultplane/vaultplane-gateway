//! Inbound TLS for the proxy listener.
//!
//! When `listen.tls` is set in the configuration, the proxy serves HTTPS via
//! [`axum_server`] using rustls. The admin listener stays plain HTTP: it is
//! intended for cluster-internal traffic, and most operators front it with
//! their own ingress.
//!
//! Cert rotation requires a process restart in this slice; the rustls config
//! is bound to the listener at startup and the hot-reload path leaves it
//! alone. `axum_server::tls_rustls::RustlsConfig::reload_from_pem_file` would
//! let a future slice rotate certs without dropping connections.

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
