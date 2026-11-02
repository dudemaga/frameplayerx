//! Plex progressive encodes use the same streamed ByteSource and seek restarts.

use super::{PlexSource, resource_url};
use crate::error::{Error, Result};
use crate::json::{array, num, text, uint};
use crate::transcode::{self, Streamed, TranscodeTarget};
use crate::watch::WatchSession;
use fp_core::source::{ByteSource, Entry};
use fp_core::sync::lock;
use std::sync::Arc;

impl PlexSource {
    pub(super) fn targets(&self, video: &Entry) -> Result<Vec<TranscodeTarget>> {
        let location = self.parse_location(&video.location)?;
        let raw = self.raw_item(&location.id)?;
        let selected = location
            .media
            .or_else(|| lock(&self.versions).get(&location.id).cloned());
        let media = self.choose_media(&raw, selected.as_deref())?;
        let height = uint(media, "height").unwrap_or(1080);
        let width = uint(media, "width").unwrap_or(height.saturating_mul(16) / 9);
        let media_id = text(media, "id").ok_or_else(|| Error::parse("Plex media", "missing id"))?;
        let endpoint = self.client()?;
        let heights = if endpoint.relay {
            vec![height.min(720)]
        } else {
            transcode::heights_below(height)
        };
        let all = heights
            .into_iter()
            .map(|h| {
                let w = ((u64::from(width) * u64::from(h) / u64::from(height.max(1))) as u32)
                    .max(2)
                    & !1;
                let bitrate = if endpoint.relay {
                    1_800_000
                } else {
                    transcode::bitrate(
                        w,
                        h,
                        30.0,
                        num(media, "bitrate").map(|b| ((b.max(0.0) * 1000.0) as u64, height)),
                    )
                };
                let mut id = url::form_urlencoded::Serializer::new(String::new());
                id.append_pair("media", &media_id)
                    .append_pair("width", &w.to_string())
                    .append_pair("height", &h.to_string())
                    .append_pair("bitrate", &bitrate.to_string());
                TranscodeTarget {
                    id: id.finish(),
                    label: format!("{h}p"),
                    height: Some(h),
                    seekable: true,
                }
            })
            .collect();
        Ok(transcode::within(lock(&self.config).max_height, all))
    }

    pub(super) fn encode(
        &self,
        location: &str,
        target: &str,
        start: f64,
        watch: Option<&WatchSession>,
    ) -> Result<Arc<dyn ByteSource>> {
        let location = self.parse_location(location)?;
        if location.kind != "item" {
            return Err(Error::Unsupported("Plex location is not a video".into()));
        }
        let args: std::collections::HashMap<_, _> = url::form_urlencoded::parse(target.as_bytes())
            .into_owned()
            .collect();
        let number = |key: &str| {
            args.get(key)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|n| *n > 0 && *n < 1_000_000_000)
                .ok_or_else(|| Error::Unsupported("invalid Plex quality target".into()))
        };
        let (width, height, bitrate) = (number("width")?, number("height")?, number("bitrate")?);
        let media_id = args
            .get("media")
            .ok_or_else(|| Error::Unsupported("invalid Plex media target".into()))?;
        let raw = self.raw_item(&location.id)?;
        let media_index = array(&raw, "Media")
            .iter()
            .position(|m| text(m, "id").as_ref() == Some(media_id))
            .ok_or_else(|| Error::NotFound("Plex version".into()))?;
        let endpoint = self.client()?;
        let encoding = crate::watch::session_id();
        if let Some(before) = lock(&self.encoding).replace(encoding.clone()) {
            stop(&endpoint.client, &endpoint.base, &before);
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("path", &format!("/library/metadata/{}", location.id))
            .append_pair("mediaIndex", &media_index.to_string()).append_pair("partIndex", "-1")
            .append_pair("protocol", "http").append_pair("directPlay", "0").append_pair("directStream", "1")
            .append_pair("offset", &if start.is_finite() { start.max(0.0) } else { 0.0 }.to_string())
            .append_pair("videoResolution", &format!("{width}x{height}"))
            .append_pair("videoBitrate", &(bitrate / 1000).to_string())
            .append_pair("X-Plex-Client-Profile-Name", "Generic")
            .append_pair("X-Plex-Client-Profile-Extra", "add-transcode-target(type=videoProfile&context=streaming&protocol=http&container=mkv&videoCodec=h264&audioCodec=aac&subtitleCodec=srt)")
            .append_pair("session", &encoding)
            .append_pair("X-Plex-Session-Identifier", watch.map_or(encoding.as_str(), |s| s.id.as_str()));
        if let Some(watch) = watch {
            if let Some(audio) = &watch.audio_stream {
                query.append_pair("audioStreamID", audio);
            }
            if let Some(subtitle) = &watch.subtitle_stream {
                query.append_pair("subtitleStreamID", subtitle);
                query.append_pair("subtitles", if subtitle == "0" { "none" } else { "burn" });
            }
        }
        let query = query.finish();
        let decision = self
            .json(&format!("/video/:/transcode/universal/decision?{query}"))
            .and_then(|value| {
                let decision = super::container(&value);
                if num(decision, "generalDecisionCode").is_some_and(|n| n >= 2000.0) {
                    return Err(Error::Unsupported(
                        text(decision, "generalDecisionText")
                            .unwrap_or_else(|| "Plex declined transcoding".into()),
                    ));
                }
                Ok(())
            });
        let body = decision
            .and_then(|_| {
                resource_url(
                    &endpoint.base,
                    &format!("/video/:/transcode/universal/start.mkv?{query}"),
                )
            })
            .and_then(|url| endpoint.client.get_stream(&url));
        let body = match body {
            Ok(body) => body,
            Err(e) => {
                stop(&endpoint.client, &endpoint.base, &encoding);
                return Err(e);
            }
        };
        let active = self.encoding.clone();
        let title = format!("Plex {}", location.id);
        Ok(Streamed::wrap(
            body,
            title,
            Some(Box::new(move || {
                if lock(&active).as_deref() == Some(encoding.as_str()) {
                    *lock(&active) = None;
                }
                stop(&endpoint.client, &endpoint.base, &encoding);
            })),
        ))
    }
}

fn stop(client: &crate::http::HttpClient, base: &str, id: &str) {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("session", id);
    let url = format!("{base}/video/:/transcode/universal/stop?{}", query.finish());
    if let Err(e) = client
        .request("GET", &url, &[], None, 4096)
        .and_then(|r| r.error_for_status(&url))
    {
        log::debug!("stopping Plex encoding: {e}");
    }
}
