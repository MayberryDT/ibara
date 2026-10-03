use anyhow::{Context, Result, ensure};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::Arc,
};
pub const ALPN: &[u8] = b"ibara-screen/1";
pub fn hash(c: &[u8]) -> String {
    hex::encode(Sha256::digest(c))
}
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
}
impl Identity {
    pub fn load(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("identity.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "identity lock failed"
        );
        let cp = dir.join("cert.der");
        let kp = dir.join("key.der");
        if !cp.exists() && !kp.exists() {
            let c = rcgen::generate_simple_self_signed(vec!["ibara-screen".into()])?;
            let mut k = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&kp)?;
            k.write_all(&c.key_pair.serialize_der())?;
            k.sync_all()?;
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&cp)?;
            f.write_all(c.cert.der())?;
            f.sync_all()?;
        }
        ensure!(
            fs::metadata(&kp)
                .context("incomplete identity")?
                .permissions()
                .mode()
                & 0o077
                == 0,
            "identity key is not private"
        );
        let cert = CertificateDer::from(fs::read(cp)?);
        let key = PrivatePkcs8KeyDer::from(fs::read(kp)?);
        // Validate the key and certificate together before emitting an identity hash.
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.clone_key().into())?;
        Ok(Self { cert, key })
    }
}
#[derive(Debug)]
struct PeerVerifier {
    pin: Option<String>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl PeerVerifier {
    fn signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
        tls13: bool,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        if tls13 {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        } else {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
    }
}
impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.pin.as_deref() == Some(&hash(end)) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate pin mismatch".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(m, c, d, false)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(m, c, d, true)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
impl ClientCertVerifier for PeerVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(m, c, d, false)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(m, c, d, true)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub fn server(id: &Identity) -> Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(PeerVerifier {
            pin: None,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }))
        .with_single_cert(vec![id.cert.clone()], id.key.clone_key().into())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut t = quinn::TransportConfig::default();
    t.max_concurrent_bidi_streams(1u32.into())
        .max_concurrent_uni_streams(1u32.into())
        .max_idle_timeout(Some(std::time::Duration::from_secs(3).try_into()?));
    cfg.transport_config(Arc::new(t));
    Ok(cfg)
}
pub fn client(id: &Identity, pin: &str) -> Result<quinn::ClientConfig> {
    ensure!(
        pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid server certificate pin"
    );
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PeerVerifier {
            pin: Some(pin.to_ascii_lowercase()),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }))
        .with_client_auth_cert(vec![id.cert.clone()], id.key.clone_key().into())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut cfg = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
    ));
    let mut t = quinn::TransportConfig::default();
    t.max_concurrent_uni_streams(16u32.into())
        .max_idle_timeout(Some(std::time::Duration::from_secs(3).try_into()?));
    cfg.transport_config(Arc::new(t));
    Ok(cfg)
}
