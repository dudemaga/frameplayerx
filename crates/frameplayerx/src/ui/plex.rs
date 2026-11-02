//! Plex source setup: account linking, server choice and a tested manual token.

use super::sources::{address_hint, name_hint, text_field};
use super::theme::{self, Weight};
use super::widgets::{self, Kind};
use super::{Action, SourceForm};
use fp_sources::PlexConfig;
use fp_sources::plex::auth::{LINK_URL, PlexCredentials, PlexPin, PlexResource};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub enum LinkState {
    #[default]
    Idle,
    Requesting,
    Waiting {
        pin: PlexPin,
        polled: Instant,
        polling: bool,
    },
    Servers {
        credentials: PlexCredentials,
        servers: Vec<PlexResource>,
    },
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionMode {
    #[default]
    Local,
    Direct,
    Relay,
}

impl ConnectionMode {
    pub(super) fn from_config(config: &PlexConfig) -> Self {
        if config.local_only {
            Self::Local
        } else if config.allow_relay {
            Self::Relay
        } else {
            Self::Direct
        }
    }

    fn apply(self, config: &mut PlexConfig) {
        config.local_only = self == Self::Local;
        config.allow_relay = self == Self::Relay;
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => tr!("plex-local-only"),
            Self::Direct => tr!("plex-connection-direct"),
            Self::Relay => tr!("plex-connection-relay"),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Local => tr!("plex-connection-local-desc"),
            Self::Direct => tr!("plex-connection-direct-desc"),
            Self::Relay => tr!("plex-connection-relay-desc"),
        }
    }
}

pub struct Connect {
    pub generation: u64,
    pub state: LinkState,
    pub manual: bool,
    pub token: String,
    pub connection: ConnectionMode,
    pub selected: Option<PlexConfig>,
}

impl Default for Connect {
    fn default() -> Self {
        Self {
            generation: 0,
            state: LinkState::Idle,
            manual: false,
            token: String::new(),
            connection: ConnectionMode::default(),
            selected: None,
        }
    }
}

impl Connect {
    /// Recheck an already chosen server before saving a different policy.
    fn set_connection(&mut self, connection: ConnectionMode) -> Option<PlexConfig> {
        if self.connection == connection {
            return None;
        }
        self.connection = connection;
        let mut config = self.selected.clone()?;
        connection.apply(&mut config);
        self.state = LinkState::Connecting;
        Some(config)
    }

    /// Invalidates any outstanding job before changing how to sign in.
    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.state = LinkState::Idle;
        self.selected = None;
    }

    fn use_token(&mut self, manual: bool) {
        if self.manual != manual {
            self.reset();
            self.manual = manual;
        }
    }
}

fn link_rows(r: &mut widgets::Rows, f: &mut SourceForm, actions: &mut Vec<Action>) {
    match f.plex.state.clone() {
        LinkState::Waiting { pin, .. } => {
            r.row_with(
                tr!("quick-connect-code"),
                Some(tr!("plex-link-waiting")),
                |ui| {
                    ui.label(
                        egui::RichText::new(LINK_URL.trim_start_matches("https://"))
                            .font(theme::font(Weight::Bold, 28.0))
                            .color(theme::ACCENT),
                    );
                },
                |ui| {
                    if widgets::button_sized(ui, None, tr!("common-cancel"), Kind::Secondary, 36.0)
                        .clicked()
                    {
                        f.plex.reset();
                    }
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(
                            pin.code
                                .chars()
                                .map(|c| c.to_string())
                                .collect::<Vec<_>>()
                                .join(" "),
                        )
                        .font(theme::font(Weight::Bold, 32.0))
                        .color(theme::ACCENT),
                    );
                },
            );
        }
        LinkState::Idle | LinkState::Requesting | LinkState::Failed(_) => {
            let requesting = matches!(f.plex.state, LinkState::Requesting);
            r.row(tr!("plex-sign-in"), Some(tr!("plex-link-desc")), |ui| {
                if ui
                    .add_enabled_ui(!requesting, |ui| {
                        widgets::button(
                            ui,
                            Some(super::icons::KEY_RETURN),
                            tr!("quick-connect-get-code"),
                            Kind::Primary,
                        )
                    })
                    .inner
                    .clicked()
                {
                    f.plex.selected = None;
                    f.plex.state = LinkState::Requesting;
                    actions.push(Action::PlexLinkStart);
                }
                if requesting {
                    ui.spinner();
                }
            });
        }
        LinkState::Connected | LinkState::Servers { .. } => {
            r.row(
                tr!("plex-sign-in"),
                matches!(f.plex.state, LinkState::Connected)
                    .then_some(tr!("quick-connect-authorized-desc")),
                |ui| {
                    if widgets::button_sized(ui, None, tr!("common-clear"), Kind::Secondary, 36.0)
                        .clicked()
                    {
                        f.plex.reset();
                    }
                    ui.label(egui::RichText::new(super::icons::CHECK_CIRCLE).color(theme::OK));
                },
            );
        }
        LinkState::Connecting => {}
    }
}

