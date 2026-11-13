//! O que o aplicativo guarda em disco, na pasta de dados do usuário:
//!
//! * `settings.json` — as configurações;
//! * `recent.json` — os computadores acessados por último;
//! * `history.jsonl` — o registro de sessões, uma linha JSON por evento, só
//!   acrescentado (serve de trilha de auditoria e é fácil de coletar).
//!
//! Tudo é texto legível, sem segredos: a chave do dispositivo fica à parte.

use opendesk_session::auth::PasswordAccess;
use serde::{Deserialize, Serialize};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerChoice {
    /// O servidor de encontro mantido pelo projeto.
    #[default]
    Default,
    /// Um servidor próprio, em `rendezvous_url`.
    Custom,
    /// Sem servidor: só conexão por endereço.
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Vazio usa o nome do computador no sistema.
    pub device_name: String,
    pub theme: ThemeChoice,
    /// Desligado, todo pedido de acesso é recusado sem perguntar.
    pub accept_incoming: bool,
    /// Desligado, quem acessa este computador só vê a tela.
    pub allow_control: bool,
    /// Bloqueia o sistema quando o último acesso a este computador termina.
    pub lock_on_end: bool,
    /// Texto copiado atravessa a sessão (só em acessos com controle).
    pub allow_clipboard: bool,
    /// Arquivos podem ser trocados (só em acessos com controle).
    pub allow_files: bool,
    /// Acesso não supervisionado: quem souber a senha entra sem aprovação.
    /// Só vale com uma senha salva (ver [`load_access`]).
    pub unattended: bool,
    pub rendezvous: ServerChoice,
    pub rendezvous_url: String,
    /// `host:porta` de um relay; vazio para não usar.
    pub relay: String,
    /// Porta de escuta; `None` usa a padrão.
    pub port: Option<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: String::new(),
            theme: ThemeChoice::System,
            accept_incoming: true,
            allow_control: true,
            lock_on_end: false,
            allow_clipboard: true,
            allow_files: true,
            unattended: false,
            rendezvous: ServerChoice::Default,
            rendezvous_url: String::new(),
            relay: String::new(),
            port: None,
        }
    }
}

