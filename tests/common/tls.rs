use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use p12_keystore::{
    Certificate, EncryptionAlgorithm, KeyStore, KeyStoreEntry, MacAlgorithm, PrivateKey,
    PrivateKeyChain,
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use tempfile::{Builder, TempDir};
use time::{Duration, OffsetDateTime};
use toml::Value;

const KEEP_CERTS_ENV: &str = "RATHOLE_TEST_KEEP_CERTS";
const PKCS12_PASSWORD: &str = "rathole-integration-test";

// The rustls `p12` loader supports legacy PBE. Native TLS uses modern PBES2
// here to avoid platform/provider-dependent support for legacy algorithms.
#[cfg(feature = "rustls")]
const PKCS12_ENCRYPTION: EncryptionAlgorithm = EncryptionAlgorithm::PbeWithShaAnd3KeyTripleDesCbc;
#[cfg(feature = "rustls")]
const PKCS12_MAC: MacAlgorithm = MacAlgorithm::HmacSha1;

#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
const PKCS12_ENCRYPTION: EncryptionAlgorithm = EncryptionAlgorithm::PbeWithHmacSha256AndAes256;
#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
const PKCS12_MAC: MacAlgorithm = MacAlgorithm::HmacSha256;

/// A rendered integration-test config and its per-test TLS artifacts.
///
/// Artifacts are deleted when this value is dropped. Set
/// `RATHOLE_TEST_KEEP_CERTS=1` to retain them for inspection.
pub struct TlsTestConfig {
    config_path: PathBuf,
    temp_dir: Option<TempDir>,
}

impl TlsTestConfig {
    pub fn from_template(template_path: impl AsRef<Path>) -> Result<Self> {
        let template_path = template_path.as_ref();
        let temp_dir = Builder::new().prefix("rathole-tls-test-").tempdir()?;
        let artifact_dir = temp_dir.path().to_path_buf();
        let now = OffsetDateTime::now_utc();
        let not_before = now - Duration::days(1);
        let not_after = now + Duration::days(7);

        let ca_key = KeyPair::generate().context("failed to generate test CA key")?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params.not_before = not_before;
        ca_params.not_after = not_after;
        ca_params.distinguished_name = distinguished_name("rathole integration test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .context("failed to create test CA certificate")?;
        let ca_issuer = Issuer::new(ca_params, ca_key);

        let server_key = KeyPair::generate().context("failed to generate test server key")?;
        let mut server_params = CertificateParams::new(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ])?;
        server_params.not_before = not_before;
        server_params.not_after = not_after;
        server_params.distinguished_name = distinguished_name("localhost");
        server_params.use_authority_key_identifier_extension = true;
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = server_params
            .signed_by(&server_key, &ca_issuer)
            .context("failed to create test server certificate")?;

        let server_cert_der = server_cert.der().as_ref();
        let ca_cert_der = ca_cert.der().as_ref();
        let key_chain = PrivateKeyChain::new(
            "rathole integration test",
            PrivateKey::from_der(server_key.serialized_der())?,
            [
                Certificate::from_der(server_cert_der)?,
                Certificate::from_der(ca_cert_der)?,
            ],
        );
        let mut key_store = KeyStore::new();
        key_store.add_entry(
            "rathole integration test",
            KeyStoreEntry::PrivateKeyChain(key_chain),
        );
        let identity = key_store
            .writer(PKCS12_PASSWORD)
            .encryption_algorithm(PKCS12_ENCRYPTION)
            .mac_algorithm(PKCS12_MAC)
            .write()
            .context("failed to create test PKCS#12 identity")?;

        let trusted_root_path = artifact_dir.join("rootCA.crt");
        let ca_key_path = artifact_dir.join("rootCA.key");
        let server_cert_path = artifact_dir.join("server.crt");
        let server_key_path = artifact_dir.join("server.key");
        let identity_path = artifact_dir.join("identity.pfx");
        let config_path = artifact_dir.join(
            template_path
                .file_name()
                .context("TLS test config template has no file name")?,
        );

        fs::write(&trusted_root_path, ca_cert.pem())?;
        fs::write(ca_key_path, ca_issuer.key().serialize_pem())?;
        fs::write(server_cert_path, server_cert.pem())?;
        fs::write(server_key_path, server_key.serialize_pem())?;
        fs::write(&identity_path, identity)?;

        let mut config: Value = fs::read_to_string(template_path)
            .with_context(|| format!("failed to read {}", template_path.display()))?
            .parse()
            .with_context(|| format!("failed to parse {}", template_path.display()))?;
        set_config_value(
            &mut config,
            &["client", "transport", "tls", "trusted_root"],
            trusted_root_path.to_string_lossy().into_owned(),
        )?;
        set_config_value(
            &mut config,
            &["server", "transport", "tls", "pkcs12"],
            identity_path.to_string_lossy().into_owned(),
        )?;
        set_config_value(
            &mut config,
            &["server", "transport", "tls", "pkcs12_password"],
            PKCS12_PASSWORD.to_owned(),
        )?;
        fs::write(&config_path, toml::to_string(&config)?)?;

        let temp_dir = if std::env::var_os(KEEP_CERTS_ENV).is_some() {
            let retained_path = temp_dir.keep();
            eprintln!(
                "retained TLS test artifacts at {} because {KEEP_CERTS_ENV} is set",
                retained_path.display()
            );
            None
        } else {
            Some(temp_dir)
        };

        Ok(Self {
            config_path,
            temp_dir,
        })
    }

    pub fn path(&self) -> &Path {
        &self.config_path
    }

    pub fn close(mut self) -> Result<()> {
        if let Some(temp_dir) = self.temp_dir.take() {
            temp_dir
                .close()
                .context("failed to remove TLS test artifacts")?;
        }
        Ok(())
    }
}

fn distinguished_name(common_name: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, common_name);
    name
}

fn set_config_value(config: &mut Value, path: &[&str], value: String) -> Result<()> {
    let (key, parents) = path
        .split_last()
        .ok_or_else(|| anyhow!("config value path must not be empty"))?;
    let mut table = config;

    for parent in parents {
        table = table
            .get_mut(*parent)
            .ok_or_else(|| anyhow!("missing `{}` in TLS test config", path.join(".")))?;
    }

    table
        .as_table_mut()
        .ok_or_else(|| anyhow!("`{}` is not a TOML table", parents.join(".")))?
        .insert((*key).to_owned(), Value::String(value));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_cleans_up_tls_artifacts() -> Result<()> {
        let config = TlsTestConfig::from_template("tests/for_tcp/tls_transport.toml")?;
        let artifact_dir = config
            .path()
            .parent()
            .context("generated config has no parent directory")?
            .to_path_buf();

        for file in [
            "rootCA.crt",
            "rootCA.key",
            "server.crt",
            "server.key",
            "identity.pfx",
            "tls_transport.toml",
        ] {
            assert!(artifact_dir.join(file).is_file(), "missing {file}");
        }

        let rendered_config = fs::read_to_string(config.path())?;
        assert!(!rendered_config.contains("generated by tests/common/tls.rs"));
        config.close()?;

        if std::env::var_os(KEEP_CERTS_ENV).is_none() {
            assert!(!artifact_dir.exists());
        }

        Ok(())
    }
}
