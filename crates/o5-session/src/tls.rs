//! TLS 配置：rustls connector 构建（webpki-roots 或设备自签跳过校验）。
//!
//! 设备要连 `wss://<IP>:7029` 这类自签端点，`fg_tls_insecure_skip_verify=true`
//! 时用 dangerous 自定义 verifier（接受一切证书——等价 Python 的
//! `ssl.CERT_NONE`）；否则用 webpki-roots（Mozilla 内置根）。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

/// 接受一切服务端证书（仅用于 `tlsInsecureSkipVerify`，危险，勿他用）。
#[derive(Debug)]
struct AcceptAllVerifier;

impl ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // 与 ring provider 的握手签名算法保持一致
        ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 构建 rustls 客户端配置。
///
/// `insecure` = true：接受一切证书（设备连 `wss://IP:7029` 自签端点）；
/// 否则用 webpki-roots。显式指定 ring provider，避免进程级默认歧义。
pub fn client_config(insecure: bool) -> Arc<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    if insecure {
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring provider 必然支持 TLS12/13")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAllVerifier))
            .with_no_client_auth();
        Arc::new(config)
    } else {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring provider 必然支持 TLS12/13")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    }
}
