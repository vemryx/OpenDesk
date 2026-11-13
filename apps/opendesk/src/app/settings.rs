//! Tela de configurações: ocupa a área principal no lugar do formulário de
//! conexão, com a coluna "este computador" sempre visível ao lado.
//!
//! Regra da casa: toda opção aqui faz o que diz. O que vale só ao reabrir o
//! app (a porta) avisa isso no próprio texto.

use super::{App, CONTENT_WIDTH, LOGO_GAP, LOGO_HEIGHT, Screen, Tab};
use crate::config::{self, ServerChoice, Settings, ThemeChoice};
use crate::theme::{self, Palette};
use eframe::egui::{self, Align, Layout, RichText, Vec2};

/// Rascunho da aba Rede. Só vale depois de "Aplicar", para não reconectar a
/// cada tecla digitada.
pub(super) struct NetForm {
    choice: ServerChoice,
    url: String,
    relay: String,
    port: String,
    /// Resultado do último "Aplicar": (deu certo, texto).
    message: Option<(bool, String)>,
}

impl NetForm {
    pub(super) fn of(settings: &Settings) -> Self {
        Self {
            choice: settings.rendezvous,
            url: settings.rendezvous_url.clone(),
            relay: settings.relay.clone(),
            port: settings.port.map(|port| port.to_string()).unwrap_or_default(),
            message: None,
        }
    }

    fn differs_from(&self, settings: &Settings) -> bool {
        self.choice != settings.rendezvous
            || self.url.trim() != settings.rendezvous_url
            || self.relay.trim() != settings.relay
            || self.port.trim() != settings.port.map(|port| port.to_string()).unwrap_or_default()
    }
}

