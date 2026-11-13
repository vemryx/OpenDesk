//! Envelope dos pacotes que passam por um relay.
//!
//! ```text
//! 0x02 | token[16] | carga (um pacote QUIC inteiro, ou vazia)
//! ```
//!
//! O relay junta os dois primeiros endereços que aparecem com o mesmo token e
//! passa a repassar os pacotes de um para o outro, sem tocar na carga. Um
//! pacote de carga vazia só apresenta o remetente ao relay.
//!
//! O primeiro byte tem os dois bits altos zerados, então nunca se confunde
//! com QUIC; e não é STUN porque o relay e os nós conferem o token antes.

pub const MAGIC: u8 = 0x02;
pub const TOKEN_LEN: usize = 16;
pub const HEADER_LEN: usize = 1 + TOKEN_LEN;
/// Porta UDP padrão de um relay.
pub const DEFAULT_PORT: u16 = 47800;

pub type Token = [u8; TOKEN_LEN];

/// Separa token e carga de um datagrama de relay.
pub fn parse(datagram: &[u8]) -> Option<(Token, &[u8])> {
    if datagram.len() < HEADER_LEN || datagram[0] != MAGIC {
        return None;
    }
    let token = datagram[1..HEADER_LEN].try_into().ok()?;
    Some((token, &datagram[HEADER_LEN..]))
}

/// Monta um datagrama de relay.
pub fn wrap(token: &Token, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(MAGIC);
    out.extend_from_slice(token);
    out.extend_from_slice(payload);
    out
}

pub fn token_to_hex(token: &Token) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn token_from_hex(text: &str) -> Option<Token> {
    if text.len() != TOKEN_LEN * 2 || !text.is_ascii() {
        return None;
    }
    let mut token = [0u8; TOKEN_LEN];
    for (i, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_and_parse_roundtrip() {
        let token = [7u8; TOKEN_LEN];
        let datagram = wrap(&token, b"quic");
        assert_eq!(parse(&datagram), Some((token, &b"quic"[..])));
        assert_eq!(parse(&wrap(&token, b"")), Some((token, &b""[..])));
    }

    #[test]
    fn rejects_short_and_foreign_datagrams() {
        assert_eq!(parse(&[MAGIC; HEADER_LEN - 1]), None);
        let mut quic = wrap(&[1; TOKEN_LEN], b"x");
        quic[0] = 0x40;
        assert_eq!(parse(&quic), None);
    }

    #[test]
    fn token_hex_roundtrip() {
        let token: Token = core::array::from_fn(|i| i as u8 * 17);
        assert_eq!(token_from_hex(&token_to_hex(&token)), Some(token));
        assert_eq!(token_from_hex("zz"), None);
    }
}
