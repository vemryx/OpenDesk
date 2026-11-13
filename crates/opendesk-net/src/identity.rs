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

    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 || !text.is_ascii() {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// Apelido numérico de 9 dígitos. Não é segredo nem prova de identidade:
    /// quem autentica é a impressão digital inteira.
    pub fn device_id(&self) -> DeviceId {
        self.device_id_attempt(0)
    }

    /// ID alternativo para quando o primeiro já pertence a outro dispositivo
    /// no servidor de encontro (9 dígitos colidem cedo). A tentativa 0 usa os
    /// primeiros 8 bytes da impressão digital; as seguintes, os de
    /// SHA-256(impressão ‖ n). `rendezvous/src/identity.ts` faz a mesma conta.
    pub fn device_id_attempt(&self, attempt: u8) -> DeviceId {
        let source: [u8; 32] = if attempt == 0 {
            self.0
        } else {
            let mut hasher = Sha256::new();
            hasher.update(self.0);
            hasher.update([attempt]);
            hasher.finalize().into()
        };
        let n = u64::from_be_bytes(source[..8].try_into().unwrap());
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

impl DeviceId {
    /// Aceita o ID como as pessoas o digitam: `602 758 973`, `602-758-973`
    /// ou `602758973`. Pontos não são separadores: `192.168.0.10` também tem
    /// nove dígitos e é um endereço, não um ID.
    pub fn parse(text: &str) -> Option<Self> {
        let digits: String = text.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
        if digits.len() != 9 || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.starts_with('0') {
            return None;
        }
        digits.parse().ok().map(Self)
    }
}

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

    /// O certificado público deste dispositivo, em DER.
    pub fn certificate(&self) -> &[u8] {
        &self.cert_der
    }

    /// Assina `message` com a chave do dispositivo (ECDSA P-256 / SHA-256,
    /// formato fixo r‖s de 64 bytes). Prova posse da chave do certificado.
    pub fn sign(&self, message: &[u8]) -> Result<[u8; 64]> {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let rng = ring::rand::SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &self.key_pkcs8_der, &rng)
            .map_err(|err| anyhow::anyhow!("chave do dispositivo inválida: {err}"))?;
        let signature = key.sign(&rng, message).map_err(|_| anyhow::anyhow!("falha ao assinar"))?;
        signature.as_ref().try_into().context("assinatura com tamanho inesperado")
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
    fn device_id_parses_what_people_type() {
        assert_eq!(DeviceId::parse("602 758 973"), Some(DeviceId(602_758_973)));
        assert_eq!(DeviceId::parse(" 602-758-973 "), Some(DeviceId(602_758_973)));
        assert_eq!(DeviceId::parse("602758973"), Some(DeviceId(602_758_973)));
        for bad in ["", "60275897", "6027589730", "062758973", "602 758 97a", "192.168.0.10", "10.0.0.1:47700"] {
            assert_eq!(DeviceId::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn alternative_ids_are_stable_and_distinct() {
        let fingerprint = Fingerprint([7; 32]);
        assert_eq!(fingerprint.device_id(), fingerprint.device_id_attempt(0));
        assert_eq!(fingerprint.device_id_attempt(3), fingerprint.device_id_attempt(3));
        assert_ne!(fingerprint.device_id_attempt(0), fingerprint.device_id_attempt(1));
        assert_eq!(Fingerprint([0; 32]).device_id_attempt(0), DeviceId(100_000_000));
    }

    #[test]
    fn fingerprint_hex_roundtrips() {
        let fingerprint = Identity::generate().unwrap().fingerprint();
        assert_eq!(Fingerprint::from_hex(&fingerprint.hex()), Some(fingerprint));
        assert_eq!(Fingerprint::from_hex("abc"), None);
    }

    #[test]
    fn signature_verifies_against_the_certificate_key() {
        use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
        let identity = Identity::generate().unwrap();
        let signature = identity.sign(b"desafio").unwrap();
        // No SPKI, logo após o OID da curva P-256 vem BIT STRING (03 42 00) e
        // a chave pública sem compressão, de 65 bytes.
        const P256_OID: [u8; 8] = [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
        let cert = identity.certificate();
        let oid = cert.windows(8).position(|w| w == P256_OID).expect("certificado P-256");
        assert_eq!(cert[oid + 8..oid + 11], [0x03, 0x42, 0x00]);
        let public = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &cert[oid + 11..oid + 76]);
        public.verify(b"desafio", &signature).unwrap();
        assert!(public.verify(b"outro", &signature).is_err());
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
