//! Jellyfin and Emby media server source via the MediaBrowser REST API.
//!
//! Jellyfin and Emby share the MediaBrowser API family. Browsing starts at
//! `/Users/{userId}/Views` (the user's libraries) and drills down through
//! `/Users/{userId}/Items?ParentId=...`.
//!
//! Media streams are played directly via `/Videos/{itemId}/stream?static=true`.
//! Video details (subtitles and chapter markers) are resolved in [`Source::details`].
//! Playback progress can be reported back to the server using the session API.

use crate::Source;
use crate::config::{Credentials, MediaBrowserConfig, SourceKind};
use crate::dir_entry;
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions, HttpReply};
use crate::json::{array, boolean, get, num, text, uint};
use crate::sidecar::Sidecars;
use crate::transcode::{self, Streamed, TranscodeTarget};
use crate::urlutil::{encode_form_value, redact};
use crate::watch::{WatchEvent, WatchKind, WatchSession};
use fp_core::format::{Projection, StereoLayout, VideoFormat, detect_from_name};
use fp_core::source::{ByteSource, Entry, EntryKind};
use fp_core::sync::lock;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

mod catalog;
pub use catalog::{MbFacets, MbFilter, MbHome, MbItem, MbKind, MbPage, MbQuery, MbSort, MbVersion};

/// Largest JSON document accepted from a MediaBrowser API endpoint (64 MiB).
const JSON_LIMIT: u64 = 64 << 20;

/// Number of items to fetch per page when listing a folder.
const PAGE_SIZE: usize = 200;

/// Client name reported in the MediaBrowser authorization header.
const CLIENT_NAME: &str = "FramePlayerX";

/// Device name reported in the MediaBrowser authorization header.
const DEVICE_NAME: &str = "SteamFrame";

static DEVICE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Sets the id this installation gives servers, before the first request.
/// Jellyfin and Emby sign a user's sessions on a device out when the same
/// device id signs in again, so two installations must not share one.
pub fn set_device_id(id: String) {
    if DEVICE_ID.set(id).is_err() {
        log::warn!("the device id was set after the first request");
    }
}

/// The id from [`set_device_id`]; else one from the host name.
fn device_id() -> &'static str {
    DEVICE_ID.get_or_init(|| {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "frameplayerx-client".to_string());
        let mut hasher = DefaultHasher::new();
        host.hash(&mut hasher);
        format!("frameplayerx-{:016x}", hasher.finish())
    })
}

/// Builds the `MediaBrowser` authorization header value.
pub(crate) fn mediabrowser_header(token: Option<&str>) -> String {
    let dev_id = device_id();
    let version = env!("CARGO_PKG_VERSION");
    match token {
        Some(tok) => format!(
            "MediaBrowser Client=\"{CLIENT_NAME}\", Device=\"{DEVICE_NAME}\", DeviceId=\"{dev_id}\", Version=\"{version}\", Token=\"{tok}\""
        ),
        None => format!(
            "MediaBrowser Client=\"{CLIENT_NAME}\", Device=\"{DEVICE_NAME}\", DeviceId=\"{dev_id}\", Version=\"{version}\""
        ),
    }
}

/// Encodes a location for a MediaBrowser view, folder, or item.
pub(crate) fn encode_location(base: &str, kind: &str, id: &str) -> String {
    format!("mb:{base}#{kind}={id}")
}

/// The location of one version (media source) of an item kept in several:
/// each has its own library record, so its own format and adjustments.
pub(crate) fn encode_version_location(base: &str, id: &str, version: &str) -> String {
    format!("mb:{base}#item={id}&version={version}")
}

/// Decodes a location string into its `(kind, id)` parts.
pub(crate) fn decode_location(loc: &str) -> Option<(&str, &str)> {
    let rest = loc.strip_prefix("mb:")?;
    let (_, fragment) = rest.rsplit_once('#')?;
    let (kind, rest) = fragment.split_once('=')?;
    let id = rest.split_once('&').map_or(rest, |(id, _)| id);
    Some((kind, id))
}

/// The version a location names ([`encode_version_location`]).
pub(crate) fn location_version(loc: &str) -> Option<&str> {
    let (_, fragment) = loc.strip_prefix("mb:")?.rsplit_once('#')?;
    fragment
        .split('&')
        .find_map(|p| p.strip_prefix("version="))
        .filter(|v| !v.is_empty())
}

/// Extracts the item or folder identifier from a MediaBrowser location.
pub(crate) fn item_id(loc: &str) -> Result<String> {
    decode_location(loc)
        .map(|(_, id)| id.to_string())
        .ok_or_else(|| Error::invalid(loc, "not a MediaBrowser location"))
}

/// Converts MediaBrowser ticks (1 tick = 100 ns = 10^-7 s) to seconds.
pub(crate) fn ticks_to_seconds(ticks: f64) -> f64 {
    ticks / 10_000_000.0
}

/// Converts seconds to MediaBrowser ticks.
pub(crate) fn seconds_to_ticks(secs: f64) -> i64 {
    (secs * 10_000_000.0) as i64
}

/// Extracts the video height in pixels from a MediaBrowser `MediaSource` object.
pub(crate) fn media_source_height(ms: &Value) -> Option<u32> {
    uint(ms, "Height").or_else(|| {
        array(ms, "MediaStreams")
            .iter()
            .find(|s| text(s, "Type").as_deref() == Some("Video"))
            .and_then(|s| uint(s, "Height"))
    })
}

/// Picks the tallest media source not exceeding `max_height`; falls back to the smallest source
/// when all exceed `max_height`. Sources of unknown height rank lowest.
pub(crate) fn best_media_source(
    media_sources: &[Value],
    max_height: Option<u32>,
) -> Option<&Value> {
    if media_sources.is_empty() {
        return None;
    }
    let h = |s: &Value| media_source_height(s).unwrap_or(0);
    let fits = |s: &&Value| max_height.is_none_or(|m| h(s) <= m);
    media_sources
        .iter()
        .rev()
        .filter(fits)
        .max_by_key(|s| h(s))
        .or_else(|| media_sources.iter().min_by_key(|s| h(s)))
}

/// The media source `chosen` names when there is one, else
/// [`best_media_source`].
pub(crate) fn pick_media_source<'a>(
    media_sources: &'a [Value],
    chosen: Option<&str>,
    max_height: Option<u32>,
) -> Option<&'a Value> {
    chosen
        .and_then(|id| {
            media_sources
                .iter()
                .find(|ms| text(ms, "Id").as_deref() == Some(id))
        })
        .or_else(|| best_media_source(media_sources, max_height))
}

/// The sizes media source `ms` can be transcoded to: its own, then the
/// usual heights below it, the width kept in proportion. Their ids are
/// `<media source>/<width>x<height>/<bits a second>`.
pub(crate) fn targets_of(ms: &Value) -> Vec<TranscodeTarget> {
    let Some(source) = text(ms, "Id") else {
        return Vec::new();
    };
    let video = array(ms, "MediaStreams")
        .iter()
        .find(|s| text(s, "Type").as_deref() == Some("Video"));
    let (Some(width), Some(height)) = (
        video.and_then(|v| uint(v, "Width")),
        video
            .and_then(|v| uint(v, "Height"))
            .or_else(|| uint(ms, "Height")),
    ) else {
        return Vec::new();
    };
    let fps = video
        .and_then(|v| num(v, "RealFrameRate").or_else(|| num(v, "AverageFrameRate")))
        .unwrap_or(30.0);
    let file = video
        .and_then(|v| num(v, "BitRate"))
        .or_else(|| num(ms, "Bitrate"))
        .map(|b| (b as u64, height));
    transcode::heights_below(height)
        .into_iter()
        .map(|h| {
            let w = ((width as f64 * h as f64 / height as f64 / 2.0).round() as u32 * 2).max(2);
            TranscodeTarget {
                id: format!("{source}/{w}x{h}/{}", transcode::bitrate(w, h, fps, file)),
                label: transcode::height_label(Some(h)),
                height: Some(h),
                seekable: true,
            }
        })
        .collect()
}

/// A transcode id's media source, width, height and bitrate.
pub(crate) fn parse_transcode_id(id: &str) -> Option<(&str, u32, u32, u64)> {
    let (rest, bitrate) = id.rsplit_once('/')?;
    let (source, size) = rest.rsplit_once('/')?;
    let (w, h) = size.split_once('x')?;
    Some((
        source,
        w.parse().ok()?,
        h.parse().ok()?,
        bitrate.parse().ok()?,
    ))
}

