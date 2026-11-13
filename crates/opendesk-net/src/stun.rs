//! O mínimo de STUN (RFC 8489) para descobrir como o mundo enxerga o nosso
//! socket: um "Binding Request" e a leitura do endereço na resposta.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const BINDING_REQUEST: [u8; 2] = [0x00, 0x01];
const BINDING_SUCCESS: [u8; 2] = [0x01, 0x01];
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const FAMILY_IPV4: u8 = 0x01;

pub(crate) type TransactionId = [u8; 12];

/// Todo pacote STUN começa com dois bits zerados e traz o cookie fixo.
pub(crate) fn looks_like_stun(data: &[u8]) -> bool {
    data.len() >= 20 && data[0] & 0xC0 == 0 && data[4..8] == MAGIC_COOKIE
}

pub(crate) fn binding_request(transaction: &TransactionId) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[..2].copy_from_slice(&BINDING_REQUEST);
    // bytes 2..4: tamanho dos atributos = 0
    packet[4..8].copy_from_slice(&MAGIC_COOKIE);
    packet[8..].copy_from_slice(transaction);
    packet
}

/// Lê uma resposta de sucesso e devolve a transação e o endereço público
/// (IPv4) que o servidor enxergou. Qualquer coisa malformada vira `None`.
pub(crate) fn parse_binding_response(data: &[u8]) -> Option<(TransactionId, SocketAddr)> {
    if !looks_like_stun(data) || data[..2] != BINDING_SUCCESS {
        return None;
    }
    let declared = u16::from_be_bytes([data[2], data[3]]) as usize;
    let attributes = data.get(20..20 + declared)?;
    let transaction: TransactionId = data[8..20].try_into().ok()?;

    let mut fallback = None;
    let mut rest = attributes;
    while rest.len() >= 4 {
        let kind = u16::from_be_bytes([rest[0], rest[1]]);
        let len = u16::from_be_bytes([rest[2], rest[3]]) as usize;
        let value = rest.get(4..4 + len)?;
        if value.len() >= 8 && value[1] == FAMILY_IPV4 {
            let port = u16::from_be_bytes([value[2], value[3]]);
            let ip = [value[4], value[5], value[6], value[7]];
            match kind {
                ATTR_XOR_MAPPED_ADDRESS => {
                    let port = port ^ u16::from_be_bytes([MAGIC_COOKIE[0], MAGIC_COOKIE[1]]);
                    let ip = Ipv4Addr::new(
                        ip[0] ^ MAGIC_COOKIE[0],
                        ip[1] ^ MAGIC_COOKIE[1],
                        ip[2] ^ MAGIC_COOKIE[2],
                        ip[3] ^ MAGIC_COOKIE[3],
                    );
                    return Some((transaction, SocketAddr::new(IpAddr::V4(ip), port)));
                }
                ATTR_MAPPED_ADDRESS => fallback = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port)),
                _ => {}
            }
        }
        // Atributos são alinhados em 4 bytes.
        rest = rest.get(4 + len.div_ceil(4) * 4..).unwrap_or(&[]);
    }
    fallback.map(|addr| (transaction, addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(attributes: &[u8], transaction: &TransactionId) -> Vec<u8> {
        let mut packet = Vec::new();
        packet.extend_from_slice(&BINDING_SUCCESS);
        packet.extend_from_slice(&(attributes.len() as u16).to_be_bytes());
        packet.extend_from_slice(&MAGIC_COOKIE);
        packet.extend_from_slice(transaction);
        packet.extend_from_slice(attributes);
        packet
    }

    #[test]
    fn request_has_the_fixed_header() {
        let packet = binding_request(&[9; 12]);
        assert!(looks_like_stun(&packet));
        assert_eq!(&packet[..4], &[0, 1, 0, 0]);
        assert_eq!(&packet[8..], &[9; 12]);
    }

    #[test]
    fn parses_xor_mapped_address() {
        // Exemplo da RFC 5769: 192.0.2.1:32853.
        let attr = [0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xA1, 0x47, 0xE1, 0x12, 0xA6, 0x43];
        let (transaction, addr) = parse_binding_response(&response(&attr, &[5; 12])).unwrap();
        assert_eq!(transaction, [5; 12]);
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
    }

    #[test]
    fn skips_unknown_attributes_and_falls_back_to_plain_mapped_address() {
        let mut attrs = vec![0x80, 0x22, 0x00, 0x05, b'a', b'b', b'c', b'd', b'e', 0, 0, 0]; // SOFTWARE, com padding
        attrs.extend_from_slice(&[0x00, 0x01, 0x00, 0x08, 0x00, 0x01, 0x1F, 0x90, 203, 0, 113, 7]);
        let (_, addr) = parse_binding_response(&response(&attrs, &[1; 12])).unwrap();
        assert_eq!(addr, "203.0.113.7:8080".parse().unwrap());
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        let attr = [0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xA1, 0x47, 0xE1, 0x12, 0xA6, 0x43];
        let good = response(&attr, &[5; 12]);
        for cut in 0..good.len() {
            assert!(parse_binding_response(&good[..cut]).is_none(), "corte {cut}");
        }
        assert!(parse_binding_response(&binding_request(&[5; 12])).is_none(), "pedido não é resposta");
        let mut quic_like = good.clone();
        quic_like[0] |= 0x40;
        assert!(parse_binding_response(&quic_like).is_none());
    }
}
