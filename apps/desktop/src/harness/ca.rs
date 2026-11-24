//! The local certificate authority that signs certificates for `*.cursor.sh`.
//!
//! It lives in the app's data directory and stays valid for years; Cursor
//! accepts it only once the user has trusted it in the system store, which is
//! why the app can report the state and the command but never escalate itself.

use std::{fs, path::PathBuf};

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_RSA_SHA256, RsaKeySize,
};
#[cfg(target_os = "macos")]
use sha1::{Digest, Sha1};
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::FromDer;

use super::{CaState, Error, Result};

/// A loaded issuer, ready to sign leaf certificates.
pub struct LoadedCa {
    issuer: Issuer<'static, KeyPair>,
}

impl LoadedCa {
    /// Takes the issuer the proxy signs leaf certificates with.
    ///
    /// This hands over the CA private key, so the caller must be the thing that
    /// terminates TLS.
    pub fn into_issuer(self) -> Issuer<'static, KeyPair> {
        self.issuer
    }
}

impl std::fmt::Debug for LoadedCa {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The issuer holds a private key; never render it.
        formatter.debug_struct("LoadedCa").finish_non_exhaustive()
    }
}

/// How long a generated CA stays valid. Ten years outlasts any Cursor install,
/// and regenerating would mean asking the user to trust a new CA again.
const CA_LIFETIME: Duration = Duration::days(3652);
/// Backdated `not_before`, so a clock a few minutes slow still accepts the CA.
const CLOCK_SKEW: Duration = Duration::minutes(5);

/// Owns the CA files: generating them once, loading them, and telling whether
/// the system trusts them.
#[derive(Debug)]
pub struct CaManager {
    dir: PathBuf,
}

impl CaManager {
    /// A manager over an explicit `ca` directory, for tests and for the real data dir.
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The PEM certificate the system trust store holds.
    pub fn cert_path(&self) -> PathBuf {
        self.dir.join("ca.crt")
    }

    /// The PEM private key that signs leaf certificates.
    pub fn key_path(&self) -> PathBuf {
        self.dir.join("ca.key")
    }

    /// Missing, untrusted, ready or invalid. `Ready` is the only state in which
    /// takeover may start.
    pub fn state(&self) -> Result<CaState> {
        let cert = fs::read_to_string(self.cert_path());
        let key = fs::read_to_string(self.key_path());
        match (cert, key) {
            (Err(cert_error), Err(key_error))
                if cert_error.kind() == std::io::ErrorKind::NotFound
                    && key_error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(CaState::Missing)
            }
            (Ok(cert), Ok(key)) => {
                if parse_issuer(&cert, &key).is_err() {
                    return Ok(CaState::Invalid);
                }
                Ok(if is_installed(&cert)? {
                    CaState::Ready
                } else {
                    CaState::Untrusted
                })
            }
            _ => Ok(CaState::Invalid),
        }
    }

    /// Parses the stored pair into an issuer the proxy can sign leaves with.
    pub fn load(&self) -> Result<LoadedCa> {
        let cert = fs::read_to_string(self.cert_path())?;
        let key = fs::read_to_string(self.key_path())?;
        Ok(LoadedCa {
            issuer: parse_issuer(&cert, &key)?,
        })
    }

    /// The command a user pastes into a terminal to trust this CA. The app
    /// cannot run it: it needs an administrator password.
    pub fn install_command(&self) -> Option<String> {
        let path = self.cert_path().to_string_lossy().replace('\'', "'\\''");
        match std::env::consts::OS {
            "macos" => Some(format!(
                "sudo security add-trusted-cert -d -r trustRoot -p ssl -k /Library/Keychains/System.keychain '{path}'"
            )),
            "windows" => Some(format!(
                "certutil -addstore -f Root \"{}\"",
                self.cert_path().display()
            )),
            "linux" => {
                let anchor = linux_anchor_file();
                Some(format!(
                    "sudo cp '{}' '{}' && sudo {}",
                    path,
                    anchor.display(),
                    linux_refresh_command()
                ))
            }
            _ => None,
        }
    }

    /// Generates the CA when missing. A ready CA is left alone, so this is safe
    /// to call on every start.
    pub fn initialize(&self) -> Result<()> {
        match self.state()? {
            CaState::Invalid => {
                return Err(Error::Config("CA files are incomplete or invalid".into()));
            }
            CaState::Ready => return Ok(()),
            CaState::Missing => self.generate()?,
            CaState::Untrusted => {}
        }
        Ok(())
    }

    fn generate(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;

        let key = KeyPair::generate_rsa_for(&PKCS_RSA_SHA256, RsaKeySize::_3072)
            .map_err(|error| Error::Config(format!("generate CA key: {error}")))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|error| Error::Config(format!("create CA parameters: {error}")))?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "Cursor-BYOK-Next Local CA");
        name.push(DnType::OrganizationName, "Cursor-BYOK-Next");
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        params.not_before = OffsetDateTime::now_utc() - CLOCK_SKEW;
        params.not_after = OffsetDateTime::now_utc() + CA_LIFETIME;
        let cert = params
            .self_signed(&key)
            .map_err(|error| Error::Config(format!("generate CA certificate: {error}")))?;
        crate::atomic::write_with_mode(&self.key_path(), key.serialize_pem().as_bytes(), 0o600)?;
        crate::atomic::write_with_mode(&self.cert_path(), cert.pem().as_bytes(), 0o644)?;
        tracing::info!(dir = %self.dir.display(), "generated the local CA");
        Ok(())
    }
}