/// The address of item `item`'s media source `source` encoded at most
/// `width` x `height` from `start` seconds, as MPEG-TS: HEVC when the
/// server encodes it (an admin's choice), else H.264, with stereo AAC.
/// Nothing is copied from the file, so the picture and sound both begin
/// at `start`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transcode_url(
    base: &str,
    item: &str,
    source: &str,
    (width, height, bitrate): (u32, u32, u64),
    start: f64,
    session: &str,
    token: &str,
) -> String {
    let mut url = format!(
        "{base}/Videos/{}/stream.ts?MediaSourceId={}&DeviceId={}&PlaySessionId={session}\
         &VideoCodec=hevc,h264&AudioCodec=aac&MaxWidth={width}&MaxHeight={height}\
         &VideoBitrate={bitrate}&AudioBitrate=192000&TranscodingMaxAudioChannels=2\
         &AllowVideoStreamCopy=false&AllowAudioStreamCopy=false&StartTimeTicks={}",
        encode_form_value(item),
        encode_form_value(source),
        encode_form_value(device_id()),
        seconds_to_ticks(start.max(0.0)),
    );
    if !token.is_empty() {
        url.push_str(&format!("&api_key={token}"));
    }
    url
}

/// Parses the access token and user ID from a `/Users/AuthenticateByName` response.
pub(crate) fn parse_auth_response(v: &Value, url: &str) -> Result<(String, String)> {
    let access_token = text(v, "AccessToken")
        .or_else(|| text(v, "Token"))
        .or_else(|| text(v, "AuthenticationToken"))
        .ok_or_else(|| Error::invalid(url, "missing AccessToken in auth response"))?;
    let user_id = get(v, "User")
        .and_then(|u| text(u, "Id"))
        .or_else(|| text(v, "UserId"))
        .or_else(|| get(v, "SessionInfo").and_then(|s| text(s, "UserId")))
        .ok_or_else(|| Error::invalid(url, "missing User.Id in auth response"))?;
    Ok((access_token, user_id))
}

/// Parses the code and secret from a `/QuickConnect/Initiate` response.
pub(crate) fn parse_quick_connect_initiate(v: &Value, url: &str) -> Result<QuickConnectState> {
    let code =
        text(v, "Code").ok_or_else(|| Error::invalid(url, "missing Code in initiate response"))?;
    let secret = text(v, "Secret")
        .ok_or_else(|| Error::invalid(url, "missing Secret in initiate response"))?;
    Ok(QuickConnectState { code, secret })
}

/// Parses authentication status and credentials from a `/QuickConnect/Connect` response.
pub(crate) fn parse_quick_connect_connect(v: &Value) -> (bool, Option<String>, Option<String>) {
    let authenticated = boolean(v, "Authenticated").unwrap_or(false);
    let auth_res = get(v, "AuthenticationResult");
    let token = auth_res
        .and_then(|ar| text(ar, "AccessToken").or_else(|| text(ar, "AuthenticationToken")))
        .or_else(|| text(v, "AccessToken"))
        .or_else(|| text(v, "AuthenticationToken"))
        .or_else(|| text(v, "Token"));
    let user_id = auth_res
        .and_then(|ar| text(ar, "UserId").or_else(|| get(ar, "User").and_then(|u| text(u, "Id"))))
        .or_else(|| text(v, "UserId"))
        .or_else(|| get(v, "User").and_then(|u| text(u, "Id")));
    (authenticated, token, user_id)
}

/// An item's primary image at card size. Its tag is in the address, so a
/// changed image has a new one.
fn primary_image(base_url: &str, id: &str, item: &Value) -> String {
    let mut url = format!("{base_url}/Items/{id}/Images/Primary?maxWidth=480&quality=80");
    if let Some(tag) = get(item, "ImageTags").and_then(|t| text(t, "Primary")) {
        url += &format!("&tag={}", encode_form_value(&tag));
    }
    url
}

/// Converts an item from `/Users/{userId}/Views` into a directory [`Entry`].
pub(crate) fn parse_views(v: &Value, base_url: &str) -> Vec<Entry> {
    array(v, "Items")
        .iter()
        .filter_map(|item| {
            let id = text(item, "Id")?;
            let name = text(item, "Name").unwrap_or_default();
            let mut e = dir_entry(&name, encode_location(base_url, "view", &id), None);
            e.thumbnail_url = Some(primary_image(base_url, &id, item));
            e.modified = text(item, "DateCreated").and_then(|d| crate::timeutil::parse_iso8601(&d));
            Some(e)
        })
        .collect()
}

/// Converts a MediaBrowser item JSON object into an [`Entry`].
pub(crate) fn item_to_entry(v: &Value, base_url: &str) -> Option<Entry> {
    let item_type = text(v, "Type")?;
    let id = text(v, "Id")?;
    let name = text(v, "Name").unwrap_or_default();
    match item_type.as_str() {
        "CollectionFolder" | "Folder" | "Series" | "Season" | "BoxSet" => {
            let mut e = dir_entry(&name, encode_location(base_url, "folder", &id), None);
            e.thumbnail_url = Some(primary_image(base_url, &id, v));
            e.modified = text(v, "DateCreated").and_then(|d| crate::timeutil::parse_iso8601(&d));
            Some(e)
        }
        "UserView" => {
            let mut e = dir_entry(&name, encode_location(base_url, "view", &id), None);
            e.thumbnail_url = Some(primary_image(base_url, &id, v));
            e.modified = text(v, "DateCreated").and_then(|d| crate::timeutil::parse_iso8601(&d));
            Some(e)
        }
        "Movie" | "Video" | "Episode" => {
            let location = encode_location(base_url, "item", &id);
            let mut e = Entry::new(&name, location, EntryKind::Video);
            e.duration = num(v, "RunTimeTicks").map(ticks_to_seconds);
            // The 16:9 art Jellyfin's own rows show, as the library keeps it
            // wherever the video was started from.
            e.thumbnail_url =
                catalog::parse_item(v, base_url).and_then(|i| i.landscape.or(i.poster));
            e.modified = text(v, "DateCreated").and_then(|d| crate::timeutil::parse_iso8601(&d));
            let path = text(v, "Path").unwrap_or_default();
            e.format = detect_from_name(&path)
                .or_else(|| detect_from_name(&name))
                .or_else(|| video_3d_format(v));
            Some(e)
        }
        _ => None,
    }
}

/// Parses 3D video layout from Jellyfin / Emby item or stream metadata.
fn video_3d_format(v: &Value) -> Option<VideoFormat> {
    let mode = text(v, "Video3DFormat").or_else(|| {
        array(v, "MediaStreams")
            .iter()
            .find(|s| text(s, "Type").as_deref() == Some("Video"))
            .and_then(|s| text(s, "Video3DFormat"))
    })?;
    let stereo = match mode.to_ascii_lowercase().as_str() {
        "halfsidebyside" | "hsbs" => StereoLayout::HalfSideBySide,
        "fullsidebyside" | "fsbs" | "sidebyside" => StereoLayout::SideBySide,
        "halftopandbottom" | "halfoverunder" | "hou" | "htb" => StereoLayout::HalfTopBottom,
        "fulltopandbottom" | "fulloverunder" | "fou" | "ftb" | "topandbottom" => {
            StereoLayout::TopBottom
        }
        _ => return None,
    };
    Some(VideoFormat::new(Projection::Flat, stereo))
}

/// Parses an items listing response from `/Users/{userId}/Items`.
pub(crate) fn parse_items(v: &Value, base_url: &str) -> Vec<Entry> {
    array(v, "Items")
        .iter()
        .filter_map(|item| item_to_entry(item, base_url))
        .collect()
}