/// The selected config is returned only after a worker has tested access.
pub fn form(
    ui: &mut egui::Ui,
    f: &mut SourceForm,
    id: String,
    actions: &mut Vec<Action>,
) -> Option<fp_sources::SourceConfig> {
    if let LinkState::Waiting {
        pin,
        polled,
        polling,
    } = &mut f.plex.state
    {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        if pin.expired() {
            f.plex.state = LinkState::Failed(tr!("plex-link-expired").into());
        } else if !*polling && polled.elapsed() >= Duration::from_secs(2) {
            *polling = true;
            *polled = Instant::now();
            actions.push(Action::PlexLinkPoll {
                generation: f.plex.generation,
                pin: pin.clone(),
            });
        }
    }
    widgets::rows(ui, |r| {
        r.row(
            tr!("source-name"),
            Some(tr!("source-name-desc-server")),
            |ui| text_field(ui, &mut f.name, name_hint(f.kind), false),
        );
        r.row(tr!("source-sign-in-with"), None, |ui| {
            if widgets::chip(ui, tr!("plex-token"), f.plex.manual).clicked() {
                f.plex.use_token(true);
            }
            if widgets::chip(ui, tr!("plex-sign-in"), !f.plex.manual).clicked() {
                f.plex.use_token(false);
            }
        });
        if f.plex.manual {
            r.row(tr!("source-address"), None, |ui| {
                text_field(ui, &mut f.address, address_hint(f.kind), false);
            });
            r.row(tr!("plex-token"), None, |ui| {
                text_field(ui, &mut f.plex.token, "", true);
            });
            r.switch(
                tr!("source-self-signed"),
                Some(tr!("source-self-signed-desc")),
                &mut f.insecure_tls,
            );
        } else {
            link_rows(r, f, actions);
        }
        r.content(|ui| {
            widgets::section_label(ui, tr!("plex-connection-mode"));
            ui.add_enabled_ui(!matches!(f.plex.state, LinkState::Connecting), |ui| {
                ui.horizontal_wrapped(|ui| {
                    for mode in [
                        ConnectionMode::Local,
                        ConnectionMode::Direct,
                        ConnectionMode::Relay,
                    ] {
                        if widgets::chip(ui, mode.label(), f.plex.connection == mode).clicked()
                            && let Some(config) = f.plex.set_connection(mode)
                        {
                            actions.push(Action::PlexConnect(config));
                        }
                    }
                });
            });
            ui.add_space(6.0);
            let mut descriptions = vec![f.plex.connection.description()];
            if f.plex.manual && f.plex.connection == ConnectionMode::Local {
                descriptions.push(tr!("plex-connection-manual-local-desc"));
            }
            descriptions.extend([
                tr!("plex-connection-pass-desc"),
                tr!("plex-connection-subnet-desc"),
            ]);
            for description in descriptions {
                ui.label(
                    egui::RichText::new(description)
                        .font(theme::font(Weight::Regular, 15.0))
                        .color(theme::TEXT_2),
                );
            }
        });
    });
    ui.add_space(12.0);
    if f.editing.is_some() {
        ui.label(tr!("plex-new-account-source"));
    }
    let state = f.plex.state.clone();
    match state {
        LinkState::Connecting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr!("settings-device-connecting"));
            });
        }
        LinkState::Servers {
            credentials,
            servers,
        } => {
            widgets::section_label(ui, tr!("plex-server-select"));
            if servers.is_empty() {
                ui.label(tr!("plex-no-servers"));
            }
            widgets::rows(ui, |r| {
                for server in servers {
                    let desc = if server.owned {
                        tr!("plex-server-owned")
                    } else {
                        tr!("plex-server-shared")
                    };
                    r.row(&server.name, Some(desc), |ui| {
                        if widgets::button_sized(ui, None, tr!("common-add"), Kind::Secondary, 44.0)
                            .clicked()
                            && let Some(cfg) = server_config(&server, &credentials, &id, f)
                        {
                            f.plex.selected = None;
                            f.plex.state = LinkState::Connecting;
                            actions.push(Action::PlexConnect(cfg));
                        }
                    });
                }
            });
        }
        LinkState::Connected if f.plex.manual => {
            ui.label(tr!("quick-connect-authorized-desc"));
        }
        LinkState::Failed(error) => {
            ui.label(egui::RichText::new(error).color(theme::WARN));
        }
        _ => {}
    }
    let busy = matches!(
        f.plex.state,
        LinkState::Requesting | LinkState::Waiting { .. } | LinkState::Connecting
    );
    if !busy
        && f.plex.manual
        && widgets::button(ui, None, tr!("plex-connect"), Kind::Secondary).clicked()
    {
        if fp_sources::plex::is_web_app(&f.address) {
            f.plex.use_token(false);
            f.plex.state = LinkState::Requesting;
            actions.push(Action::PlexLinkStart);
        } else if !f.address.trim().is_empty() && !f.plex.token.trim().is_empty() {
            let id = replacement_id(f, id.clone());
            let cfg = manual_config(f, id);
            f.plex.selected = None;
            f.plex.state = LinkState::Connecting;
            actions.push(Action::PlexConnect(cfg));
        }
    }
    if f.plex.manual
        && f.plex
            .selected
            .as_ref()
            .is_some_and(|c| c.url != f.address.trim() || c.access_token != f.plex.token.trim())
    {
        f.plex.selected = None;
    }
    ui.add_space(16.0);
    if let Some(mut cfg) = f.plex.selected.clone()
        && matches!(f.plex.state, LinkState::Connected)
    {
        if !f.name.trim().is_empty() {
            cfg.name = f.name.trim().into();
        }
        f.plex.connection.apply(&mut cfg);
        cfg.insecure_tls = f.insecure_tls;
        if widgets::button(
            ui,
            Some(super::icons::CHECK),
            tr!("common-save"),
            Kind::Primary,
        )
        .clicked()
        {
            return Some(fp_sources::SourceConfig::Plex(cfg));
        }
    } else {
        ui.add_enabled_ui(false, |ui| {
            widgets::button(
                ui,
                Some(super::icons::CHECK),
                tr!("common-save"),
                Kind::Primary,
            )
        });
        ui.label(egui::RichText::new(tr!("plex-test-required")).color(theme::TEXT_3));
    }
    None
}

