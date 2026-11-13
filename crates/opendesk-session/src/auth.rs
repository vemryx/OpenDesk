//! Senha de acesso não supervisionado.
//!
//! A senha nunca atravessa a rede, nem cifrada. Os dois lados rodam SPAKE2
//! (um PAKE): cada tentativa de quem não sabe a senha vale **um** palpite, que
//! o host conta e limita, e quem observa ou intermedeia a troca não leva nada
//! que permita testar senhas por conta própria.
//!
//! A troca é amarrada às identidades TLS dos dois dispositivos (as impressões
//! digitais entram como identidades do SPAKE2). Um intermediário que
//! apresente outro certificado para cada lado faz as chaves divergirem e a
//! confirmação falhar, mesmo que repasse as mensagens sem alterar.
//!
//! O host não guarda a senha: guarda um sal e a senha esticada com
//! PBKDF2-HMAC-SHA256. Esse valor é equivalente à senha **para este host**
//! (quem o rouba do disco consegue entrar), então fica num arquivo à parte, ao
//! lado da chave do dispositivo; mas não revela a senha digitada.

use anyhow::{Result, bail};
use opendesk_net::Fingerprint;
use opendesk_proto::control::PasswordChallenge;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{hmac, pbkdf2};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

/// Custo da derivação. Quem conecta paga uma vez por tentativa (frações de
/// segundo); quem roubar o arquivo do host paga por palpite.
const ITERATIONS: u32 = 600_000;
/// Tamanho mínimo aceito para uma senha nova.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Tentativas erradas toleradas, somando todos os dispositivos, antes de o
/// host parar de aceitar senha por um tempo.
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Tentativas erradas toleradas numa mesma conexão.
pub(crate) const MAX_FAILURES_PER_CONNECTION: usize = 3;

/// O que o host guarda para conferir a senha.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordAccess {
    salt: [u8; 16],
    iterations: u32,
    secret: [u8; 32],
}

impl std::fmt::Debug for PasswordAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordAccess(..)")
    }
}

impl PasswordAccess {
    /// Prepara o acesso com uma senha nova.
    pub fn create(password: &str) -> Result<Self> {
        if password.chars().count() < MIN_PASSWORD_LEN {
            bail!("a senha precisa ter pelo menos {MIN_PASSWORD_LEN} caracteres");
        }
        let mut salt = [0u8; 16];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| anyhow::anyhow!("o sistema não forneceu números aleatórios"))?;
        let challenge = PasswordChallenge { salt, iterations: ITERATIONS };
        Ok(Self { salt, iterations: ITERATIONS, secret: stretch(password, challenge) })
    }

    pub(crate) fn challenge(&self) -> PasswordChallenge {
        PasswordChallenge { salt: self.salt, iterations: self.iterations }
    }
}

/// Estica a senha com o sal do host. É a entrada do SPAKE2 nos dois lados.
/// Custa caro de propósito: rode fora da thread da interface.
pub fn stretch(password: &str, challenge: PasswordChallenge) -> [u8; 32] {
    // Um host malicioso não escolhe o custo: nem baixo demais, nem um que
    // trave quem conecta.
    let iterations = challenge.iterations.clamp(100_000, 5_000_000);
    let mut secret = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations).expect("limitado acima de zero"),
        &challenge.salt,
        password.as_bytes(),
        &mut secret,
    );
    secret
}

const VIEWER_PROOF: &[u8] = b"opendesk-auth-v1 viewer";
const HOST_PROOF: &[u8] = b"opendesk-auth-v1 host";

fn proof(key: &[u8], label: &[u8]) -> [u8; 32] {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), label);
    tag.as_ref().try_into().expect("HMAC-SHA256 tem 32 bytes")
}

fn verify(key: &[u8], label: &[u8], tag: &[u8; 32]) -> bool {
    // Comparação em tempo constante.
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), label, tag).is_ok()
}

/// O lado de quem conecta.
pub(crate) struct ViewerExchange {
    state: Spake2<Ed25519Group>,
}

impl ViewerExchange {
    /// Devolve a troca em andamento e a mensagem a enviar ao host.
    pub(crate) fn start(secret: &[u8; 32], viewer: Fingerprint, host: Fingerprint) -> (Self, Vec<u8>) {
        let (state, message) =
            Spake2::<Ed25519Group>::start_a(&Password::new(secret), &Identity::new(&viewer.0), &Identity::new(&host.0));
        (Self { state }, message)
    }

    /// Com a mensagem do host, devolve a chave e a prova a enviar.
    pub(crate) fn finish(self, host_message: &[u8]) -> Result<(SessionKey, [u8; 32])> {
        let key = self.state.finish(host_message).map_err(|_| anyhow::anyhow!("resposta de senha inválida do host"))?;
        let tag = proof(&key, VIEWER_PROOF);
        Ok((SessionKey(key), tag))
    }
}