/// Populates an entry with media stream details, format, and chapter markers.
pub(crate) fn apply_item_details(
    entry: &mut Entry,
    v: &Value,
    base_url: &str,
    item_id: &str,
    chosen: Option<&str>,
    max_height: Option<u32>,
) {
    // Named as the server names it: an entry made from the library may
    // know no more than the address.
    if let Some(item) = catalog::parse_item(v, base_url).filter(|i| !i.name.trim().is_empty()) {
        entry.name = item.full_title();
    }
    if let Some(ticks) = num(v, "RunTimeTicks") {
        entry.duration = Some(ticks_to_seconds(ticks));
    }
    if let Some(overview) = text(v, "Overview").filter(|o| !o.trim().is_empty()) {
        entry.description = Some(overview);
    }
    let tags = catalog::tag_names(v);
    if !tags.is_empty() {
        entry.tags = tags;
    }
    if let Some(user) = get(v, "UserData") {
        entry.resume = Some(
            num(user, "PlaybackPositionTicks")
                .filter(|t| *t > 0.0)
                .map_or(0.0, ticks_to_seconds),
        );
    }

    let media_sources = array(v, "MediaSources");
    let ms = pick_media_source(media_sources, chosen, max_height).or_else(|| media_sources.first());

    // Versions differ in format and are adjusted apart: each is its own video.
    if media_sources.len() > 1
        && let Some(id) = ms.and_then(|m| text(m, "Id"))
    {
        entry.location = encode_version_location(base_url, item_id, &id);
        entry.size = None;
        // The listing's guess came from the first version's file.
        entry.format = None;
    }

    if let Some(ms) = ms {
        if entry.size.is_none() {
            entry.size = num(ms, "Size").filter(|s| *s > 0.0).map(|s| s as u64);
        }
        let media_source_id = text(ms, "Id").unwrap_or_else(|| item_id.to_string());
        // Embedded subtitles come with the file (it streams as is); the
        // server only has to hand over the ones beside it. Asking it for an
        // embedded one makes it extract that from the whole file first,
        // which can take minutes.
        for stream in array(ms, "MediaStreams") {
            if text(stream, "Type").as_deref() != Some("Subtitle")
                || boolean(stream, "IsExternal") != Some(true)
            {
                continue;
            }
            let codec = text(stream, "Codec").unwrap_or_else(|| "srt".to_string());
            let index = num(stream, "Index").map(|n| n as u32).unwrap_or(0);
            let location = format!(
                "{base_url}/Videos/{item_id}/{media_source_id}/Subtitles/{index}/Stream.{codec}"
            );
            let mut file = crate::sidecar::subtitle_file(&location, "");
            // The address names every file `Stream.<codec>`; without its own
            // file name or title the player labels it by language and format.
            file.name = text(stream, "Path")
                .map(|p| crate::urlutil::last_segment(&p))
                .or_else(|| text(stream, "Title"))
                .unwrap_or_default();
            let mut tag = text(stream, "Language").unwrap_or_default();
            for (flag, word) in [("IsForced", ".forced"), ("IsHearingImpaired", ".sdh")] {
                if boolean(stream, flag) == Some(true) {
                    tag.push_str(word);
                }
            }
            file.language = Some(tag).filter(|t| !t.is_empty());
            entry.subtitles.push(file);
        }
    }

    let path = ms.and_then(|m| text(m, "Path")).or_else(|| text(v, "Path"));
    if let Some(p) = path
        && let Some(fmt) = detect_from_name(&p)
    {
        entry.format = Some(fmt);
    }
    if entry.format.is_none() {
        entry.format = detect_from_name(&entry.name);
    }
    if entry.format.is_none() {
        entry.format = ms.and_then(video_3d_format).or_else(|| video_3d_format(v));
    }

    for ch in array(v, "Chapters") {
        let name = text(ch, "Name").unwrap_or_default();
        if let Some(ticks) = num(ch, "StartPositionTicks") {
            entry.markers.push((ticks_to_seconds(ticks), name));
        }
    }
}

/// A Jellyfin or Emby media server browsed via the MediaBrowser REST API.
pub struct MediaBrowserSource {
    id: String,
    name: String,
    base_url: String,
    credentials: Option<Credentials>,
    insecure_tls: bool,
    parallel_downloads: bool,
    max_height: Option<u32>,
    auth: Mutex<Option<AuthState>>,
    parents: Mutex<HashMap<String, String>>,
    /// The server's tags and when they were read.
    tags: Mutex<Option<(std::time::Instant, Vec<String>)>>,
    /// The version chosen for an item (item id to media source id), kept
    /// while the app runs.
    versions: Mutex<HashMap<String, String>>,
    /// The play session of the transcode the server is encoding for us.
    encoding: Arc<Mutex<Option<String>>>,
}

/// Authentication and session state for an active server connection.
struct AuthState {
    user_id: String,
    #[allow(dead_code)]
    access_token: String,
    #[allow(dead_code)]
    server_name: String,
    client: HttpClient,
}

impl fmt::Debug for AuthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthState")
            .field("user_id", &self.user_id)
            .field("access_token", &"***")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

/// Quick Connect pairing state returned by [`MediaBrowserSource::quick_connect_initiate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickConnectState {
    /// 6-digit code to display to the user for pairing on another device.
    pub code: String,
    /// Secret used to poll for authorization.
    pub secret: String,
}

