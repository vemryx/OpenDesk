//! Hosts conhecidos: a impressão digital que cada ID apresentou da primeira
//! vez, no estilo do `known_hosts` do SSH.
//!
//! Um ID tem só 9 dígitos, então sozinho não identifica ninguém com
//! segurança: quem controla o servidor de encontro, ou fabrica um certificado
//! que dê o mesmo ID, poderia se passar pelo computador de destino. Fixar a
//! impressão digital no primeiro contato fecha essa porta para todas as
//! conexões seguintes: se ela mudar, a conexão é recusada com um aviso.

use crate::identity::{DeviceId, Fingerprint};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE: &str = "known_hosts";

pub struct KnownHosts {
    path: Option<PathBuf>,
    entries: Mutex<HashMap<u32, Fingerprint>>,
}

impl KnownHosts {
    /// Carrega de `dir`. Um arquivo ausente ou ilegível vira uma lista vazia.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let entries = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (id, fingerprint) = line.split_once(' ')?;
                Some((DeviceId::parse(id)?.0, Fingerprint::from_hex(fingerprint.trim())?))
            })
            .collect();
        Self { path: Some(path), entries: Mutex::new(entries) }
    }

    /// Lista só em memória, para testes.
    pub fn in_memory() -> Self {
        Self { path: None, entries: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, id: DeviceId) -> Option<Fingerprint> {
        self.entries.lock().unwrap().get(&id.0).copied()
    }

    /// Guarda a impressão digital de `id`. Não troca uma já guardada: para
    /// isso é preciso [`Self::forget`] antes, de propósito.
    pub fn remember(&self, id: DeviceId, fingerprint: Fingerprint) -> io::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&id.0) {
            return Ok(());
        }
        entries.insert(id.0, fingerprint);
        self.save(&entries)
    }

    pub fn forget(&self, id: DeviceId) -> io::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        if entries.remove(&id.0).is_none() {
            return Ok(());
        }
        self.save(&entries)
    }

    fn save(&self, entries: &HashMap<u32, Fingerprint>) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut lines: Vec<String> = entries.iter().map(|(id, fp)| format!("{id} {}\n", fp.hex())).collect();
        lines.sort();
        std::fs::write(path, lines.concat())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_first_fingerprint_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!("opendesk-known-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (id, first, second) = (DeviceId(602_758_973), Fingerprint([1; 32]), Fingerprint([2; 32]));

        let known = KnownHosts::load(&dir);
        assert_eq!(known.get(id), None);
        known.remember(id, first).unwrap();
        // Uma segunda identidade para o mesmo ID nunca substitui a primeira.
        known.remember(id, second).unwrap();
        assert_eq!(known.get(id), Some(first));

        assert_eq!(KnownHosts::load(&dir).get(id), Some(first));

        known.forget(id).unwrap();
        assert_eq!(KnownHosts::load(&dir).get(id), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_lines_are_ignored() {
        let dir = std::env::temp_dir().join(format!("opendesk-known-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = Fingerprint([9; 32]);
        std::fs::write(dir.join(FILE), format!("lixo\n123 abc\n602758973 {}\n", good.hex())).unwrap();
        let known = KnownHosts::load(&dir);
        assert_eq!(known.get(DeviceId(602_758_973)), Some(good));
        assert_eq!(known.entries.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