impl Settings {
    /// Arquivo ausente ou ilegível vale como configuração padrão: o app nunca
    /// deixa de abrir por causa dele.
    pub fn load(dir: &Path) -> Self {
        match std::fs::read_to_string(dir.join("settings.json")) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!("settings.json ilegível, usando o padrão: {err}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        write_atomic(&dir.join("settings.json"), &serde_json::to_string_pretty(self)?)
    }

    /// Servidor de encontro a usar. A variável `OPENDESK_RENDEZVOUS`, quando
    /// definida, tem prioridade (é como testes e implantações o fixam).
    pub fn rendezvous_url(&self) -> Option<String> {
        if std::env::var_os("OPENDESK_RENDEZVOUS").is_some() {
            return opendesk_net::rendezvous_url();
        }
        match self.rendezvous {
            ServerChoice::Default => opendesk_net::DEFAULT_RENDEZVOUS.map(str::to_string),
            ServerChoice::Custom => Some(self.rendezvous_url.trim().to_string()).filter(|url| !url.is_empty()),
            ServerChoice::Off => None,
        }
    }

    /// Relay a usar; `OPENDESK_RELAY` tem prioridade.
    pub fn relay(&self) -> Option<String> {
        opendesk_net::relay_addr().or_else(|| Some(self.relay.trim().to_string()).filter(|relay| !relay.is_empty()))
    }

    /// Porta de escuta; `OPENDESK_PORT` tem prioridade.
    pub fn port(&self) -> u16 {
        std::env::var("OPENDESK_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .or(self.port)
            .unwrap_or(opendesk_proto::DEFAULT_PORT)
    }
}

/// Confere se `text` tem a forma `host:porta`.
pub fn is_host_port(text: &str) -> bool {
    text.rsplit_once(':')
        .is_some_and(|(host, port)| !host.trim().is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0))
}

/// `2026-11-12 22:41` vira `12/11 22:41`, para as listas.
pub fn short_when(when: &str) -> String {
    match (when.get(5..7), when.get(8..10), when.get(11..16)) {
        (Some(month), Some(day), Some(time)) => format!("{day}/{month} {time}"),
        _ => when.to_string(),
    }
}

/// A senha de acesso não supervisionado, na forma em que o host a guarda (sal
/// e senha esticada; nunca a senha digitada). Fica num arquivo próprio, fora
/// das configurações, porque quem o copia consegue entrar neste computador.
pub fn load_access(dir: &Path) -> Option<PasswordAccess> {
    let text = std::fs::read_to_string(dir.join("unattended.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_access(dir: &Path, access: &PasswordAccess) -> io::Result<()> {
    write_atomic(&dir.join("unattended.json"), &serde_json::to_string(access)?)
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

// ───────────────────────────── Recentes ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recent {
    /// O que se digita para conectar de novo: o ID ou o endereço.
    pub target: String,
    /// O nome com que o computador se apresentou.
    pub name: String,
    pub last: String,
}

/// Os computadores acessados por último, do mais recente para o mais antigo.
pub struct Recents {
    path: Option<PathBuf>,
    pub items: Vec<Recent>,
}

impl Recents {
    const LIMIT: usize = 8;

    pub fn load(dir: &Path) -> Self {
        let path = dir.join("recent.json");
        let items = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok());
        Self { path: Some(path), items: items.unwrap_or_default() }
    }

    /// Lista só em memória, para a pré-visualização em builds de desenvolvimento.
    #[cfg(debug_assertions)]
    pub fn in_memory(items: Vec<Recent>) -> Self {
        Self { path: None, items }
    }

    /// Põe `target` no topo, atualizando nome e data.
    pub fn touch(&mut self, target: &str, name: &str, when: String) {
        self.items.retain(|item| item.target != target);
        self.items.insert(0, Recent { target: target.to_string(), name: name.to_string(), last: when });
        self.items.truncate(Self::LIMIT);
        self.save();
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let result = serde_json::to_string_pretty(&self.items)
            .map_err(io::Error::from)
            .and_then(|text| write_atomic(path, &text));
        if let Err(err) = result {
            tracing::warn!("não foi possível gravar recent.json: {err}");
        }
    }
}

// ───────────────────────────── Histórico ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Uma sessão que aconteceu.
    Session,
    /// Pedido recusado por quem estava no computador.
    Refused,
    /// Pedido recusado sozinho, porque a entrada está desligada.
    Blocked,
    /// Senha de acesso não supervisionado errada.
    BadPassword,
    /// Um arquivo trocado durante uma sessão.
    File,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub when: String,
    /// `true`: alguém acessou este computador. `false`: este acessou outro.
    pub incoming: bool,
    pub kind: Kind,
    pub name: String,
    /// ID do outro dispositivo (ou o endereço, quando não há ID).
    pub id: String,
    pub control: bool,
    pub seconds: u64,
    /// A sessão foi liberada pela senha, sem ninguém aprovar.
    #[serde(default)]
    pub password: bool,
    /// Nome do arquivo, quando o evento é uma transferência. Nesse caso
    /// `incoming` diz se o arquivo chegou a este computador.
    #[serde(default)]
    pub file: String,
}

impl Entry {
    /// Resumo do que aconteceu, para a lista do histórico.
    pub fn summary(&self) -> String {
        match self.kind {
            Kind::Refused => "Recusado".into(),
            Kind::Blocked => "Bloqueado".into(),
            Kind::BadPassword => "Senha errada".into(),
            Kind::File if self.incoming => format!("Recebeu {}", self.file),
            Kind::File => format!("Enviou {}", self.file),
            Kind::Session => {
                let mode = if self.control { "Controle" } else { "Só ver" };
                let how = if self.password { " por senha" } else { "" };
                format!("{mode}{how}, {}", duration(self.seconds))
            }
        }
    }
}

/// Tamanho de arquivo legível: `512 B`, `1,5 MB`, `2,0 GB`.
pub fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (mut value, mut unit) = (bytes as f64 / 1024.0, 0);
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit]).replace('.', ",")
}

fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3_600 => format!("{} min", seconds / 60),
        _ => format!("{} h {:02} min", seconds / 3_600, seconds % 3_600 / 60),
    }
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("history.jsonl") }
    }

    pub fn append(&self, entry: &Entry) {
        let result = serde_json::to_string(entry).map_err(io::Error::from).and_then(|line| {
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
            writeln!(file, "{line}")
        });
        if let Err(err) = result {
            tracing::warn!("não foi possível gravar o histórico: {err}");
        }
    }

    /// Os `limit` eventos mais recentes, do mais novo para o mais antigo.
    /// Linhas ilegíveis são ignoradas.
    pub fn latest(&self, limit: usize) -> Vec<Entry> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        text.lines().rev().filter_map(|line| serde_json::from_str(line).ok()).take(limit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("opendesk-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_round_trip_and_tolerate_old_files() {
        let dir = temp_dir("settings");
        assert_eq!(Settings::load(&dir), Settings::default());

        let settings =
            Settings { device_name: "Recepção".into(), allow_control: false, port: Some(5000), ..Default::default() };
        settings.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), settings);

        // Um arquivo de versão antiga, sem os campos novos, mantém o padrão neles.
        std::fs::write(dir.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        let loaded = Settings::load(&dir);
        assert_eq!((loaded.theme, loaded.accept_incoming, loaded.allow_control), (ThemeChoice::Dark, true, true));

        // Arquivo corrompido não impede o app de abrir.
        std::fs::write(dir.join("settings.json"), "{ isto não é json").unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recents_keep_latest_first_without_duplicates() {
        let dir = temp_dir("recents");
        let mut recents = Recents::load(&dir);
        for index in 0..10 {
            recents.touch(&format!("id-{index}"), "PC", "agora".into());
        }
        assert_eq!(recents.items.len(), Recents::LIMIT);
        assert_eq!(recents.items[0].target, "id-9");

        recents.touch("id-5", "PC renomeado", "depois".into());
        assert_eq!((recents.items[0].target.as_str(), recents.items[0].name.as_str()), ("id-5", "PC renomeado"));
        assert_eq!(recents.items.iter().filter(|item| item.target == "id-5").count(), 1);

        recents.remove(0);
        assert_eq!(Recents::load(&dir).items, recents.items);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_appends_and_reads_newest_first() {
        let dir = temp_dir("history");
        let history = History::new(&dir);
        assert!(history.latest(10).is_empty());
        let entry = |name: &str, kind, seconds| Entry {
            when: "2026-11-12 22:41".into(),
            incoming: true,
            kind,
            name: name.into(),
            id: "602 758 973".into(),
            control: true,
            seconds,
            password: false,
            file: String::new(),
        };
        history.append(&entry("A", Kind::Session, 42));
        history.append(&entry("B", Kind::Refused, 0));
        history.append(&entry("C", Kind::Session, 3_900));

        let latest = history.latest(2);
        assert_eq!(latest.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["C", "B"]);
        assert_eq!(latest[0].summary(), "Controle, 1 h 05 min");
        assert_eq!(latest[1].summary(), "Recusado");
        assert_eq!(history.latest(10)[2].summary(), "Controle, 42 s");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_history_lines_still_load_and_password_sessions_say_so() {
        // Linha gravada antes de existir o campo `password`.
        let old = r#"{"when":"2026-11-12 22:41","incoming":true,"kind":"session","name":"A","id":"1","control":false,"seconds":90}"#;
        let entry: Entry = serde_json::from_str(old).unwrap();
        assert_eq!(entry.summary(), "Só ver, 1 min");
        let by_password = Entry { password: true, control: true, ..entry };
        assert_eq!(by_password.summary(), "Controle por senha, 1 min");
        let file = Entry { kind: Kind::File, file: "nota.pdf".into(), ..by_password };
        assert_eq!(file.summary(), "Recebeu nota.pdf");
        assert_eq!(Entry { incoming: false, ..file }.summary(), "Enviou nota.pdf");
    }

    #[test]
    fn password_access_round_trips_through_its_own_file() {
        let dir = temp_dir("access");
        assert!(load_access(&dir).is_none());
        let access = PasswordAccess::create("uma senha bem comprida").unwrap();
        save_access(&dir, &access).unwrap();
        assert_eq!(load_access(&dir), Some(access));
        let on_disk = std::fs::read_to_string(dir.join("unattended.json")).unwrap();
        assert!(!on_disk.contains("comprida"), "a senha digitada não pode ir para o disco");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_sizes_read_naturally() {
        assert_eq!(file_size(0), "0 B");
        assert_eq!(file_size(1023), "1023 B");
        assert_eq!(file_size(1536), "1,5 kB");
        assert_eq!(file_size(300_000), "293,0 kB");
        assert_eq!(file_size(5 * 1024 * 1024), "5,0 MB");
        assert_eq!(file_size(3 * 1024 * 1024 * 1024), "3,0 GB");
    }

    #[test]
    fn short_when_is_day_month_time() {
        assert_eq!(short_when("2026-11-12 22:41"), "12/11 22:41");
        assert_eq!(short_when("2026-11-12 22:41 UTC"), "12/11 22:41");
        assert_eq!(short_when("agora"), "agora");
    }

    #[test]
    fn host_port_shape() {
        assert!(is_host_port("relay.exemplo.com:47800"));
        assert!(is_host_port("10.0.0.5:1"));
        for bad in ["relay.exemplo.com", ":47800", "host:0", "host:porta", "host:70000", ""] {
            assert!(!is_host_port(bad), "{bad}");
        }
    }
}