impl App {
    pub(super) fn settings_screen(&mut self, ui: &mut egui::Ui, p: &Palette, tab: Tab) {
        // "Voltar" fica onde estava o botão "Configurações": ir e voltar é o
        // mesmo gesto, no mesmo lugar.
        let header = Vec2::new(ui.available_width().min(CONTENT_WIDTH), LOGO_HEIGHT);
        ui.allocate_ui_with_layout(header, Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Voltar").clicked() {
                self.screen = Screen::Home;
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.label(RichText::new("Configurações").font(theme::semibold(18.0)).color(p.text));
            });
        });
        ui.add_space(LOGO_GAP - 14.0);

        let tabs = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 22.0;
                for (item, text) in [
                    (Tab::General, "Geral"),
                    (Tab::Security, "Segurança"),
                    (Tab::Network, "Rede"),
                    (Tab::History, "Histórico"),
                    (Tab::About, "Sobre"),
                ] {
                    if theme::tab(ui, p, text, item == tab).clicked() {
                        self.screen = Screen::Settings(item);
                    }
                }
            })
            .response
            .rect;
        let rule = tabs.left()..=(tabs.left() + ui.available_width().min(CONTENT_WIDTH));
        ui.painter().hline(rule, tabs.bottom() - 0.5, egui::Stroke::new(1.0, p.border));
        ui.add_space(12.0);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(CONTENT_WIDTH));
            ui.spacing_mut().item_spacing.y = 18.0;
            if let Some(problem) = &self.settings_problem {
                ui.label(RichText::new(problem).color(p.danger));
            }
            match tab {
                Tab::General => self.general(ui, p),
                Tab::Security => self.security(ui, p),
                Tab::Network => self.network(ui, p),
                Tab::History => self.history_list(ui, p),
                Tab::About => self.about(ui, p),
            }
        });
    }

    fn general(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut changed = false;
        let system_name = self.system_name.clone();
        theme::setting(
            ui,
            p,
            "Nome deste computador",
            "Como você aparece para quem recebe o seu pedido de acesso.",
            |ui| {
                let field = egui::TextEdit::singleline(&mut self.settings.device_name)
                    .hint_text(system_name)
                    .char_limit(64)
                    .desired_width(220.0);
                changed |= ui.add(field).changed();
            },
        );
        theme::setting(ui, p, "Tema", "", |ui| {
            let label = |choice| match choice {
                ThemeChoice::System => "Igual ao Windows",
                ThemeChoice::Light => "Claro",
                ThemeChoice::Dark => "Escuro",
            };
            egui::ComboBox::from_id_salt("tema").width(160.0).selected_text(label(self.settings.theme)).show_ui(
                ui,
                |ui| {
                    for choice in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
                        changed |= ui.selectable_value(&mut self.settings.theme, choice, label(choice)).changed();
                    }
                },
            );
        });
        let mut autostart = self.autostart;
        theme::setting(
            ui,
            p,
            "Abrir com o Windows",
            "O OpenDesk abre minimizado quando você entra no Windows, e este computador fica acessível sem você precisar abri-lo.",
            |ui| {
                theme::toggle(ui, p, &mut autostart, "Abrir com o Windows");
            },
        );
        if autostart != self.autostart {
            self.set_autostart(autostart);
        }
        if changed {
            self.settings_changed();
        }
    }

    fn security(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut changed = false;
        let settings = &mut self.settings;
        theme::setting(
            ui,
            p,
            "Aceitar pedidos de acesso",
            "Desligado, ninguém consegue pedir acesso a este computador: todo pedido é recusado na hora e fica no histórico.",
            |ui| changed |= theme::toggle(ui, p, &mut settings.accept_incoming, "Aceitar pedidos de acesso").changed(),
        );
        theme::setting(
            ui,
            p,
            "Permitir controle de mouse e teclado",
            "Desligado, quem acessa este computador só vê a tela, mesmo que peça controle.",
            |ui| {
                changed |=
                    theme::toggle(ui, p, &mut settings.allow_control, "Permitir controle de mouse e teclado").changed();
            },
        );
        theme::setting(
            ui,
            p,
            "Bloquear o Windows ao fim do acesso",
            "Quando a última pessoa desconectar, este computador volta para a tela de bloqueio.",
            |ui| {
                changed |=
                    theme::toggle(ui, p, &mut settings.lock_on_end, "Bloquear o Windows ao fim do acesso").changed();
            },
        );
        theme::setting(
            ui,
            p,
            "Sincronizar a área de transferência",
            "Durante um acesso com controle, o texto copiado em um computador pode ser colado no outro.",
            |ui| {
                changed |= theme::toggle(ui, p, &mut settings.allow_clipboard, "Sincronizar a área de transferência")
                    .changed();
            },
        );
        theme::setting(
            ui,
            p,
            "Acesso não supervisionado",
            "Quem souber a senha de acesso entra neste computador sem ninguém aprovar. Ligue só em computadores seus, com uma senha longa que você não usa em nenhum outro lugar.",
            |ui| changed |= theme::toggle(ui, p, &mut settings.unattended, "Acesso não supervisionado").changed(),
        );
        let mut save = false;
        if settings.unattended {
            let (title, help) = if self.access.is_some() {
                (
                    "Trocar a senha de acesso",
                    "Já há uma senha salva. Ela não pode ser lida de volta; para trocar, digite uma nova.",
                )
            } else {
                ("Senha de acesso", "Pelo menos 8 caracteres. O acesso só passa a valer depois de salvar a senha.")
            };
            let long_enough = self.new_password.chars().count() >= opendesk_session::auth::MIN_PASSWORD_LEN;
            theme::setting(ui, p, title, help, |ui| {
                save = ui.add_enabled(long_enough, egui::Button::new("Salvar")).clicked();
                let field = egui::TextEdit::singleline(&mut self.new_password)
                    .password(true)
                    .hint_text("Nova senha")
                    .desired_width(200.0);
                let typed = ui.add(field);
                save |= long_enough && typed.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if typed.changed() {
                    self.password_note = None;
                }
            });
            if let Some((ok, text)) = &self.password_note {
                ui.label(RichText::new(text).small().color(if *ok { p.muted } else { p.danger }));
            }
        } else {
            ui.label(
                theme::muted(
                    "Com o acesso não supervisionado desligado, todo acesso precisa ser aprovado aqui, por quem está na frente do computador.",
                    p,
                )
                .small(),
            );
        }
        if save {
            self.save_password();
        }
        if changed {
            self.settings_changed();
        }
    }

    fn network(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let form = &mut self.net;
        let mut edited = false;
        theme::setting(
            ui,
            p,
            "Servidor de encontro",
            "Apresenta os computadores um ao outro para a conexão por ID. Ele nunca vê a sessão.",
            |ui| {
                let label = |choice| match choice {
                    ServerChoice::Default => "Do projeto OpenDesk",
                    ServerChoice::Custom => "Servidor próprio",
                    ServerChoice::Off => "Nenhum (só por endereço)",
                };
                egui::ComboBox::from_id_salt("encontro").width(200.0).selected_text(label(form.choice)).show_ui(
                    ui,
                    |ui| {
                        for choice in [ServerChoice::Default, ServerChoice::Custom, ServerChoice::Off] {
                            edited |= ui.selectable_value(&mut form.choice, choice, label(choice)).changed();
                        }
                    },
                );
            },
        );
        if form.choice == ServerChoice::Custom {
            theme::setting(ui, p, "Endereço do servidor de encontro", "", |ui| {
                let field = egui::TextEdit::singleline(&mut form.url)
                    .hint_text("wss://encontro.suaempresa.com")
                    .desired_width(300.0);
                edited |= ui.add(field).changed();
            });
        }
        theme::setting(
            ui,
            p,
            "Relay",
            "Usado só quando a conexão direta falha; a sessão continua cifrada de ponta a ponta. Deixe vazio para não usar.",
            |ui| {
                let field = egui::TextEdit::singleline(&mut form.relay)
                    .hint_text("relay.suaempresa.com:47800")
                    .desired_width(240.0);
                edited |= ui.add(field).changed();
            },
        );
        theme::setting(
            ui,
            p,
            "Porta",
            "Porta UDP em que este computador recebe conexões. A mudança vale na próxima vez que o OpenDesk abrir.",
            |ui| {
                let hint = opendesk_proto::DEFAULT_PORT.to_string();
                let field =
                    egui::TextEdit::singleline(&mut form.port).hint_text(hint).char_limit(5).desired_width(80.0);
                edited |= ui.add(field).changed();
            },
        );
        if edited {
            form.message = None;
        }

        let pending = form.differs_from(&self.settings);
        let mut apply = false;
        ui.horizontal(|ui| {
            apply = theme::primary_button(ui, p, "Aplicar", pending, 96.0).clicked();
            if let Some((ok, text)) = &self.net.message {
                ui.label(RichText::new(text).color(if *ok { p.muted } else { p.danger }));
            }
        });
        let overridden: Vec<&str> = ["OPENDESK_RENDEZVOUS", "OPENDESK_RELAY", "OPENDESK_PORT"]
            .into_iter()
            .filter(|name| std::env::var_os(name).is_some())
            .collect();
        if !overridden.is_empty() {
            let text = format!("Definido no ambiente e com prioridade sobre esta tela: {}.", overridden.join(", "));
            ui.label(theme::muted(text, p).small());
        }
        if apply {
            self.apply_network();
        }
    }

    fn apply_network(&mut self) {
        let form = &mut self.net;
        let (url, relay, port_text) = (form.url.trim(), form.relay.trim(), form.port.trim());
        let port = match port_text.parse::<u16>() {
            _ if port_text.is_empty() => None,
            Ok(port) if port != 0 => Some(port),
            _ => {
                form.message = Some((false, "A porta deve ser um número de 1 a 65535.".into()));
                return;
            }
        };
        if form.choice == ServerChoice::Custom && opendesk_net::Rendezvous::new(url).is_err() {
            form.message =
                Some((false, "O endereço do servidor de encontro deve começar com wss:// ou https://.".into()));
            return;
        }
        if !relay.is_empty() && !config::is_host_port(relay) {
            form.message = Some((false, "O relay deve ter a forma servidor:porta.".into()));
            return;
        }

        let port_changed = port != self.settings.port;
        self.settings.rendezvous = form.choice;
        self.settings.rendezvous_url = url.to_string();
        self.settings.relay = relay.to_string();
        self.settings.port = port;
        form.message = Some((
            true,
            if port_changed { "Aplicado. A porta nova vale ao reabrir o OpenDesk." } else { "Aplicado." }.into(),
        ));
        self.settings_changed();
        self.start_presence();
    }

    fn history_list(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.history_view.is_empty() {
            ui.label(theme::muted(
                "Nenhuma sessão registrada ainda. As sessões feitas e recebidas neste computador, e os pedidos recusados, aparecem aqui.",
                p,
            ));
        } else {
            egui::Grid::new("historico").num_columns(4).spacing([18.0, 8.0]).min_row_height(18.0).show(ui, |ui| {
                for entry in &self.history_view {
                    ui.label(theme::muted(config::short_when(&entry.when), p));
                    ui.label(theme::muted(if entry.incoming { "Recebida" } else { "Feita" }, p));
                    ui.label(RichText::new(format!("{}  {}", entry.name, entry.id)).color(p.text));
                    ui.label(theme::muted(entry.summary(), p));
                    ui.end_row();
                }
            });
        }
        ui.horizontal(|ui| {
            if ui.button("Abrir a pasta do histórico").clicked() {
                self.open_data_dir();
            }
            ui.label(theme::muted("O arquivo history.jsonl guarda o registro completo.", p).small());
        });
    }

    fn about(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let fingerprint = self.identity().fingerprint();
        egui::Grid::new("sobre").num_columns(2).spacing([18.0, 8.0]).min_row_height(18.0).show(ui, |ui| {
            theme::detail(ui, p, "Versão", env!("CARGO_PKG_VERSION"));
            theme::detail(ui, p, "Licença", "Apache-2.0, código aberto");
            ui.label(theme::muted("Código-fonte", p));
            ui.hyperlink_to("github.com/vemryx/OpenDesk", "https://github.com/vemryx/OpenDesk");
            ui.end_row();
            theme::detail(ui, p, "ID deste dispositivo", &fingerprint.device_id().to_string());
            theme::detail(ui, p, "Impressão digital", &fingerprint.short());
        });
        if ui.button("Abrir a pasta de dados").on_hover_text(self.data_dir.display().to_string()).clicked() {
            self.open_data_dir();
        }
    }

    /// Define a senha de acesso não supervisionado. Só a forma derivada vai
    /// para o disco; o que foi digitado sai da memória da tela.
    fn save_password(&mut self) {
        let password = std::mem::take(&mut self.new_password);
        let saved = opendesk_session::auth::PasswordAccess::create(&password)
            .and_then(|access| Ok(config::save_access(&self.data_dir, &access).map(|()| access)?));
        self.password_note = Some(match saved {
            Ok(access) => {
                self.access = Some(access);
                self.apply_host_policy();
                if self.settings.accept_incoming {
                    (true, "Senha salva. O acesso não supervisionado está valendo.".into())
                } else {
                    (true, "Senha salva. Ela passa a valer quando \"Aceitar pedidos de acesso\" estiver ligado.".into())
                }
            }
            Err(err) => (false, format!("Não foi possível salvar a senha: {err:#}.")),
        });
    }

    /// Grava as configurações e aplica o que tem efeito imediato.
    fn settings_changed(&mut self) {
        self.settings_problem = self
            .settings
            .save(&self.data_dir)
            .err()
            .map(|err| format!("Não foi possível gravar as configurações: {err}."));
        self.ctx.set_theme(match self.settings.theme {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        });
        let name = super::effective_name(&self.settings, &self.system_name);
        if name != self.device_name {
            self.host.set_device_name(name.clone());
            self.device_name = name;
        }
        self.apply_host_policy();
    }

    fn set_autostart(&mut self, on: bool) {
        let result = if on {
            std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| {
                opendesk_platform::system::set_autostart(Some(&format!("\"{}\" --minimized", exe.display())))
            })
        } else {
            opendesk_platform::system::set_autostart(None)
        };
        match result {
            Ok(()) => {
                self.autostart = on;
                self.settings_problem = None;
            }
            Err(err) => {
                self.settings_problem = Some(format!("Não foi possível mudar a abertura com o Windows: {err:#}."))
            }
        }
    }

    fn open_data_dir(&mut self) {
        let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
        if let Err(err) = std::process::Command::new(opener).arg(&self.data_dir).spawn() {
            self.settings_problem = Some(format!("Não foi possível abrir a pasta: {err}."));
        }
    }
}
