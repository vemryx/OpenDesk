// Identidade de dispositivo, do lado do servidor.
//
// Tem de produzir exatamente os mesmos valores que
// `crates/opendesk-net/src/identity.rs`: a impressão digital é o SHA-256 do
// certificado, e o ID é derivado dela.

/** Quantos IDs alternativos um dispositivo pode tentar se o primeiro colidir. */
export const MAX_ID_ATTEMPT = 15;

export function hex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

export function unhex(text: unknown, maxBytes: number): Uint8Array | null {
  if (typeof text !== "string" || text.length % 2 !== 0 || text.length > maxBytes * 2) return null;
  if (!/^[0-9a-f]*$/.test(text)) return null;
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(text.slice(i * 2, i * 2 + 2), 16);
  return out;
}

async function sha256(bytes: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

/**
 * ID de 9 dígitos para a tentativa `attempt`. A tentativa 0 usa os primeiros
 * 8 bytes da impressão digital; as seguintes, os de SHA-256(impressão ‖ n).
 */
export async function deviceId(fingerprint: Uint8Array, attempt: number): Promise<string> {
  let source = fingerprint;
  if (attempt > 0) {
    const seeded = new Uint8Array(fingerprint.length + 1);
    seeded.set(fingerprint);
    seeded[fingerprint.length] = attempt;
    source = await sha256(seeded);
  }
  const n = new DataView(source.buffer, source.byteOffset, 8).getBigUint64(0, false);
  return (100_000_000n + (n % 900_000_000n)).toString();
}

interface Tlv {
  tag: number;
  /** Início do elemento inteiro (cabeçalho incluído). */
  start: number;
  contentStart: number;
  end: number;
}

function readTlv(der: Uint8Array, at: number): Tlv {
  const tag = der[at];
  const first = der[at + 1];
  if (tag === undefined || first === undefined) throw new Error("DER truncado");
  let length = first;
  let contentStart = at + 2;
  if (first & 0x80) {
    const count = first & 0x7f;
    if (count === 0 || count > 3) throw new Error("comprimento DER inválido");
    length = 0;
    for (let i = 0; i < count; i++) {
      const byte = der[contentStart + i];
      if (byte === undefined) throw new Error("DER truncado");
      length = (length << 8) | byte;
    }
    contentStart += count;
  }
  const end = contentStart + length;
  if (end > der.length) throw new Error("DER truncado");
  return { tag, start: at, contentStart, end };
}

/**
 * Extrai o SubjectPublicKeyInfo de um certificado X.509:
 * Certificate → tbsCertificate → [versão] serial, algoritmo, emissor,
 * validade, sujeito, **subjectPublicKeyInfo**.
 */
export function spkiFromCertificate(der: Uint8Array): Uint8Array {
  const certificate = readTlv(der, 0);
  const tbs = readTlv(der, certificate.contentStart);
  let at = tbs.contentStart;
  let field = readTlv(der, at);
  if (field.tag === 0xa0) {
    at = field.end;
    field = readTlv(der, at);
  }
  // `field` é o número de série; pula serial, algoritmo, emissor, validade e sujeito.
  for (let skipped = 0; skipped < 5; skipped++) {
    at = field.end;
    field = readTlv(der, at);
  }
  if (field.tag !== 0x30 || field.end > tbs.end) throw new Error("certificado sem chave pública");
  return der.slice(field.start, field.end);
}

/** Texto que o dispositivo assina para provar que possui a chave do certificado. */
export function registrationMessage(id: string, nonce: string): Uint8Array {
  return new TextEncoder().encode(`opendesk-rendezvous-v1:register:${id}:${nonce}`);
}

export type Verified = { ok: true; fingerprint: string } | { ok: false; code: string };

/**
 * Confere um registro: o certificado dá aquele ID, e a assinatura (ECDSA
 * P-256 / SHA-256, formato r‖s) sobre o desafio foi feita com a chave dele.
 */
export async function verifyRegistration(
  id: string,
  nonce: string,
  message: { cert?: unknown; attempt?: unknown; sig?: unknown },
): Promise<Verified> {
  const cert = unhex(message.cert, 4096);
  const signature = unhex(message.sig, 64);
  const attempt = message.attempt;
  if (!cert || !signature || signature.length !== 64) return { ok: false, code: "bad_request" };
  if (typeof attempt !== "number" || !Number.isInteger(attempt) || attempt < 0 || attempt > MAX_ID_ATTEMPT) {
    return { ok: false, code: "bad_request" };
  }

  const fingerprint = await sha256(cert);
  if ((await deviceId(fingerprint, attempt)) !== id) return { ok: false, code: "id_mismatch" };

  try {
    const key = await crypto.subtle.importKey(
      "spki",
      spkiFromCertificate(cert),
      { name: "ECDSA", namedCurve: "P-256" },
      false,
      ["verify"],
    );
    const valid = await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      key,
      signature,
      registrationMessage(id, nonce),
    );
    return valid ? { ok: true, fingerprint: hex(fingerprint) } : { ok: false, code: "bad_signature" };
  } catch {
    return { ok: false, code: "bad_certificate" };
  }
}