fn replacement_id(f: &mut SourceForm, current: String) -> String {
    if let Some(fp_sources::SourceConfig::Plex(old)) = &f.editing
        && old.access_token != f.plex.token.trim()
    {
        f.editing = None;
        format!("plex-{}", fp_core::playback::now_ms())
    } else {
        current
    }
}

fn manual_config(f: &SourceForm, id: String) -> PlexConfig {
    let mut cfg = match &f.editing {
        Some(fp_sources::SourceConfig::Plex(c)) => c.clone(),
        _ => PlexConfig {
            id,
            name: "Plex".into(),
            url: String::new(),
            machine_identifier: String::new(),
            access_token: String::new(),
            connections: Vec::new(),
            auth: None,
            insecure_tls: false,
            max_height: None,
            local_only: true,
            allow_relay: false,
        },
    };
    cfg.url = fp_sources::urlutil::with_http_scheme(f.address.trim());
    cfg.name = if f.name.trim().is_empty() {
        "Plex".into()
    } else {
        f.name.trim().to_string()
    };
    cfg.access_token = f.plex.token.trim().to_string();
    cfg.auth = None;
    cfg.connections.clear();
    cfg.insecure_tls = f.insecure_tls;
    f.plex.connection.apply(&mut cfg);
    cfg
}

