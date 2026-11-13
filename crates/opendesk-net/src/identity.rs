use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::Path;

const CERT_FILE: &str = "identity.cert.der";
const KEY_FILE: &str = "identity.key.der";

/// SHA-256 do certificado do dispositivo.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of_cert(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Forma curta para conferência visual: `a1b2-c3d4-e5f6-0718`.
    pub fn short(&self) -> String {
        self.0[..8].chunks(2).map(|pair| format!("{:02x}{:02x}", pair[0], pair[1])).collect::<Vec<_>>().join("-")
    }

    /// Apelido numérico de 9 dígitos. Não é segredo nem prova de identidade:
    /// quem autentica é a impressão digital inteira.
    pub fn device_id(&self) -> DeviceId {
        let n = u64::from_be_bytes(self.0[..8].try_into().unwrap());
        DeviceId(100_000_000 + (n % 900_000_000) as u32)
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DeviceId(pub u32);

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        write!(f, "{:03} {:03} {:03}", n / 1_000_000, (n / 1_000) % 1_000, n % 1_000)
    }
}

/// Par de chaves e certificado deste dispositivo.
pub struct Identity {
    pub(crate) cert_der: Vec<u8>,
    pub(crate) key_pkcs8_der: Vec<u8>,
    fingerprint: Fingerprint,
}

impl Identity {
    /// Gera uma identidade nova, só em memória.
    pub fn generate() -> Result<Self> {
        let key = rcgen::KeyPair::generate().context("gerar par de chaves")?;
        let params =
            rcgen::CertificateParams::new(vec!["opendesk.device".to_string()]).context("parâmetros do certificado")?;
        let cert = params.self_signed(&key).context("assinar certificado")?;
        let cert_der = cert.der().to_vec();
        Ok(Self { fingerprint: Fingerprint::of_cert(&cert_der), cert_der, key_pkcs8_der: key.serialize_der() })
    }

    /// Carrega a identidade de `dir`, criando-a na primeira execução.
    ///
    /// A chave privada fica em disco sem cifragem, protegida só pelas
    /// permissões do perfil do usuário. Protegê-la com DPAPI/Keychain está no
    /// roteiro.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        if cert_path.exists() && key_path.exists() {
            let cert_der = std::fs::read(&cert_path).with_context(|| format!("ler {}", cert_path.display()))?;
            let key_pkcs8_der = std::fs::read(&key_path).with_context(|| format!("ler {}", key_path.display()))?;
            return Ok(Self { fingerprint: Fingerprint::of_cert(&cert_der), cert_der, key_pkcs8_der });
        }
        std::fs::create_dir_all(dir).with_context(|| format!("criar {}", dir.display()))?;
        let identity = Self::generate()?;
        std::fs::write(&key_path, &identity.key_pkcs8_der).with_context(|| format!("gravar {}", key_path.display()))?;
        std::fs::write(&cert_path, &identity.cert_der).with_context(|| format!("gravar {}", cert_path.display()))?;
        Ok(identity)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn device_id(&self) -> DeviceId {
        self.fingerprint.device_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_nine_digits_and_formats_in_groups() {
        for seed in [0u8, 1, 0x7F, 0xFF] {
            let id = Fingerprint([seed; 32]).device_id();
            assert!((100_000_000..1_000_000_000).contains(&id.0));
            let text = id.to_string();
            assert_eq!(text.len(), 11);
            assert_eq!(text.replace(' ', "").parse::<u32>().unwrap(), id.0);
        }
    }

    #[test]
    fn identity_persists_across_loads() {
        let dir = std::env::temp_dir().join(format!("opendesk-id-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Identity::load_or_create(&dir).unwrap();
        let second = Identity::load_or_create(&dir).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_ne!(first.fingerprint(), Identity::generate().unwrap().fingerprint());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
