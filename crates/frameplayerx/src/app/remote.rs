//! What the web remote shows of the open video beyond its playback status:
//! its picture, format, markers, volume, tracks, Up next and the loop
//! and Autoplay switches.

use super::App;
use crate::playback::Playback;
use crate::services::Services;
use crate::subtitles::Pick;
use fp_core::SubtitleTrack;
use fp_remote::{RemoteAudio, RemoteDetails, RemoteMarker, RemoteSubtitles};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often the details are built again; publishing compares first, so
/// details that did not change cost only the building.
const EVERY: Duration = Duration::from_millis(250);

impl App {
    pub(super) fn publish_details(&mut self) {
        let Some(remote) = &self.services.remote else {
            return;
        };
        if self.remote_details_at.elapsed() < EVERY {
            return;
        }
        self.remote_details_at = Instant::now();
        let Some(p) = &self.playback else {
            self.remote_picture = None;
            remote.publish_details(&RemoteDetails::default());
            return;
        };
        // Once a video: it asks the library.
        if self
            .remote_picture
            .as_ref()
            .is_none_or(|(at, _)| *at != p.request.location)
        {
            self.remote_picture = Some((p.request.location.clone(), picture(&self.services, p)));
        }
        let info = p.player.info();
        let playing = p.player.audio_stream();
        let audio = info
            .audio_streams()
            .map(|s| {
                let mut label = crate::subtitles::track_name(s);
                if s.ambisonic {
                    label += tr!("tracks-spatial");
                }
                RemoteAudio {
                    stream: s.index,
                    label,
                    detail: format!("{} · {} ch", s.codec, s.channels),
                    selected: playing == Some(s.index),
                }
            })
            .collect();
        let showing = p.subtitles_showing();
        let subtitles =
            crate::subtitles::rows(&self.settings.subtitle_language, info, &p.subtitle_files)
                .into_iter()
                .filter_map(|row| {
                    let track = match row.pick {
                        Pick::Stream(i) => SubtitleTrack::Stream(i),
                        Pick::File(i) => SubtitleTrack::File(i),
                        Pick::Off => return None,
                    };
                    Some(RemoteSubtitles {
                        track,
                        label: row.title,
                        detail: row.detail,
                        selected: row.pick == showing,
                    })
                })
                .collect();
        let queue = &self.queue;
        let has = |dir: i32| queue.neighbour(dir, self.settings.loop_playlist).is_some();
        remote.publish_details(&RemoteDetails {
            format: crate::ui::widgets::format_short(&p.format),
            picture: self.remote_picture.as_ref().and_then(|(_, p)| p.clone()),
            markers: p
                .markers
                .iter()
                .map(|m| RemoteMarker {
                    time: m.time,
                    name: m.name.clone(),
                })
                .collect(),
            volume: f64::from(p.player.volume()),
            audio,
            subtitles,
            has_previous: has(-1),
            has_next: has(1),
            up_next: queue
                .items
                .iter()
                .map(|r| crate::ui::widgets::display_title(&r.shown_title()))
                .collect(),
            up_next_at: queue.pos,
            loop_video: self.settings.loop_video,
            autoplay: self.settings.autoplay,
            loop_playlist: self.settings.loop_playlist,
        });
    }
}

/// The video's picture on the headset: the server's, if the image cache
/// already holds it, else the thumbnail made on the headset.
fn picture(services: &Services, p: &Playback) -> Option<PathBuf> {
    let record = p
        .record_id
        .and_then(|id| services.library.get(id).ok().flatten());
    let url = record
        .as_ref()
        .and_then(|r| r.thumbnail_url.clone())
        .or_else(|| {
            p.request
                .entry
                .as_ref()
                .and_then(|e| e.thumbnail_url.clone())
        });
    url.and_then(|u| services.images.cached_file(&u))
        .or_else(|| record.and_then(|r| r.thumbnail))
}