fn server_config(
    server: &PlexResource,
    credentials: &PlexCredentials,
    id: &str,
    f: &SourceForm,
) -> Option<PlexConfig> {
    let connection = server
        .connections
        .iter()
        .find(|c| c.local && c.protocol == "https")
        .or_else(|| server.connections.iter().find(|c| c.local))
        .or_else(|| server.connections.iter().find(|c| !c.relay))
        .or_else(|| server.connections.first())?;
    let mut config = PlexConfig {
        id: f
            .editing
            .as_ref()
            .map_or_else(|| id.to_string(), |c| c.id().to_string()),
        name: if f.name.trim().is_empty() {
            server.name.clone()
        } else {
            f.name.trim().to_string()
        },
        url: connection.uri.clone(),
        machine_identifier: server.machine_identifier.clone(),
        access_token: server
            .access_token
            .clone()
            .unwrap_or_else(|| credentials.access_token.clone()),
        connections: server.connections.clone(),
        auth: Some(credentials.clone()),
        insecure_tls: false,
        max_height: None,
        local_only: true,
        allow_relay: false,
    };
    f.plex.connection.apply(&mut config);
    Some(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_modes_default_to_local_and_reach_both_setup_methods() {
        let mut form = SourceForm::default();
        assert_eq!(form.plex.connection, ConnectionMode::Local);
        let server = PlexResource {
            name: "Plex".into(),
            machine_identifier: "machine".into(),
            access_token: Some("server-token".into()),
            connections: vec![fp_sources::plex::auth::PlexConnection {
                uri: "https://server.plex.direct:32400".into(),
                address: Some("192.168.1.10".parse().unwrap()),
                port: Some(32400),
                local: true,
                relay: false,
                protocol: "https".into(),
            }],
            owned: true,
            provides: "server".into(),
        };
        let auth = PlexCredentials::manual("account-token".into(), "client".into());
        for (mode, local_only, allow_relay) in [
            (ConnectionMode::Local, true, false),
            (ConnectionMode::Direct, false, false),
            (ConnectionMode::Relay, false, true),
        ] {
            assert!(form.plex.set_connection(mode).is_none());
            assert_eq!(form.plex.connection, mode);
            assert!(matches!(form.plex.state, LinkState::Idle));
            let manual = manual_config(&form, "manual".into());
            let linked = server_config(&server, &auth, "linked", &form).unwrap();
            for config in [manual, linked] {
                assert_eq!(
                    (config.local_only, config.allow_relay),
                    (local_only, allow_relay)
                );
                assert_eq!(ConnectionMode::from_config(&config), mode);
            }
        }
    }

    #[test]
    fn editing_preserves_saved_modes_and_rechecks_a_changed_connection_policy() {
        for (local_only, allow_relay, mode) in [
            (true, false, ConnectionMode::Local),
            (true, true, ConnectionMode::Local),
            (false, false, ConnectionMode::Direct),
            (false, true, ConnectionMode::Relay),
        ] {
            let mut saved = manual_config(&SourceForm::default(), "saved-source".into());
            saved.url = "https://server.plex.direct:32400".into();
            saved.access_token = "server-token".into();
            saved.auth = Some(PlexCredentials::manual(
                "account-token".into(),
                "client".into(),
            ));
            saved.local_only = local_only;
            saved.allow_relay = allow_relay;
            let mut form =
                super::super::sources::edit_form(&fp_sources::SourceConfig::Plex(saved.clone()));
            assert_eq!(form.plex.connection, mode);
            assert_eq!(form.plex.selected.as_ref(), Some(&saved));
            assert!(form.plex.set_connection(mode).is_none());
            assert!(matches!(form.plex.state, LinkState::Connected));
            let next = if mode == ConnectionMode::Local {
                ConnectionMode::Direct
            } else {
                ConnectionMode::Local
            };
            let checked = form.plex.set_connection(next).unwrap();
            assert!(matches!(form.plex.state, LinkState::Connecting));
            assert_eq!(checked.id, saved.id);
            assert_eq!(checked.auth, saved.auth);
            assert_eq!(checked.access_token, saved.access_token);
            assert_eq!(ConnectionMode::from_config(&checked), next);
        }
    }

    #[test]
    fn code_linking_is_default_and_switching_methods_discards_pending_work() {
        let mut form = SourceForm::default();
        assert!(!form.plex.manual);
        form.plex.generation = 17;
        form.plex.state = LinkState::Connecting;
        form.plex.selected = Some(manual_config(&form, "source".into()));
        form.plex.use_token(true);
        assert!(form.plex.manual);
        assert_ne!(
            form.plex.generation, 17,
            "a pending result must no longer match"
        );
        assert!(form.plex.selected.is_none());
        assert!(matches!(form.plex.state, LinkState::Idle));
        form.plex.use_token(false);
        assert!(!form.plex.manual);
        form.plex.state = LinkState::Requesting;
        let generation = form.plex.generation;
        form.plex.reset();
        assert_ne!(form.plex.generation, generation);
        assert!(matches!(form.plex.state, LinkState::Idle));
    }

    #[test]
    fn replacing_manual_credentials_gets_a_separate_source_identity() {
        let mut form = SourceForm {
            address: "http://127.0.0.1:32400".into(),
            plex: Connect {
                token: "old-token".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let original = manual_config(&form, "old-source".into());
        form.editing = Some(fp_sources::SourceConfig::Plex(original));
        assert_eq!(replacement_id(&mut form, "old-source".into()), "old-source");
        form.plex.token = "another-account".into();
        let id = replacement_id(&mut form, "old-source".into());
        assert_ne!(id, "old-source");
        assert!(form.editing.is_none());
        let new = manual_config(&form, id.clone());
        assert_eq!(new.id, id);
        assert_eq!(new.access_token, "another-account");
        assert!(new.machine_identifier.is_empty());
    }
}
