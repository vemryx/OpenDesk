//! Transferência de arquivos durante a sessão.
//!
//! Cada arquivo viaja num stream QUIC bidirecional próprio, aberto por quem
//! envia: não trava o controle nem o vídeo, e herda a cifragem de ponta a
//! ponta da sessão.
//!
//! ```text
//! quem envia                      quem recebe
//!   Offer { nome, tamanho }  ──▶
//!                            ◀──  Accept | Refuse
//!   bytes…  (fim do stream)  ──▶
//!                            ◀──  Saved | Failed
//! ```
//!
//! Quem recebe nunca confia no nome: tira qualquer caminho, troca caracteres
//! proibidos e grava só dentro da pasta de recebidos, sem sobrescrever nada.
//! O arquivo só aparece com o nome final depois de chegar inteiro.

use anyhow::{Context, Result, bail};
use opendesk_proto::wire::{recv_msg, send_msg};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maior arquivo aceito.
pub const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
/// Intervalo mínimo entre dois avisos de progresso.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);
/// Sufixo do arquivo enquanto ainda está chegando.
const PARTIAL: &str = ".opendesk-recebendo";

#[derive(Debug, Serialize, Deserialize)]
struct Offer {
    name: String,
    size: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Answer {
    Accept,
    Refuse,
    Saved,
    Failed,
}

/// O que aconteceu com um arquivo, para a interface acompanhar.
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// Identifica a transferência dentro da sessão.
    pub id: u64,
    /// `true`: está chegando neste computador.
    pub incoming: bool,
    pub name: String,
    pub size: u64,
    pub state: FileState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// O outro lado quer enviar; aguarda a resposta daqui.
    Offered,
    /// Bytes transferidos até agora.
    Progress(u64),
    /// Terminou. Para um arquivo recebido, é onde ele foi gravado.
    Done(PathBuf),
    /// O outro lado não quis receber.
    Refused,
    Failed(String),
}

/// Nome e tamanho do arquivo a enviar.
pub(crate) fn describe(path: &Path) -> Result<(String, u64)> {
    let metadata = std::fs::metadata(path).with_context(|| format!("abrir {}", path.display()))?;
    if !metadata.is_file() {
        bail!("só é possível enviar arquivos, não pastas");
    }
    if metadata.len() > MAX_FILE_SIZE {
        bail!("o arquivo passa do limite de {} GB", MAX_FILE_SIZE >> 30);
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    Ok((name, metadata.len()))
}

pub(crate) enum Sent {
    Delivered,
    Refused,
}

/// Envia `path` por um stream novo de `conn`.
pub(crate) async fn send(
    conn: &quinn::Connection,
    path: &Path,
    name: &str,
    size: u64,
    mut progress: impl FnMut(u64),
) -> Result<Sent> {
    let mut file = tokio::fs::File::open(path).await.context("abrir o arquivo")?;
    let (mut tx, mut rx) = conn.open_bi().await.context("abrir o canal do arquivo")?;
    send_msg(&mut tx, &Offer { name: name.to_string(), size }).await?;
    if recv_msg::<_, Answer>(&mut rx).await.context("aguardar a resposta")? != Answer::Accept {
        return Ok(Sent::Refused);
    }

    let mut buffer = vec![0u8; CHUNK];
    let (mut sent, mut last) = (0u64, Instant::now());
    // Nunca manda mais do que anunciou, mesmo que o arquivo cresça no meio.
    while sent < size {
        let want = CHUNK.min((size - sent) as usize);
        let read = file.read(&mut buffer[..want]).await.context("ler o arquivo")?;
        if read == 0 {
            bail!("o arquivo mudou de tamanho durante o envio");
        }
        tx.write_all(&buffer[..read]).await.context("enviar o arquivo")?;
        sent += read as u64;
        if last.elapsed() >= PROGRESS_EVERY {
            progress(sent);
            last = Instant::now();
        }
    }
    tx.finish().context("concluir o envio")?;
    match recv_msg::<_, Answer>(&mut rx).await {
        Ok(Answer::Saved) => Ok(Sent::Delivered),
        _ => bail!("o outro computador não conseguiu gravar o arquivo"),
    }
}

/// Lê a oferta que abre um stream de arquivo. O nome já vem saneado.
pub(crate) async fn read_offer(rx: &mut quinn::RecvStream) -> Result<(String, u64)> {
    let offer: Offer = recv_msg(rx).await?;
    if offer.size > MAX_FILE_SIZE {
        bail!("arquivo grande demais");
    }
    Ok((sanitize(&offer.name), offer.size))
}

pub(crate) async fn refuse(tx: &mut quinn::SendStream) {
    let _ = send_msg(tx, &Answer::Refuse).await;
    let _ = tx.finish();
}

/// Aceita a oferta e grava o arquivo em `dir`. Devolve onde ele ficou.
pub(crate) async fn save(
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
    dir: &Path,
    name: &str,
    size: u64,
    progress: impl FnMut(u64),
) -> Result<PathBuf> {
    let result = write_to_disk(tx, rx, dir, name, size, progress).await;
    let _ = send_msg(tx, if result.is_ok() { &Answer::Saved } else { &Answer::Failed }).await;
    let _ = tx.finish();
    result
}

async fn write_to_disk(
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
    dir: &Path,
    name: &str,
    size: u64,
    mut progress: impl FnMut(u64),
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await.with_context(|| format!("criar {}", dir.display()))?;
    let partial = unique(dir, &format!("{name}{PARTIAL}"));
    let mut file = tokio::fs::File::create(&partial).await.context("criar o arquivo")?;
    send_msg(tx, &Answer::Accept).await?;

    let received = async {
        let mut buffer = vec![0u8; CHUNK];
        let (mut got, mut last) = (0u64, Instant::now());
        while let Some(read) = rx.read(&mut buffer).await.context("receber o arquivo")? {
            got += read as u64;
            if got > size {
                bail!("chegou mais do que o tamanho anunciado");
            }
            file.write_all(&buffer[..read]).await.context("gravar o arquivo")?;
            if last.elapsed() >= PROGRESS_EVERY {
                progress(got);
                last = Instant::now();
            }
        }
        if got != size {
            bail!("a transferência foi interrompida");
        }
        file.flush().await.context("gravar o arquivo")?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(err) = received {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(err);
    }
    // O nome final só é escolhido agora, para não colidir com outro arquivo
    // que tenha terminado enquanto este chegava.
    let path = unique(dir, name);
    tokio::fs::rename(&partial, &path).await.context("finalizar o arquivo")?;
    Ok(path)
}

/// Deixa só um nome de arquivo seguro: sem caminho, sem caracteres que o
/// Windows proíbe, sem nomes reservados e de tamanho razoável.
pub(crate) fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String =
        base.chars().map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c }).take(150).collect();
    let clean = clean.trim().trim_end_matches(['.', ' ']);
    if clean.is_empty() {
        return "arquivo".into();
    }
    let stem = clean.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let numbered = |prefix: &str| stem.strip_prefix(prefix).is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']));
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT") {
        return format!("_{clean}");
    }
    clean.to_string()
}

