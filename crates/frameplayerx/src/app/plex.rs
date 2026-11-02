//! Plex linking and connection checks run on jobs, tied to the open form.

use super::*;
use fp_sources::plex::auth::{PlexAuthClient, PlexCredentials, PlexPin, PlexResource};
use ui::plex::LinkState;

impl App {
    pub(super) fn plex_link_start(&mut self) {
        let generation = self.ui.next_seq();
        let Some(f) = self
            .ui
            .source_form
            .as_mut()
            .filter(|f| f.kind == fp_sources::SourceKind::Plex && !f.plex.manual)
        else {
            return;
        };
        f.plex.generation = generation;
        // A newly linked account must not inherit another principal's
        // history, queue or cached artwork under an existing source id.
        f.editing = None;
        f.plex.state = LinkState::Requesting;
        let client = self.services.device_identifier().to_string();
        self.jobs.spawn("plex-link", move || {
            Job::PlexInitiated(
                generation,
                PlexAuthClient::new()
                    .initiate(&client)
                    .map_err(|e| ui::errors::source(&e)),
            )
        });
    }

    pub(super) fn plex_link_poll(&mut self, generation: u64, pin: PlexPin) {
        let current = self.ui.source_form.as_ref().is_some_and(|f| f.kind == fp_sources::SourceKind::Plex
            && !f.plex.manual
            && f.plex.generation == generation
            && matches!(&f.plex.state, LinkState::Waiting { pin: current, .. } if current.id == pin.id));
        if !current {
            return;
        }
        self.jobs.spawn("plex-poll", move || {
            let api = PlexAuthClient::new();
            let result = api
                .poll(&pin)
                .and_then(|credentials| {
                    credentials
                        .map(|c| api.resources(&c).map(|servers| (c, servers)))
                        .transpose()
                })
                .map_err(|e| ui::errors::source(&e));
            Job::PlexPolled(generation, Box::new(result))
        });
    }

    pub(super) fn plex_connect(&mut self, config: fp_sources::PlexConfig) {
        let generation = self.ui.next_seq();
        let Some(f) = self
            .ui
            .source_form
            .as_mut()
            .filter(|f| f.kind == fp_sources::SourceKind::Plex)
        else {
            return;
        };
        f.plex.generation = generation;
        f.plex.state = LinkState::Connecting;
        self.jobs.spawn("plex-connect", move || {
            Job::PlexConnected(
                generation,
                Box::new(
                    fp_sources::plex::PlexSource::new(&config)
                        .and_then(|s| s.probe())
                        .map_err(|e| ui::errors::source(&e)),
                ),
            )
        });
    }

    fn plex_form(&mut self, generation: u64) -> Option<&mut ui::SourceForm> {
        self.ui
            .source_form
            .as_mut()
            .filter(|f| f.kind == fp_sources::SourceKind::Plex && f.plex.generation == generation)
    }

    pub(super) fn plex_initiated(&mut self, generation: u64, result: Result<PlexPin, String>) {
        let Some(f) = self.plex_form(generation) else {
            return;
        };
        if f.plex.manual || !matches!(f.plex.state, LinkState::Requesting) {
            return;
        }
        f.plex.state = match result {
            Ok(pin) => LinkState::Waiting {
                pin,
                polled: Instant::now() - Duration::from_secs(2),
                polling: false,
            },
            Err(e) => LinkState::Failed(e),
        };
        self.panels[MAIN].request_repaint();
    }

    pub(super) fn plex_polled(
        &mut self,
        generation: u64,
        result: Result<Option<(PlexCredentials, Vec<PlexResource>)>, String>,
    ) {
        let Some(f) = self.plex_form(generation) else {
            return;
        };
        let LinkState::Waiting { pin, .. } = &f.plex.state else {
            return;
        };
        if f.plex.manual {
            return;
        }
        if pin.expired() {
            f.plex.state = LinkState::Failed(tr!("plex-link-expired").into());
            self.panels[MAIN].request_repaint();
            return;
        }
        match result {
            Ok(Some((credentials, servers))) => {
                f.plex.state = LinkState::Servers {
                    credentials,
                    servers,
                }
            }
            Ok(None) => {
                if let LinkState::Waiting { polling, .. } = &mut f.plex.state {
                    *polling = false;
                }
            }
            Err(e) => f.plex.state = LinkState::Failed(e),
        }
        self.panels[MAIN].request_repaint();
    }

    pub(super) fn plex_connected(
        &mut self,
        generation: u64,
        result: Result<fp_sources::PlexConfig, String>,
    ) {
        let Some(f) = self.plex_form(generation) else {
            return;
        };
        if !matches!(f.plex.state, LinkState::Connecting) {
            return;
        }
        match result {
            Ok(cfg) => {
                f.address = cfg.url.clone();
                if f.name.trim().is_empty() {
                    f.name = cfg.name.clone();
                }
                f.plex.selected = Some(cfg);
                f.plex.state = LinkState::Connected;
            }
            Err(e) => f.plex.state = LinkState::Failed(e),
        }
        self.panels[MAIN].request_repaint();
    }
}
