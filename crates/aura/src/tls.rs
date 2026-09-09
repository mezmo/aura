//! TLS trust configuration for outbound reqwest clients.
//!
//! The config load path freezes the validated CA bundle bytes into
//! [`aura_config::TlsConfig`] once at startup; applying them here is an
//! in-memory operation and never re-reads the bundle file.

use anyhow::Context;
use aura_config::TlsConfig;

/// Apply custom trusted CA roots onto `builder`, additively on top of the
/// built-in webpki roots.
///
/// Each certificate in `tls.frozen_bundle` is added as an extra trust root;
/// built-in roots are never removed, so publicly-rooted endpoints keep
/// working. `None` or an empty `frozen_bundle` returns the builder
/// unchanged. Failures name the configured `ca_bundle` path. DER validity
/// is enforced later, when the built client's root store parses each
/// certificate.
pub fn apply(
    builder: reqwest::ClientBuilder,
    tls: Option<&TlsConfig>,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let Some(tls) = tls else {
        return Ok(builder);
    };
    if tls.frozen_bundle.is_empty() {
        return Ok(builder);
    }

    let certificates =
        reqwest::Certificate::from_pem_bundle(&tls.frozen_bundle).with_context(|| {
            format!(
                "failed to parse TLS CA bundle '{}'",
                tls.ca_bundle.display()
            )
        })?;

    let mut builder = builder;
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    const RESPONSE_BODY: &str = "p49 ok";
    const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

    fn rcgen_ca() -> (rcgen::Certificate, rcgen::KeyPair) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "p49 test ca");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        (ca_cert, ca_key)
    }

    fn server_leaf(
        ca_cert: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
    ) -> (rcgen::Certificate, rcgen::KeyPair) {
        let server_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "127.0.0.1");
        params.subject_alt_names.push(SanType::IpAddress(
            std::net::Ipv4Addr::new(127, 0, 0, 1).into(),
        ));
        let server_cert = params.signed_by(&server_key, ca_cert, ca_key).unwrap();
        (server_cert, server_key)
    }

    fn acceptor_for(server_cert: &rcgen::Certificate, server_key: &rcgen::KeyPair) -> TlsAcceptor {
        let cert: CertificateDer<'static> = server_cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        TlsAcceptor::from(Arc::new(config))
    }

    /// Binds a TLS server on 127.0.0.1:0 that accepts exactly one
    /// connection, reads to the end of the request headers, replies with a
    /// canned HTTP/1.1 200 and closes.
    async fn spawn_one_shot_server(acceptor: TlsAcceptor) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut tls) = acceptor.accept(stream).await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match tls.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                RESPONSE_BODY.len(),
                RESPONSE_BODY
            );
            let _ = tls.write_all(response.as_bytes()).await;
            let _ = tls.shutdown().await;
        });
        addr
    }

    async fn fresh_server() -> SocketAddr {
        let (ca_cert, ca_key) = rcgen_ca();
        let (server_cert, server_key) = server_leaf(&ca_cert, &ca_key);
        spawn_one_shot_server(acceptor_for(&server_cert, &server_key)).await
    }

    fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .timeout(CLIENT_TIMEOUT)
            .no_proxy()
    }

    fn test_tls(ca_pem: &[u8]) -> TlsConfig {
        TlsConfig {
            ca_bundle: PathBuf::from("/tmp/p49-test/ca.pem"),
            frozen_bundle: Arc::from(ca_pem),
        }
    }

    fn error_chain(err: reqwest::Error) -> String {
        format!("{:?}", anyhow::Error::from(err))
    }

    fn assert_certificate_failure(message: &str) {
        assert!(
            message.contains("certificate") || message.contains("handshake"),
            "error should report a certificate/handshake failure: {message}"
        );
    }

    #[tokio::test]
    async fn client_with_frozen_bundle_trusts_rcgen_ca() {
        let (ca_cert, ca_key) = rcgen_ca();
        let (server_cert, server_key) = server_leaf(&ca_cert, &ca_key);
        let addr = spawn_one_shot_server(acceptor_for(&server_cert, &server_key)).await;

        let tls = test_tls(ca_cert.pem().as_bytes());
        let client = apply(client_builder(), Some(&tls))
            .expect("a real rcgen CA bundle must parse")
            .build()
            .expect("reqwest client builder only fails on TLS backend init");

        let response = client
            .get(format!("https://{addr}/"))
            .send()
            .await
            .expect("client must trust the rcgen CA");
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), RESPONSE_BODY);
    }

    #[tokio::test]
    async fn default_client_rejects_rcgen_ca() {
        let addr = fresh_server().await;

        let client = client_builder().build().unwrap();
        let err = client
            .get(format!("https://{addr}/"))
            .send()
            .await
            .expect_err("default webpki roots must not trust the rcgen CA");
        assert_certificate_failure(&error_chain(err));
    }

    #[tokio::test]
    async fn apply_without_bundle_behaves_like_default_client() {
        let addr = fresh_server().await;
        let untouched = apply(client_builder(), None)
            .expect("None must leave the builder unchanged")
            .build()
            .unwrap();
        let err = untouched
            .get(format!("https://{addr}/"))
            .send()
            .await
            .expect_err("a client without the bundle must reject the rcgen CA");
        assert_certificate_failure(&error_chain(err));

        let addr = fresh_server().await;
        let empty = TlsConfig {
            ca_bundle: PathBuf::from("/tmp/p49-test/ca.pem"),
            frozen_bundle: Default::default(),
        };
        let client = apply(client_builder(), Some(&empty))
            .expect("an empty frozen bundle must leave the builder unchanged")
            .build()
            .unwrap();
        let err = client
            .get(format!("https://{addr}/"))
            .send()
            .await
            .expect_err("an empty frozen bundle must not add trust roots");
        assert_certificate_failure(&error_chain(err));
    }

    #[tokio::test]
    async fn validated_bundle_bytes_drive_a_working_client() {
        let (ca_cert, ca_key) = rcgen_ca();
        let (server_cert, server_key) = server_leaf(&ca_cert, &ca_key);
        let addr = spawn_one_shot_server(acceptor_for(&server_cert, &server_key)).await;

        let dir = tempfile::TempDir::new().unwrap();
        let bundle_path = dir.path().join("ca.pem");
        std::fs::write(&bundle_path, ca_cert.pem()).unwrap();
        let mut config = aura_config::Config {
            tls: Some(TlsConfig {
                ca_bundle: bundle_path,
                frozen_bundle: Default::default(),
            }),
            ..Default::default()
        };
        let frozen = config
            .validate_tls_bundle()
            .expect("a real rcgen CA must pass validation");
        if let Some(tls) = config.tls.as_mut() {
            tls.frozen_bundle = frozen.into();
        }

        let client = apply(client_builder(), config.tls.as_ref())
            .expect("real DER must parse")
            .build()
            .unwrap();
        let response = client
            .get(format!("https://{addr}/"))
            .send()
            .await
            .expect("client must trust the rcgen CA through the load path");
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), RESPONSE_BODY);
    }

    #[test]
    fn apply_error_names_ca_bundle_path() {
        let corrupted =
            "-----BEGIN CERTIFICATE-----\n!!!not base64!!!\n-----END CERTIFICATE-----\n";
        let tls = TlsConfig {
            ca_bundle: PathBuf::from("/etc/p49/corporate-ca.pem"),
            frozen_bundle: Arc::from(corrupted.as_bytes()),
        };

        let err = apply(reqwest::Client::builder(), Some(&tls))
            .expect_err("a corrupted PEM block must fail");
        assert!(
            err.to_string().contains("/etc/p49/corporate-ca.pem"),
            "error must name the configured path: {err}"
        );
    }
}