/// Um caminho em `dir` que ainda não existe: `nome.ext`, `nome (1).ext`, …
fn unique(dir: &Path, name: &str) -> PathBuf {
    let direct = dir.join(name);
    if !direct.exists() {
        return direct;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){extension}"))).find(|path| !path.exists()).expect("sempre há um livre")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_the_network_cannot_escape_the_folder() {
        assert_eq!(sanitize("relatório final.pdf"), "relatório final.pdf");
        assert_eq!(sanitize("..\\..\\Windows\\System32\\drivers\\etc\\hosts"), "hosts");
        assert_eq!(sanitize("/etc/passwd"), "passwd");
        assert_eq!(sanitize("C:\\Users\\alguem\\startup.bat"), "startup.bat");
        assert_eq!(sanitize(".."), "arquivo");
        assert_eq!(sanitize("pasta/"), "arquivo");
        assert_eq!(sanitize(""), "arquivo");
    }

    #[test]
    fn forbidden_characters_and_reserved_names_are_neutralized() {
        assert_eq!(sanitize("a<b>c:d\"e|f?g*h.txt"), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(sanitize("quebra\nde linha.txt"), "quebra_de linha.txt");
        assert_eq!(sanitize("nome. . ."), "nome");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul.txt"), "_nul.txt");
        assert_eq!(sanitize("COM1.log"), "_COM1.log");
        assert_eq!(sanitize("COMUM.txt"), "COMUM.txt");
        assert_eq!(sanitize(&"x".repeat(400)).chars().count(), 150);
    }

    #[test]
    fn unique_never_points_at_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("opendesk-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique(&dir, "foto.png"), dir.join("foto.png"));
        std::fs::write(dir.join("foto.png"), b"1").unwrap();
        assert_eq!(unique(&dir, "foto.png"), dir.join("foto (1).png"));
        std::fs::write(dir.join("foto (1).png"), b"2").unwrap();
        assert_eq!(unique(&dir, "foto.png"), dir.join("foto (2).png"));
        std::fs::write(dir.join("LEIAME"), b"3").unwrap();
        assert_eq!(unique(&dir, "LEIAME"), dir.join("LEIAME (1)"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