impl MediaBrowserSource {
    fn stable_asset(&self, location: &str) -> Option<String> {
        let mut url = url::Url::parse(location).ok()?;
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "api_key" | "access_token" | "token" | "x-emby-token"
                )
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        let relative = crate::location::relative_url(&self.base_url, url.as_str())?;
        Some(crate::location::encode(&self.id, "mb-asset", &relative))
    }

    fn stable_entry(&self, entry: &mut Entry) {
        if let Some(location) = self.canonical_location(&entry.location) {
            entry.location = location;
        }
        if let Some(url) = entry.thumbnail_url.as_mut()
            && let Some(stable) = self.stable_asset(url)
        {
            *url = stable;
        }
        for subtitle in &mut entry.subtitles {
            if let Some(stable) = self.stable_asset(&subtitle.location) {
                subtitle.location = stable;
            }
        }
        for script in &mut entry.scripts {
            if let Some(stable) = self.stable_asset(script) {
                *script = stable;
            }
        }
    }

    /// Restores a source-owned item reference to the adapter's internal form.
    /// The embedded owner is checked before any server request is made.
    fn resolved_location(&self, location: &str) -> Result<String> {
        if !crate::is_source_location(location) {
            return Ok(location.to_string());
        }
        let reference = crate::location::decode(location)
            .filter(|reference| reference.source_id == self.id && reference.kind == "mb")
            .ok_or_else(|| Error::invalid(location, "not an item of this MediaBrowser source"))?;
        let restored = format!("mb:{}#{}", self.base_url, reference.key);
        if decode_location(&restored).is_none() {
            return Err(Error::invalid(
                location,
                "invalid MediaBrowser item reference",
            ));
        }
        Ok(restored)
    }

    /// Creates a new MediaBrowser (Jellyfin/Emby) source from configuration.
    pub fn new(config: &MediaBrowserConfig) -> Result<Self> {
        let base_url = config.url.trim_end_matches('/').to_string();
        if !crate::urlutil::is_http_url(&base_url) {
            return Err(Error::invalid(
                &config.url,
                "expected an http:// or https:// MediaBrowser URL",
            ));
        }
        let auth = if let (Some(token), Some(uid)) = (&config.access_token, &config.user_id) {
            let authed_header = mediabrowser_header(Some(token));
            let opts = HttpOptions {
                insecure_tls: config.insecure_tls,
                cache: crate::cache::CacheOptions::for_downloads(config.parallel_downloads),
                ..HttpOptions::default()
            };
            let authed_client = HttpClient::new_with_header(opts, &authed_header, &base_url);
            Some(AuthState {
                user_id: uid.clone(),
                access_token: token.clone(),
                server_name: config.name.clone(),
                client: authed_client,
            })
        } else {
            None
        };
        Ok(MediaBrowserSource {
            id: config.id.clone(),
            name: config.name.clone(),
            base_url,
            credentials: config.credentials.clone(),
            insecure_tls: config.insecure_tls,
            parallel_downloads: config.parallel_downloads,
            max_height: config.max_height,
            auth: Mutex::new(auth),
            parents: Mutex::new(HashMap::new()),
            tags: Mutex::new(None),
            versions: Mutex::new(HashMap::new()),
            encoding: Arc::default(),
        })
    }

    /// The media source of `location` that plays: the version it names,
    /// the one chosen, else the tallest that fits the maximum height.
    fn playing_version(&self, location: &str, item: &Value) -> Option<Value> {
        let location = self.resolved_location(location).ok()?;
        let location = location.as_str();
        let id = text(item, "Id").unwrap_or_default();
        let chosen = location_version(location)
            .map(str::to_string)
            .or_else(|| self.chosen_version(&id));
        pick_media_source(
            array(item, "MediaSources"),
            chosen.as_deref(),
            self.max_height,
        )
        .cloned()
    }

    /// Tells the server to stop the encode of play session `session`.
    fn stop_encoding(client: &HttpClient, base: &str, session: &str) {
        let url = format!(
            "{base}/Videos/ActiveEncodings?DeviceId={}&PlaySessionId={session}",
            encode_form_value(device_id())
        );
        match client.request("DELETE", &url, &[], None, 64 << 10) {
            Ok(r) if r.status < 300 || r.status == 404 => {}
            Ok(r) => log::warn!("stopping transcode {session}: HTTP {}", r.status),
            Err(e) => log::warn!("stopping transcode {session}: {e}"),
        }
    }

    /// Plays the video at `location` from the media source `version` from
    /// now on (subtitles included); `None` goes back to the tallest that
    /// fits the maximum height.
    pub fn set_version(&self, location: &str, version: Option<&str>) {
        let Ok(location) = self.resolved_location(location) else {
            return;
        };
        let location = location.as_str();
        let Ok(id) = item_id(location) else {
            return;
        };
        let mut versions = lock(&self.versions);
        match version {
            Some(v) => versions.insert(id, v.to_string()),
            None => versions.remove(&id),
        };
    }

    /// The version chosen for the item `id`.
    fn chosen_version(&self, id: &str) -> Option<String> {
        lock(&self.versions).get(id).cloned()
    }

    /// Authenticates with the server using configured credentials, storing the session token.
    fn authenticate(&self) -> Result<()> {
        let creds = self
            .credentials
            .as_ref()
            .ok_or_else(|| Error::SignedOut(redact(&self.base_url)))?;
        let opts = HttpOptions {
            insecure_tls: self.insecure_tls,
            cache: crate::cache::CacheOptions::for_downloads(self.parallel_downloads),
            ..HttpOptions::default()
        };
        let unauthed_client = HttpClient::new(opts.clone(), None);
        let auth_url = format!("{}/Users/AuthenticateByName", self.base_url);
        let auth_header = mediabrowser_header(None);
        let body = serde_json::to_vec(&serde_json::json!({
            "Username": creds.username,
            "Pw": creds.password,
            "Password": creds.password,
        }))
        .map_err(|e| Error::invalid(&auth_url, e.to_string()))?;

        let reply = unauthed_client
            .request(
                "POST",
                &auth_url,
                &[
                    ("Authorization", &auth_header),
                    ("X-Emby-Authorization", &auth_header),
                    ("Accept", "application/json"),
                    ("Content-Type", "application/json"),
                ],
                Some(&body),
                JSON_LIMIT,
            )?
            .error_for_status(&auth_url)?;

        let v: Value = serde_json::from_slice(&reply.body)
            .map_err(|e| Error::invalid(&auth_url, format!("invalid JSON: {e}")))?;
        let (access_token, user_id) = parse_auth_response(&v, &auth_url)?;

        let info_url = format!("{}/System/Info/Public", self.base_url);
        let server_name = unauthed_client
            .request(
                "GET",
                &info_url,
                &[
                    ("Authorization", &auth_header),
                    ("X-Emby-Authorization", &auth_header),
                    ("Accept", "application/json"),
                ],
                None,
                JSON_LIMIT,
            )
            .ok()
            .and_then(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .and_then(|info| text(&info, "ProductName"))
            .unwrap_or_else(|| "MediaBrowser Server".to_string());

        let authed_header = mediabrowser_header(Some(&access_token));
        let authed_client = HttpClient::new_with_header(opts, &authed_header, &self.base_url);

        let mut auth = lock(&self.auth);
        *auth = Some(AuthState {
            user_id,
            access_token,
            server_name,
            client: authed_client,
        });

        Ok(())
    }

    /// Returns an authenticated HTTP client and current user ID, authenticating lazily if needed.
    fn client(&self) -> Result<(HttpClient, String)> {
        {
            let auth = lock(&self.auth);
            if let Some(state) = auth.as_ref() {
                return Ok((state.client.clone(), state.user_id.clone()));
            }
        }
        self.authenticate()?;
        let auth = lock(&self.auth);
        let state = auth
            .as_ref()
            .ok_or_else(|| Error::Auth(redact(&self.base_url)))?;
        Ok((state.client.clone(), state.user_id.clone()))
    }

    /// Sends an authenticated API request with automatic single retry on 401.
    fn api_request(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&[u8]>,
    ) -> Result<HttpReply> {
        let (client, _) = self.client()?;
        let url = format!("{}{}", self.base_url, path_and_query);
        let headers: &[(&str, &str)] = if body.is_some() {
            &[
                ("Content-Type", "application/json"),
                ("Accept", "application/json"),
            ]
        } else {
            &[("Accept", "application/json")]
        };
        let reply = client.request(method, &url, headers, body, JSON_LIMIT)?;
        if reply.status == 401 {
            log::debug!("MediaBrowser 401 at {}, re-authenticating", redact(&url));
            {
                let mut auth = lock(&self.auth);
                *auth = None;
            }
            if self.credentials.is_none() {
                return Err(Error::SignedOut(redact(&self.base_url)));
            }
            self.authenticate()?;
            let (client, _) = self.client()?;
            let reply = client.request(method, &url, headers, body, JSON_LIMIT)?;
            return reply.error_for_status(&url);
        }
        reply.error_for_status(&url)
    }

    /// Sends an authenticated GET request and parses the body as JSON.
    fn api_get_json(&self, path_and_query: &str) -> Result<Value> {
        let reply = self.api_request("GET", path_and_query, None)?;
        let url = format!("{}{}", self.base_url, path_and_query);
        serde_json::from_slice(&reply.body)
            .map_err(|e| Error::invalid(&url, format!("invalid JSON: {e}")))
    }

    /// Sends an authenticated POST request with a JSON body.
    fn api_post_json(&self, path: &str, body: &Value) -> Result<()> {
        let json_bytes =
            serde_json::to_vec(body).map_err(|e| Error::invalid(path, e.to_string()))?;
        self.api_request("POST", path, Some(&json_bytes))?;
        Ok(())
    }

    /// Checks if Quick Connect is enabled on the server.
    pub fn quick_connect_enabled(&self) -> Result<bool> {
        let opts = HttpOptions {
            insecure_tls: self.insecure_tls,
            cache: crate::cache::CacheOptions::for_downloads(self.parallel_downloads),
            ..HttpOptions::default()
        };
        let client = HttpClient::new(opts, None);
        let url = format!("{}/QuickConnect/Enabled", self.base_url);
        let reply = client.request("GET", &url, &[], None, JSON_LIMIT)?;
        if reply.status == 404 {
            return Ok(false);
        }
        let reply = reply.error_for_status(&url)?;
        let text = reply.text();
        Ok(text.trim().eq_ignore_ascii_case("true"))
    }

    /// Initiates Quick Connect, returning the code to display to the user.
    pub fn quick_connect_initiate(&self) -> Result<QuickConnectState> {
        let opts = HttpOptions {
            insecure_tls: self.insecure_tls,
            cache: crate::cache::CacheOptions::for_downloads(self.parallel_downloads),
            ..HttpOptions::default()
        };
        let client = HttpClient::new(opts, None);
        let url = format!("{}/QuickConnect/Initiate", self.base_url);
        let auth_header = mediabrowser_header(None);
        let reply = client.request(
            "POST",
            &url,
            &[
                ("Authorization", &auth_header),
                ("X-Emby-Authorization", &auth_header),
                ("Accept", "application/json"),
                ("Content-Type", "application/json"),
            ],
            Some(b"{}"),
            JSON_LIMIT,
        )?;
        if reply.status == 404 {
            return Err(Error::Unsupported(
                "Quick Connect on this server (Emby has none): sign in with your \
                 user name and password"
                    .into(),
            ));
        }
        let reply = reply.error_for_status(&url)?;
        let v: Value = serde_json::from_slice(&reply.body)
            .map_err(|e| Error::invalid(&url, format!("invalid JSON: {e}")))?;
        parse_quick_connect_initiate(&v, &url)
    }

    /// Polls for Quick Connect authorization. Returns `Ok(Some((token, user_id)))` when authorized.
    pub fn quick_connect_poll_auth(&self, secret: &str) -> Result<Option<(String, String)>> {
        let opts = HttpOptions {
            insecure_tls: self.insecure_tls,
            cache: crate::cache::CacheOptions::for_downloads(self.parallel_downloads),
            ..HttpOptions::default()
        };
        let client = HttpClient::new(opts.clone(), None);
        let encoded_secret = encode_form_value(secret);
        let url = format!(
            "{}/QuickConnect/Connect?secret={encoded_secret}",
            self.base_url
        );
        let auth_header = mediabrowser_header(None);
        let reply = client.request(
            "GET",
            &url,
            &[
                ("Authorization", &auth_header),
                ("X-Emby-Authorization", &auth_header),
                ("Accept", "application/json"),
            ],
            None,
            JSON_LIMIT,
        )?;
        let reply = reply.error_for_status(&url)?;
        let v: Value = serde_json::from_slice(&reply.body)
            .map_err(|e| Error::invalid(&url, format!("invalid JSON: {e}")))?;
        let (authenticated, token_opt, user_id_opt) = parse_quick_connect_connect(&v);
        if !authenticated {
            return Ok(None);
        }

        let (token, user_id) = if let Some(token) = token_opt {
            let uid = if let Some(uid) = user_id_opt {
                uid
            } else {
                let authed_header = mediabrowser_header(Some(&token));
                let authed_client =
                    HttpClient::new_with_header(opts.clone(), &authed_header, &self.base_url);
                let me_url = format!("{}/Users/Me", self.base_url);
                authed_client
                    .request(
                        "GET",
                        &me_url,
                        &[
                            ("Authorization", &authed_header),
                            ("X-Emby-Authorization", &authed_header),
                            ("Accept", "application/json"),
                        ],
                        None,
                        JSON_LIMIT,
                    )
                    .ok()
                    .and_then(|r| serde_json::from_slice::<Value>(&r.body).ok())
                    .and_then(|me| text(&me, "Id"))
                    .unwrap_or_default()
            };
            (token, uid)
        } else {
            let qc_auth_url = format!("{}/Users/AuthenticateWithQuickConnect", self.base_url);
            let body = serde_json::to_vec(&serde_json::json!({
                "Secret": secret,
            }))
            .map_err(|e| Error::invalid(&qc_auth_url, e.to_string()))?;
            let reply = client
                .request(
                    "POST",
                    &qc_auth_url,
                    &[
                        ("Authorization", &auth_header),
                        ("X-Emby-Authorization", &auth_header),
                        ("Accept", "application/json"),
                        ("Content-Type", "application/json"),
                    ],
                    Some(&body),
                    JSON_LIMIT,
                )?
                .error_for_status(&qc_auth_url)?;
            let auth_v: Value = serde_json::from_slice(&reply.body)
                .map_err(|e| Error::invalid(&qc_auth_url, format!("invalid JSON: {e}")))?;
            parse_auth_response(&auth_v, &qc_auth_url)?
        };

        let authed_header = mediabrowser_header(Some(&token));
        let authed_client = HttpClient::new_with_header(opts, &authed_header, &self.base_url);

        let info_url = format!("{}/System/Info/Public", self.base_url);
        let server_name = client
            .request(
                "GET",
                &info_url,
                &[
                    ("Authorization", &auth_header),
                    ("X-Emby-Authorization", &auth_header),
                    ("Accept", "application/json"),
                ],
                None,
                JSON_LIMIT,
            )
            .ok()
            .and_then(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .and_then(|info| text(&info, "ProductName"))
            .unwrap_or_else(|| "MediaBrowser Server".to_string());

        let mut auth = lock(&self.auth);
        *auth = Some(AuthState {
            user_id: user_id.clone(),
            access_token: token.clone(),
            server_name,
            client: authed_client,
        });

        Ok(Some((token, user_id)))
    }

    /// Polls for Quick Connect authorization. Returns `true` when authorized.
    pub fn quick_connect_poll(&self, secret: &str) -> Result<bool> {
        Ok(self.quick_connect_poll_auth(secret)?.is_some())
    }

    /// Returns the current access token and user ID if authenticated.
    pub fn auth_credentials(&self) -> Option<(String, String)> {
        let auth = lock(&self.auth);
        auth.as_ref()
            .map(|a| (a.access_token.clone(), a.user_id.clone()))
    }
}

impl Source for MediaBrowserSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::MediaBrowser
    }

    fn describe(&self) -> String {
        format!("MediaBrowser server {}", redact(&self.base_url))
    }

    fn canonical_location(&self, location: &str) -> Option<String> {
        if crate::is_source_location(location) {
            let reference = crate::location::decode(location)?;
            return (reference.source_id == self.id
                && matches!(reference.kind.as_str(), "mb" | "mb-asset"))
            .then(|| crate::location::encode(&self.id, &reference.kind, &reference.key));
        }
        if let Some((_, key)) = location
            .strip_prefix("mb:")
            .and_then(|rest| rest.rsplit_once('#'))
            && decode_location(location).is_some()
        {
            return Some(crate::location::encode(&self.id, "mb", key));
        }
        // Older callers may have saved the direct-play URL rather than the
        // item's MediaBrowser location. Recover its exact item/version,
        // excluding ephemeral authentication and streaming parameters.
        let asset = self.stable_asset(location)?;
        if let Some(reference) = crate::location::decode(&asset) {
            let path = reference.key.split('?').next()?;
            let segments: Vec<_> = path.split('/').collect();
            if let ["Videos", id, "stream"] = segments.as_slice()
                && !id.is_empty()
            {
                let parsed = url::Url::parse(location).ok()?;
                let version = parsed.query_pairs().find_map(|(key, value)| {
                    key.eq_ignore_ascii_case("MediaSourceId")
                        .then(|| value.into_owned())
                });
                let id = crate::urlutil::percent_decode(id);
                let key = match version.filter(|version| !version.is_empty()) {
                    Some(version) => format!("item={id}&version={version}"),
                    None => format!("item={id}"),
                };
                return Some(crate::location::encode(&self.id, "mb", &key));
            }
        }
        Some(asset)
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        match location {
            None => {
                let (_, user_id) = self.client()?;
                let path = format!("/Users/{user_id}/Views");
                let v = self.api_get_json(&path)?;
                let mut entries = parse_views(&v, &self.base_url);
                for entry in &mut entries {
                    self.stable_entry(entry);
                }
                Ok(entries)
            }
            Some(loc) => {
                let resolved = self.resolved_location(loc)?;
                let loc = resolved.as_str();
                let id = item_id(loc)?;
                let (_, user_id) = self.client()?;
                let mut start_index: usize = 0;
                let mut all_entries = Vec::new();

                loop {
                    let query = format!(
                        "/Users/{user_id}/Items?ParentId={id}&Recursive=false\
                        &Fields=Path,MediaSources,Overview,DateCreated&SortBy=SortName&SortOrder=Ascending\
                        &StartIndex={start_index}&Limit={PAGE_SIZE}"
                    );
                    let v = self.api_get_json(&query)?;
                    // The page as sent: one of only photos still goes on.
                    let count = array(&v, "Items").len();
                    let mut entries = parse_items(&v, &self.base_url);
                    for entry in &mut entries {
                        self.stable_entry(entry);
                    }
                    all_entries.extend(entries);

                    let total = num(&v, "TotalRecordCount").map(|n| n as usize).unwrap_or(0);
                    start_index += PAGE_SIZE;
                    if count == 0 || start_index >= total {
                        break;
                    }
                }

                {
                    let mut parents = lock(&self.parents);
                    for entry in &all_entries {
                        parents.insert(
                            entry.location.clone(),
                            self.canonical_location(loc)
                                .unwrap_or_else(|| loc.to_string()),
                        );
                    }
                }

                Ok(all_entries)
            }
        }
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        if let Some(reference) = crate::location::decode(location)
            && reference.source_id == self.id
            && reference.kind == "mb-asset"
        {
            let url = crate::location::resolve_url(&self.base_url, &reference.key)
                .ok_or_else(|| Error::invalid(location, "invalid MediaBrowser resource path"))?;
            return Ok(Arc::new(self.client()?.0.open(&url)?));
        }
        let resolved = self.resolved_location(location)?;
        let location = resolved.as_str();
        let id = item_id(location)?;
        let (client, user_id) = self.client()?;
        let token = self.auth_credentials().map(|(t, _)| t).unwrap_or_default();
        let mut url = format!("{}/Videos/{}/stream?static=true", self.base_url, id);
        // The version the details and subtitles came from (the one chosen,
        // else by max height); otherwise the server streams its own choice.
        let version = location_version(location)
            .map(str::to_string)
            .or_else(|| self.chosen_version(&id))
            .or_else(|| {
                self.api_get_json(&format!("/Users/{user_id}/Items/{id}?Fields=MediaSources"))
                    .ok()
                    .and_then(|v| {
                        best_media_source(array(&v, "MediaSources"), self.max_height)
                            .and_then(|ms| text(ms, "Id"))
                    })
            });
        if let Some(ms) = version {
            url.push_str(&format!("&MediaSourceId={}", encode_form_value(&ms)));
        }
        if !token.is_empty() {
            url.push_str(&format!("&api_key={token}"));
        }
        Ok(Arc::new(client.open(&url)?))
    }

    fn picture_request(&self, location: &str) -> Option<(HttpClient, String)> {
        let reference = crate::location::decode(location)?;
        if reference.source_id != self.id || reference.kind != "mb-asset" {
            return None;
        }
        let url = crate::location::resolve_url(&self.base_url, &reference.key)?;
        Some((self.client().ok()?.0, url))
    }

    fn parent(&self, location: &str) -> Option<String> {
        let location = self.canonical_location(location)?;
        lock(&self.parents).get(&location).cloned()
    }

    fn details(&self, entry: &Entry) -> Result<Entry> {
        let resolved = self.resolved_location(&entry.location)?;
        let Ok(id) = item_id(&resolved) else {
            return Ok(entry.clone());
        };
        let (_, user_id) = self.client()?;
        let path =
            format!("/Users/{user_id}/Items/{id}?Fields=MediaSources,Chapters,Path,Overview,Tags");
        let v = self.api_get_json(&path)?;
        let mut out = entry.clone();
        let chosen = location_version(&resolved)
            .map(str::to_string)
            .or_else(|| self.chosen_version(&id));
        apply_item_details(
            &mut out,
            &v,
            &self.base_url,
            &id,
            chosen.as_deref(),
            self.max_height,
        );
        self.stable_entry(&mut out);
        Ok(out)
    }

    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        let full = if video.subtitles.is_empty() {
            self.details(video)?
        } else {
            video.clone()
        };
        Ok(Sidecars::from_entry(&full))
    }

    fn watch_session(&self, video: &Entry) -> Option<WatchSession> {
        let resolved = self.resolved_location(&video.location).ok()?;
        let id = item_id(&resolved).ok()?;
        Some(WatchSession::new(id, video.name.clone(), None))
    }

    /// The server's own playback reports: started, progress (with what
    /// changed) and stopped, which sets the resume point or marks the item
    /// played.
    fn report_watch(&self, session: &WatchSession, event: WatchEvent) -> Result<()> {
        let mut body = serde_json::json!({
            "ItemId": session.item,
            "PlaySessionId": session.id,
            "PositionTicks": seconds_to_ticks(event.position),
            "IsPaused": event.paused,
            "PlaybackRate": event.speed,
            "CanSeek": true,
            "PlayMethod": "DirectPlay",
        });
        let path = match event.kind {
            WatchKind::Start => "/Sessions/Playing",
            WatchKind::Stop => "/Sessions/Playing/Stopped",
            kind => {
                body["EventName"] = match kind {
                    WatchKind::Pause => "Pause",
                    WatchKind::Resume => "Unpause",
                    WatchKind::Speed => "PlaybackRateChange",
                    _ => "TimeUpdate",
                }
                .into();
                "/Sessions/Playing/Progress"
            }
        };
        self.api_post_json(path, &body)
    }

    /// The sizes the playing version can be transcoded to: its own and the
    /// usual heights below, none above the maximum height.
    fn transcode_targets(&self, video: &Entry) -> Vec<TranscodeTarget> {
        let found = self
            .resolved_location(&video.location)
            .and_then(|location| item_id(&location))
            .and_then(|id| {
                let (_, user) = self.client()?;
                self.api_get_json(&format!("/Users/{user}/Items/{id}?Fields=MediaSources"))
            });
        match found {
            Ok(item) => self
                .playing_version(&video.location, &item)
                .map(|ms| transcode::within(self.max_height, targets_of(&ms)))
                .unwrap_or_default(),
            Err(e) => {
                log::warn!("transcode targets of {}: {e}", redact(&video.location));
                Vec::new()
            }
        }
    }

    /// Stops the encode the server is running for us, then asks for this
    /// one. Dropping the stream stops its encode: Emby otherwise encodes on
    /// to the end, Jellyfin for a while.
    fn open_transcode(&self, location: &str, id: &str, start: f64) -> Result<Arc<dyn ByteSource>> {
        let resolved = self.resolved_location(location)?;
        let location = resolved.as_str();
        let item = item_id(location)?;
        let (source, width, height, bitrate) = parse_transcode_id(id)
            .ok_or_else(|| Error::invalid(location, format!("no transcode {id}")))?;
        let (client, _) = self.client()?;
        let session = crate::watch::session_id();
        if let Some(before) = lock(&self.encoding).replace(session.clone()) {
            Self::stop_encoding(&client, &self.base_url, &before);
        }
        let token = self.auth_credentials().map(|(t, _)| t).unwrap_or_default();
        let url = transcode_url(
            &self.base_url,
            &item,
            source,
            (width, height, bitrate),
            start,
            &session,
            &token,
        );
        log::info!(
            "{}: transcode {width}x{height} at {:.1} Mbit/s from {start:.1} s",
            self.name,
            bitrate as f64 / 1e6
        );
        let body = client.get_stream(&url)?;
        let (base, encoding) = (self.base_url.clone(), self.encoding.clone());
        let stop = move || {
            {
                let mut playing = lock(&encoding);
                if playing.as_deref() == Some(session.as_str()) {
                    *playing = None;
                }
            }
            Self::stop_encoding(&client, &base, &session);
        };
        Ok(Streamed::wrap(body, redact(&url), Some(Box::new(stop))))
    }

    fn as_media_browser(&self) -> Option<&MediaBrowserSource> {
        Some(self)
    }

    fn as_catalog(&self) -> Option<&dyn crate::Catalog> {
        Some(self)
    }
}