pub(crate) struct SessionKey(Vec<u8>);

impl SessionKey {
    /// Confere a prova de que o host também conhece a senha.
    pub(crate) fn host_is_genuine(&self, tag: &[u8; 32]) -> bool {
        verify(&self.0, HOST_PROOF, tag)
    }
}

/// O lado do host numa tentativa.
pub(crate) struct HostExchange {
    key: Vec<u8>,
}

impl HostExchange {
    /// Responde à mensagem do visualizador. Devolve a troca e a mensagem a
    /// enviar de volta.
    pub(crate) fn respond(
        access: &PasswordAccess,
        viewer: Fingerprint,
        host: Fingerprint,
        viewer_message: &[u8],
    ) -> Result<(Self, Vec<u8>)> {
        let (state, message) = Spake2::<Ed25519Group>::start_b(
            &Password::new(access.secret),
            &Identity::new(&viewer.0),
            &Identity::new(&host.0),
        );
        let key = state.finish(viewer_message).map_err(|_| anyhow::anyhow!("mensagem de senha inválida"))?;
        Ok((Self { key }, message))
    }

    /// Confere a prova do visualizador; se conferir, devolve a prova do host.
    pub(crate) fn check(&self, viewer_tag: &[u8; 32]) -> Option<[u8; 32]> {
        verify(&self.key, VIEWER_PROOF, viewer_tag).then(|| proof(&self.key, HOST_PROOF))
    }
}

/// Conta senhas erradas e fecha a porta por um tempo quando passam do limite.
/// É o que impede alguém de ficar testando senhas, uma por conexão.
#[derive(Default)]
pub(crate) struct Throttle {
    failures: VecDeque<Instant>,
}

impl Throttle {
    fn prune(&mut self, now: Instant) {
        while self.failures.front().is_some_and(|at| now.duration_since(*at) > FAILURE_WINDOW) {
            self.failures.pop_front();
        }
    }

    pub(crate) fn locked(&mut self, now: Instant) -> bool {
        self.prune(now);
        self.failures.len() >= MAX_FAILURES
    }

    pub(crate) fn record_failure(&mut self, now: Instant) {
        self.prune(now);
        self.failures.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(byte: u8) -> Fingerprint {
        Fingerprint([byte; 32])
    }

    /// Roda a troca inteira e diz se os dois lados se aceitaram.
    fn exchange(access: &PasswordAccess, typed: &str, viewer_sees_host: Fingerprint, host_is: Fingerprint) -> bool {
        let secret = stretch(typed, access.challenge());
        let (viewer, to_host) = ViewerExchange::start(&secret, fp(1), viewer_sees_host);
        let (host, to_viewer) = HostExchange::respond(access, fp(1), host_is, &to_host).unwrap();
        let (key, viewer_tag) = viewer.finish(&to_viewer).unwrap();
        match host.check(&viewer_tag) {
            Some(host_tag) => key.host_is_genuine(&host_tag),
            None => false,
        }
    }

    #[test]
    fn right_password_is_accepted_and_wrong_is_not() {
        let access = PasswordAccess::create("correta cavalo bateria").unwrap();
        assert!(exchange(&access, "correta cavalo bateria", fp(2), fp(2)));
        assert!(!exchange(&access, "correta cavalo bateri", fp(2), fp(2)));
        assert!(!exchange(&access, "", fp(2), fp(2)));
    }

    #[test]
    fn a_relayed_exchange_fails_even_with_the_right_password() {
        // O visualizador acha que fala com o dispositivo 9 (um intermediário),
        // que repassa tudo ao host verdadeiro, o 2. As identidades divergem.
        let access = PasswordAccess::create("correta cavalo bateria").unwrap();
        assert!(!exchange(&access, "correta cavalo bateria", fp(9), fp(2)));
    }

    #[test]
    fn short_passwords_are_refused() {
        assert!(PasswordAccess::create("1234567").is_err());
        assert!(PasswordAccess::create("12345678").is_ok());
    }

    #[test]
    fn stored_form_does_not_contain_the_password_and_round_trips() {
        let access = PasswordAccess::create("senha-de-teste-123").unwrap();
        let other = PasswordAccess::create("senha-de-teste-123").unwrap();
        assert_ne!(access.salt, other.salt, "cada senha definida tem o seu sal");
        assert_ne!(access.secret, other.secret);
        assert!(!format!("{access:?}").contains("senha"));
    }

    #[test]
    fn throttle_locks_after_too_many_failures_and_reopens_later() {
        let mut throttle = Throttle::default();
        let start = Instant::now();
        for _ in 0..MAX_FAILURES {
            assert!(!throttle.locked(start));
            throttle.record_failure(start);
        }
        assert!(throttle.locked(start));
        assert!(throttle.locked(start + FAILURE_WINDOW));
        assert!(!throttle.locked(start + FAILURE_WINDOW + Duration::from_secs(1)));
    }
}