fn parse_issuer(cert: &str, key: &str) -> Result<Issuer<'static, KeyPair>> {
    let key =
        KeyPair::from_pem(key).map_err(|error| Error::Config(format!("parse CA key: {error}")))?;
    let pem = pem::parse(cert).map_err(|error| Error::Config(format!("parse CA PEM: {error}")))?;
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(pem.contents())
        .map_err(|error| Error::Config(format!("parse CA X.509 certificate: {error}")))?;
    if parsed.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        return Err(Error::Config(
            "the CA certificate and private key do not match".into(),
        ));
    }
    if !parsed.validity().is_valid() {
        return Err(Error::Config(
            "the CA certificate is outside its validity period".into(),
        ));
    }
    if !parsed
        .basic_constraints()
        .map_err(|error| Error::Config(format!("read CA constraints: {error}")))?
        .is_some_and(|constraints| constraints.value.ca)
    {
        return Err(Error::Config("the certificate is not a CA".into()));
    }
    Issuer::from_ca_cert_pem(cert, key)
        .map_err(|error| Error::Config(format!("parse CA certificate: {error}")))
}

#[cfg(target_os = "macos")]
fn fingerprint(cert: &str) -> Result<String> {
    let pem = pem::parse(cert).map_err(|error| Error::Config(format!("parse CA PEM: {error}")))?;
    Ok(hex::encode_upper(Sha1::digest(pem.contents())))
}

/// Whether the system trusts this CA: the login and System keychains on macOS,
/// the machine's Root store on Windows, the anchor file on Linux.
#[cfg(target_os = "macos")]
fn is_installed(cert: &str) -> Result<bool> {
    let fingerprint = fingerprint(cert)?;
    for keychain in ["login.keychain-db", "/Library/Keychains/System.keychain"] {
        let output = Command::new("security")
            .args(["find-certificate", "-a", "-Z", keychain])
            .output()?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&fingerprint)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "windows")]
fn is_installed(cert: &str) -> Result<bool> {
    super::windows::is_installed(cert)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_installed(cert: &str) -> Result<bool> {
    match fs::read_to_string(linux_anchor_file()) {
        Ok(installed) => Ok(installed.trim() == cert.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

const LINUX_ANCHOR_NAME: &str = "cursor-byok-next-local-ca.crt";

fn linux_anchor_file() -> PathBuf {
    if PathBuf::from("/etc/pki/ca-trust/source/anchors").is_dir() {
        PathBuf::from("/etc/pki/ca-trust/source/anchors").join(LINUX_ANCHOR_NAME)
    } else if PathBuf::from("/etc/ca-certificates/trust-source/anchors").is_dir() {
        PathBuf::from("/etc/ca-certificates/trust-source/anchors").join(LINUX_ANCHOR_NAME)
    } else {
        PathBuf::from("/usr/local/share/ca-certificates").join(LINUX_ANCHOR_NAME)
    }
}

fn linux_refresh_command() -> &'static str {
    match linux_anchor_file().parent().and_then(|dir| dir.to_str()) {
        Some("/usr/local/share/ca-certificates") => "update-ca-certificates",
        _ => "update-ca-trust extract",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_once_then_loads_the_same_issuer() {
        let dir = tempfile::tempdir().unwrap();
        let ca = CaManager::at(dir.path().join("ca"));

        assert_eq!(ca.state().unwrap(), CaState::Missing);
        ca.initialize().unwrap();
        let generated = fs::read_to_string(ca.cert_path()).unwrap();

        ca.initialize().unwrap();
        assert_eq!(fs::read_to_string(ca.cert_path()).unwrap(), generated);
        assert!(ca.load().is_ok());
    }

    #[test]
    fn a_key_that_does_not_match_the_certificate_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let ca = CaManager::at(dir.path().join("ca"));
        ca.initialize().unwrap();

        let other = KeyPair::generate_rsa_for(&PKCS_RSA_SHA256, RsaKeySize::_3072).unwrap();
        fs::write(ca.key_path(), other.serialize_pem()).unwrap();

        assert_eq!(ca.state().unwrap(), CaState::Invalid);
        assert!(ca.load().is_err());
    }
}