/// Initiates Quick Connect for a server URL.
pub fn quick_connect_initiate_url(url: &str, insecure_tls: bool) -> Result<QuickConnectState> {
    let config = MediaBrowserConfig {
        id: "qc".into(),
        name: "Jellyfin".into(),
        url: url.into(),
        credentials: None,
        insecure_tls,
        parallel_downloads: false,
        max_height: None,
        access_token: None,
        user_id: None,
    };
    let src = MediaBrowserSource::new(&config)?;
    src.quick_connect_initiate()
}

/// Polls Quick Connect authorization for a server URL and secret.
pub fn quick_connect_poll_url(
    url: &str,
    secret: &str,
    insecure_tls: bool,
) -> Result<Option<(String, String)>> {
    let config = MediaBrowserConfig {
        id: "qc".into(),
        name: "Jellyfin".into(),
        url: url.into(),
        credentials: None,
        insecure_tls,
        parallel_downloads: false,
        max_height: None,
        access_token: None,
        user_id: None,
    };
    let src = MediaBrowserSource::new(&config)?;
    src.quick_connect_poll_auth(secret)
}

impl fmt::Debug for MediaBrowserSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaBrowserSource")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("base_url", &redact(&self.base_url))
            .field("credentials", &self.credentials)
            .field("insecure_tls", &self.insecure_tls)
            .field("max_height", &self.max_height)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_location_roundtrip() {
        let base = "http://jellyfin:8096";

        let view_loc = encode_location(base, "view", "v123");
        assert_eq!(view_loc, "mb:http://jellyfin:8096#view=v123");
        assert_eq!(decode_location(&view_loc), Some(("view", "v123")));
        assert_eq!(item_id(&view_loc).unwrap(), "v123");

        let folder_loc = encode_location(base, "folder", "f456");
        assert_eq!(folder_loc, "mb:http://jellyfin:8096#folder=f456");
        assert_eq!(decode_location(&folder_loc), Some(("folder", "f456")));
        assert_eq!(item_id(&folder_loc).unwrap(), "f456");

        let item_loc = encode_location(base, "item", "i789");
        assert_eq!(item_loc, "mb:http://jellyfin:8096#item=i789");
        assert_eq!(decode_location(&item_loc), Some(("item", "i789")));
        assert_eq!(item_id(&item_loc).unwrap(), "i789");

        assert!(decode_location("http://not-a-mb-loc").is_none());
        assert!(decode_location("mb:invalid_without_hash").is_none());
        assert!(item_id("invalid").is_err());
    }

    #[test]
    fn test_mediabrowser_header() {
        let unauthed = mediabrowser_header(None);
        assert!(unauthed.starts_with(
            "MediaBrowser Client=\"FramePlayerX\", Device=\"SteamFrame\", DeviceId=\""
        ));
        assert!(unauthed.contains(&format!("Version=\"{}\"", env!("CARGO_PKG_VERSION"))));
        assert!(!unauthed.contains("Token="));

        let authed = mediabrowser_header(Some("test_access_token"));
        assert!(authed.starts_with(
            "MediaBrowser Client=\"FramePlayerX\", Device=\"SteamFrame\", DeviceId=\""
        ));
        assert!(authed.contains(&format!("Version=\"{}\"", env!("CARGO_PKG_VERSION"))));
        assert!(authed.contains("Token=\"test_access_token\""));
    }

    #[test]
    fn test_parse_auth_response() {
        let v_full = json!({
            "AccessToken": "secret_token_123",
            "User": {
                "Id": "user_id_456",
                "Name": "Alice"
            }
        });
        let (token, uid) = parse_auth_response(&v_full, "http://server/auth").unwrap();
        assert_eq!(token, "secret_token_123");
        assert_eq!(uid, "user_id_456");

        let v_root_uid = json!({
            "AccessToken": "tok_xyz",
            "UserId": "user_id_789"
        });
        let (token2, uid2) = parse_auth_response(&v_root_uid, "http://server/auth").unwrap();
        assert_eq!(token2, "tok_xyz");
        assert_eq!(uid2, "user_id_789");

        let v_missing_token = json!({
            "User": { "Id": "user_id_1" }
        });
        assert!(parse_auth_response(&v_missing_token, "http://server/auth").is_err());

        let v_missing_uid = json!({
            "AccessToken": "tok_123"
        });
        assert!(parse_auth_response(&v_missing_uid, "http://server/auth").is_err());
    }

    #[test]
    fn test_parse_views() {
        let v = json!({
            "Items": [
                {
                    "Id": "v1",
                    "Name": "Movies",
                    "Type": "UserView",
                    "DateCreated": "2024-01-15T12:00:00Z",
                    "ImageTags": {"Primary": "p1"}
                },
                {
                    "Id": "v2",
                    "Name": "TV Shows",
                    "Type": "CollectionFolder"
                }
            ],
            "TotalRecordCount": 2
        });

        let views = parse_views(&v, "http://server:8096");
        assert_eq!(views.len(), 2);

        assert_eq!(views[0].name, "Movies");
        assert_eq!(views[0].location, "mb:http://server:8096#view=v1");
        assert_eq!(views[0].kind, EntryKind::Directory);
        assert_eq!(
            views[0].thumbnail_url.as_deref(),
            Some("http://server:8096/Items/v1/Images/Primary?maxWidth=480&quality=80&tag=p1")
        );
        assert!(views[0].modified.is_some());

        assert_eq!(views[1].name, "TV Shows");
        assert_eq!(views[1].location, "mb:http://server:8096#view=v2");
        assert_eq!(views[1].kind, EntryKind::Directory);
    }

    #[test]
    fn test_parse_items_mixed() {
        let v = json!({
            "Items": [
                {
                    "Id": "f1",
                    "Name": "Sci-Fi",
                    "Type": "Folder"
                },
                {
                    "Id": "s1",
                    "Name": "Westworld",
                    "Type": "Series"
                },
                {
                    "Id": "m1",
                    "Name": "Blade Runner",
                    "Type": "Movie",
                    "RunTimeTicks": 70200000000.0,
                    "Path": "/media/Blade Runner 2049.mkv",
                    "ImageTags": {"Primary": "mp", "Thumb": "mt"}
                },
                {
                    "Id": "e1",
                    "Name": "The Original",
                    "Type": "Episode",
                    "RunTimeTicks": 40800000000.0,
                    "Path": "/media/Westworld S01E01.mp4"
                },
                {
                    "Id": "p1",
                    "Name": "Ridley Scott",
                    "Type": "Person"
                }
            ],
            "TotalRecordCount": 5
        });

        let entries = parse_items(&v, "http://server:8096");
        assert_eq!(entries.len(), 4);

        assert_eq!(entries[0].name, "Sci-Fi");
        assert_eq!(entries[0].location, "mb:http://server:8096#folder=f1");
        assert_eq!(entries[0].kind, EntryKind::Directory);

        assert_eq!(entries[1].name, "Westworld");
        assert_eq!(entries[1].location, "mb:http://server:8096#folder=s1");
        assert_eq!(entries[1].kind, EntryKind::Directory);

        assert_eq!(entries[2].name, "Blade Runner");
        assert_eq!(entries[2].location, "mb:http://server:8096#item=m1");
        assert_eq!(entries[2].kind, EntryKind::Video);
        assert_eq!(entries[2].duration, Some(7020.0));
        // The 16:9 art, not the poster.
        assert_eq!(
            entries[2].thumbnail_url.as_deref(),
            Some("http://server:8096/Items/m1/Images/Thumb?maxWidth=640&quality=90&tag=mt")
        );

        assert_eq!(entries[3].name, "The Original");
        assert_eq!(entries[3].location, "mb:http://server:8096#item=e1");
        assert_eq!(entries[3].kind, EntryKind::Video);
        assert_eq!(entries[3].duration, Some(4080.0));
        // A server that sends no tags: its still, untagged.
        assert_eq!(
            entries[3].thumbnail_url.as_deref(),
            Some("http://server:8096/Items/e1/Images/Primary?maxHeight=480&quality=90")
        );
    }

    #[test]
    fn test_item_with_subtitles_and_chapters() {
        let v = json!({
            "Id": "item100",
            "Name": "Demo VR Scene",
            "RunTimeTicks": 6000000000.0,
            "Path": "/media/demo_180_sbs.mp4",
            "Overview": "A demo scene.",
            "Tags": ["Beach", "Sunset"],
            "MediaSources": [
                {
                    "Id": "ms1",
                    "Size": 104857600,
                    "MediaStreams": [
                        {
                            "Type": "Video",
                            "Height": 1080,
                            "Width": 1920
                        },
                        {
                            "Type": "Subtitle",
                            "Index": 2,
                            "Codec": "subrip",
                            "Language": "ara"
                        },
                        {
                            "Type": "Subtitle",
                            "Index": 3,
                            "Codec": "ass",
                            "Language": "eng",
                            "IsExternal": true,
                            "Path": "/media/demo_180_sbs.en.ass"
                        },
                        {
                            "Type": "Subtitle",
                            "Index": 4,
                            "Codec": "subrip",
                            "Language": "ger",
                            "IsForced": true,
                            "IsExternal": true
                        },
                        {
                            "Type": "Subtitle",
                            "Index": 5,
                            "Codec": "subrip",
                            "IsHearingImpaired": true,
                            "IsExternal": true
                        }
                    ]
                }
            ],
            "Chapters": [
                {
                    "Name": "Intro",
                    "StartPositionTicks": 0
                },
                {
                    "Name": "Action",
                    "StartPositionTicks": 1500000000.0
                }
            ]
        });

        let mut entry = Entry::new(
            "Demo VR Scene",
            "mb:http://server:8096#item=item100",
            EntryKind::Video,
        );
        apply_item_details(&mut entry, &v, "http://server:8096", "item100", None, None);

        assert_eq!(entry.duration, Some(600.0));
        assert_eq!(entry.size, Some(104857600));
        assert_eq!(entry.description.as_deref(), Some("A demo scene."));
        assert_eq!(entry.tags, ["Beach", "Sunset"]);
        assert!(entry.format.is_some());
        // The embedded Arabic track comes with the file, not as a download.
        let subs: Vec<_> = entry
            .subtitles
            .iter()
            .map(|s| (s.location.as_str(), s.name.as_str(), s.language.as_deref()))
            .collect();
        assert_eq!(
            subs,
            [
                (
                    "http://server:8096/Videos/item100/ms1/Subtitles/3/Stream.ass",
                    "demo_180_sbs.en.ass",
                    Some("eng")
                ),
                (
                    "http://server:8096/Videos/item100/ms1/Subtitles/4/Stream.subrip",
                    "",
                    Some("ger.forced")
                ),
                (
                    "http://server:8096/Videos/item100/ms1/Subtitles/5/Stream.subrip",
                    "",
                    Some(".sdh")
                ),
            ]
        );
        assert_eq!(entry.markers.len(), 2);
        assert_eq!(entry.markers[0], (0.0, "Intro".to_string()));
        assert_eq!(entry.markers[1], (150.0, "Action".to_string()));
    }

    #[test]
    fn details_name_the_video_as_the_server_does() {
        // Opened from the library, where only the address was at hand.
        let mut entry = Entry::new(
            "generic-media.example",
            "mb:https://generic-media.example#item=e7",
            EntryKind::Video,
        );
        let v = json!({
            "Id": "e7",
            "Name": "The Original",
            "Type": "Episode",
            "SeriesName": "Westworld",
            "ParentIndexNumber": 1,
            "IndexNumber": 1
        });
        apply_item_details(
            &mut entry,
            &v,
            "https://generic-media.example",
            "e7",
            None,
            None,
        );
        assert_eq!(entry.name, "Westworld S1:E1 · The Original");
        // Without a name the entry keeps its own.
        let mut entry = Entry::new("Kept", "mb:https://s#item=x", EntryKind::Video);
        apply_item_details(
            &mut entry,
            &json!({"Id": "x"}),
            "https://s",
            "x",
            None,
            None,
        );
        assert_eq!(entry.name, "Kept");
    }

    #[test]
    fn test_ticks_to_seconds() {
        assert_eq!(ticks_to_seconds(0.0), 0.0);
        assert_eq!(ticks_to_seconds(10_000_000.0), 1.0);
        assert_eq!(ticks_to_seconds(600_000_000.0), 60.0);
        assert_eq!(ticks_to_seconds(88_800_000_000.0), 8880.0);

        assert_eq!(seconds_to_ticks(0.0), 0);
        assert_eq!(seconds_to_ticks(1.0), 10_000_000);
        assert_eq!(seconds_to_ticks(60.0), 600_000_000);
        assert_eq!(seconds_to_ticks(1.5), 15_000_000);
    }

    #[test]
    fn a_chosen_version_plays_over_the_tallest() {
        let sources = vec![
            json!({"Id": "ms_4k", "Height": 2160}),
            json!({"Id": "ms_1080", "Height": 1080}),
        ];
        let id = |ms: Option<&Value>| ms.and_then(|m| text(m, "Id"));
        assert_eq!(
            id(pick_media_source(&sources, Some("ms_1080"), None)).as_deref(),
            Some("ms_1080")
        );
        // A version the item no longer has falls back to the tallest.
        assert_eq!(
            id(pick_media_source(&sources, Some("gone"), None)).as_deref(),
            Some("ms_4k")
        );
        assert_eq!(
            id(pick_media_source(&sources, None, Some(1080))).as_deref(),
            Some("ms_1080")
        );
    }

    #[test]
    fn subtitles_come_from_the_chosen_version() {
        let sub = |index: u32| json!({"Type": "Subtitle", "Index": index, "Codec": "srt", "IsExternal": true});
        let v = json!({
            "Id": "m1",
            "MediaSources": [
                {"Id": "a", "Height": 2160, "MediaStreams": [sub(3)]},
                {"Id": "b", "Height": 1080, "MediaStreams": [sub(5)]}
            ]
        });
        let mut entry = Entry::new("Film", "mb:http://s#item=m1", EntryKind::Video);
        apply_item_details(&mut entry, &v, "http://s", "m1", Some("b"), None);
        assert_eq!(
            entry.subtitles[0].location,
            "http://s/Videos/m1/b/Subtitles/5/Stream.srt"
        );
    }

    #[test]
    fn a_version_has_its_own_location() {
        let loc = encode_version_location("http://s:8096", "m1", "b");
        assert_eq!(loc, "mb:http://s:8096#item=m1&version=b");
        assert_eq!(item_id(&loc).unwrap(), "m1");
        assert_eq!(location_version(&loc), Some("b"));
        assert_eq!(location_version("mb:http://s:8096#item=m1"), None);
    }

    #[test]
    fn each_version_is_its_own_video_with_its_own_format() {
        let v = json!({
            "Id": "m1",
            "Path": "/films/Film_180_LR.mp4",
            "MediaSources": [
                {"Id": "a", "Height": 2160, "Path": "/films/Film_180_LR.mp4"},
                {"Id": "b", "Height": 1080, "Path": "/films/Film - Flat.mkv", "Size": 5}
            ]
        });
        // As listed: the guess from the first version's file.
        let listed = || {
            let mut e = Entry::new("Film", "mb:http://s#item=m1", EntryKind::Video);
            e.format = detect_from_name("/films/Film_180_LR.mp4");
            e
        };
        let mut flat = listed();
        apply_item_details(&mut flat, &v, "http://s", "m1", Some("b"), None);
        assert_eq!(flat.location, "mb:http://s#item=m1&version=b");
        assert_eq!(flat.format, detect_from_name("/films/Film - Flat.mkv"));
        assert_ne!(flat.format, detect_from_name("/films/Film_180_LR.mp4"));
        assert_eq!(flat.size, Some(5));
        let mut vr = listed();
        apply_item_details(&mut vr, &v, "http://s", "m1", None, None);
        assert_eq!(vr.location, "mb:http://s#item=m1&version=a");
        assert_eq!(vr.format, detect_from_name("/films/Film_180_LR.mp4"));

        // One version: the item's location, as before.
        let one = json!({"Id": "m2", "MediaSources": [{"Id": "x"}]});
        let mut e = Entry::new("Other", "mb:http://s#item=m2", EntryKind::Video);
        apply_item_details(&mut e, &one, "http://s", "m2", None, None);
        assert_eq!(e.location, "mb:http://s#item=m2");
    }

    #[test]
    fn set_version_is_kept_per_item() {
        let src = MediaBrowserSource::new(&MediaBrowserConfig {
            id: "jf".into(),
            name: "Jellyfin".into(),
            url: "http://s".into(),
            credentials: None,
            insecure_tls: false,
            parallel_downloads: false,
            max_height: None,
            access_token: None,
            user_id: None,
        })
        .unwrap();
        src.set_version("mb:http://s#item=m1", Some("b"));
        assert_eq!(src.chosen_version("m1").as_deref(), Some("b"));
        assert_eq!(src.chosen_version("m2"), None);
        src.set_version("mb:http://s#item=m1", None);
        assert_eq!(src.chosen_version("m1"), None);
    }

    #[test]
    fn transcodes_keep_the_videos_proportions() {
        let ms = json!({"Id": "ms8k", "MediaStreams": [
            {"Type": "Audio"},
            {"Type": "Video", "Width": 7680, "Height": 3840, "RealFrameRate": 59.94, "BitRate": 80000000}
        ]});
        let t = targets_of(&ms);
        let labels: Vec<&str> = t.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            ["3840p", "2880p", "2160p", "1440p", "1080p", "720p"]
        );
        assert!(t.iter().all(|t| t.seekable));
        assert_eq!(
            parse_transcode_id(&t[2].id),
            Some(("ms8k", 4320, 2160, 25_312_500))
        );
        assert_eq!(parse_transcode_id("ms/1x2"), None);
        assert!(targets_of(&json!({"Id": "a", "MediaStreams": []})).is_empty());

        let url = transcode_url(
            "http://jf:8096",
            "item 1",
            "ms8k",
            (4320, 2160, 25_312_500),
            61.5,
            "p1",
            "tok",
        );
        assert!(url.starts_with("http://jf:8096/Videos/item%201/stream.ts?MediaSourceId=ms8k&"));
        for part in [
            "&PlaySessionId=p1&",
            "&MaxWidth=4320&MaxHeight=2160&VideoBitrate=25312500&",
            "&AllowVideoStreamCopy=false&AllowAudioStreamCopy=false&",
            "&StartTimeTicks=615000000&",
        ] {
            assert!(url.contains(part), "{part} in {url}");
        }
        assert!(url.ends_with("&api_key=tok"));
    }

    #[test]
    fn test_best_media_source() {
        let ms_4k = json!({
            "Id": "ms_4k",
            "Height": 2160,
            "MediaStreams": [{"Type": "Video", "Height": 2160}]
        });
        let ms_1080 = json!({
            "Id": "ms_1080",
            "Height": 1080,
            "MediaStreams": [{"Type": "Video", "Height": 1080}]
        });
        let ms_720 = json!({
            "Id": "ms_720",
            "Height": 720,
            "MediaStreams": [{"Type": "Video", "Height": 720}]
        });
        let ms_unknown = json!({
            "Id": "ms_unknown"
        });

        let sources = vec![ms_4k.clone(), ms_1080.clone(), ms_720.clone()];

        // No max_height picks tallest
        assert_eq!(best_media_source(&sources, None), Some(&ms_4k));

        // Max height 1080 picks 1080
        assert_eq!(best_media_source(&sources, Some(1080)), Some(&ms_1080));

        // Max height 720 picks 720
        assert_eq!(best_media_source(&sources, Some(720)), Some(&ms_720));

        // When all sources exceed max_height, picks smallest
        assert_eq!(best_media_source(&sources, Some(480)), Some(&ms_720));

        // Unknown height ranks lower than known heights
        let mixed = vec![ms_unknown.clone(), ms_720.clone()];
        assert_eq!(best_media_source(&mixed, None), Some(&ms_720));

        // Empty sources returns None
        assert!(best_media_source(&[], None).is_none());
    }

    #[test]
    fn test_quick_connect_parse() {
        let init_v = json!({
            "Code": "123456",
            "Secret": "sec-abcdef"
        });
        let state = parse_quick_connect_initiate(&init_v, "http://server/quickconnect").unwrap();
        assert_eq!(state.code, "123456");
        assert_eq!(state.secret, "sec-abcdef");

        let connect_pending = json!({
            "Authenticated": false
        });
        let (auth, tok, uid) = parse_quick_connect_connect(&connect_pending);
        assert!(!auth);
        assert!(tok.is_none());
        assert!(uid.is_none());

        let connect_done = json!({
            "Authenticated": true,
            "AccessToken": "token_xyz",
            "UserId": "user_789"
        });
        let (auth, tok, uid) = parse_quick_connect_connect(&connect_done);
        assert!(auth);
        assert_eq!(tok.as_deref(), Some("token_xyz"));
        assert_eq!(uid.as_deref(), Some("user_789"));
    }

    #[test]
    fn test_video_3d_format_parse() {
        let v_hsbs = json!({
            "Video3DFormat": "HalfSideBySide"
        });
        assert_eq!(
            video_3d_format(&v_hsbs),
            Some(VideoFormat::new(
                Projection::Flat,
                StereoLayout::HalfSideBySide
            ))
        );

        let v_stream = json!({
            "MediaStreams": [
                {"Type": "Audio"},
                {"Type": "Video", "Video3DFormat": "FullSideBySide"}
            ]
        });
        assert_eq!(
            video_3d_format(&v_stream),
            Some(VideoFormat::new(Projection::Flat, StereoLayout::SideBySide))
        );
    }
}
