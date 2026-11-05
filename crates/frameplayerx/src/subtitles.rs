//! Which subtitles a video starts with, and how its tracks are named.

use crate::settings::SubtitleMode;
use fp_core::format::{StereoLayout, VideoFormat};
use fp_core::language::Tag;
use fp_core::source::SubtitleFile;
use fp_library::PickedSubtitles;
use fp_media::subtitle::SubBitmap;
use fp_media::{MediaInfo, StreamInfo};
use fp_sources::urlutil::last_segment;

/// The subtitles a video starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Off,
    /// An embedded stream, by index.
    Stream(usize),
    /// One of the video's subtitle files, by position.
    File(usize),
}

impl Pick {
    pub fn stream(self) -> Option<usize> {
        match self {
            Pick::Stream(i) => Some(i),
            _ => None,
        }
    }
}

struct Track {
    pick: Pick,
    tag: Tag,
    default: bool,
    /// A file with no language, named like the video (`Movie.srt`).
    beside: bool,
}

impl Track {
    /// Full subtitles before forced ones, then without SDH, files (added
    /// on purpose) before embedded streams, then the stream marked default.
    fn rank(&self) -> (bool, bool, bool, bool) {
        (
            self.tag.forced,
            self.tag.sdh,
            matches!(self.pick, Pick::Stream(_)),
            !self.default,
        )
    }
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// A stream's tag, with what its dispositions and title add.
fn stream_tag(s: &StreamInfo) -> Tag {
    let mut tag = Tag::parse(s.language.as_deref().unwrap_or(""));
    let title = s.title.as_deref().unwrap_or("").to_ascii_lowercase();
    tag.forced |= s.forced || title.contains("forced");
    tag.sdh |= s.hearing_impaired || title.contains("sdh");
    tag
}

/// The subtitles to start with, for `mode` and the user's `language`:
///
/// - Always: the best subtitles in `language`, when there are any.
/// - Automatic (and Always without them): subtitles in `language` when the
///   audio is in another one; else forced subtitles in the audio's language
///   or `language`; else a subtitle file named like the video.
pub fn choose(
    mode: SubtitleMode,
    language: &str,
    info: &MediaInfo,
    files: &[SubtitleFile],
    video_name: &str,
) -> Pick {
    if mode == SubtitleMode::Off {
        return Pick::Off;
    }
    let mut tracks: Vec<Track> = files
        .iter()
        .enumerate()
        .map(|(i, f)| Track {
            pick: Pick::File(i),
            tag: Tag::parse(f.language.as_deref().unwrap_or("")),
            default: false,
            beside: f.language.is_none() && stem(&f.name).eq_ignore_ascii_case(stem(video_name)),
        })
        .collect();
    tracks.extend(info.subtitle_streams().map(|s| Track {
        pick: Pick::Stream(s.index),
        tag: stream_tag(s),
        default: s.default,
        beside: false,
    }));
    // A stable sort keeps the files' and streams' own order among equals.
    tracks.sort_by_key(Track::rank);
    let mine = Some(language);
    let audio = info
        .audio
        .and_then(|i| info.streams.iter().find(|s| s.index == i))
        .and_then(|s| Tag::parse(s.language.as_deref().unwrap_or("")).code);
    let in_mine = tracks.iter().find(|t| t.tag.code == mine);
    if let Some(t) = in_mine
        && (mode == SubtitleMode::Always || audio.is_some_and(|a| Some(a) != mine))
    {
        return t.pick;
    }
    let forced = tracks.iter().find(|t| {
        t.tag.forced && (t.tag.code.is_none() || t.tag.code == audio || t.tag.code == mine)
    });
    forced
        .or_else(|| tracks.iter().find(|t| t.beside))
        .map_or(Pick::Off, |t| t.pick)
}

/// What Subtitles on / off switches to: off while some are showing, else
/// those it last turned off (`last`), else those Always would choose, else
/// the first listed. None when the video has none.
pub fn toggled(
    showing: Pick,
    last: Option<Pick>,
    language: &str,
    info: &MediaInfo,
    files: &[SubtitleFile],
    video_name: &str,
) -> Option<Pick> {
    if showing != Pick::Off {
        return Some(Pick::Off);
    }
    last.or_else(|| {
        Some(choose(
            SubtitleMode::Always,
            language,
            info,
            files,
            video_name,
        ))
        .filter(|p| *p != Pick::Off)
    })
    .or_else(|| rows(language, info, files).first().map(|r| r.pick))
}

/// The subtitles a video starts with: those picked for it while they are
/// still there, else what the settings [`choose`] with the audio it starts
/// with (`audio`, when one was picked).
pub fn starting(
    picked: Option<&PickedSubtitles>,
    audio: Option<usize>,
    mode: SubtitleMode,
    language: &str,
    info: &MediaInfo,
    files: &[SubtitleFile],
    video_name: &str,
) -> Pick {
    if let Some(pick) = picked.and_then(|p| restored(p, info, files)) {
        return pick;
    }
    match audio.filter(|i| Some(*i) != info.audio && info.audio_streams().any(|s| s.index == *i)) {
        Some(i) => {
            let mut info = info.clone();
            info.audio = Some(i);
            choose(mode, language, &info, files, video_name)
        }
        None => choose(mode, language, info, files, video_name),
    }
}

/// A pick as it is saved with the video: a file by where it is, as the
/// files beside a video can change.
pub fn saved(pick: Pick, files: &[SubtitleFile]) -> Option<PickedSubtitles> {
    Some(match pick {
        Pick::Off => PickedSubtitles::Off,
        Pick::Stream(i) => PickedSubtitles::Stream(i),
        Pick::File(i) => PickedSubtitles::File(files.get(i)?.location.clone()),
    })
}

/// A saved pick again, while what it names is still there.
fn restored(saved: &PickedSubtitles, info: &MediaInfo, files: &[SubtitleFile]) -> Option<Pick> {
    match saved {
        PickedSubtitles::Off => Some(Pick::Off),
        PickedSubtitles::Stream(i) => info
            .subtitle_streams()
            .any(|s| s.index == *i)
            .then_some(Pick::Stream(*i)),
        PickedSubtitles::File(location) => files
            .iter()
            .position(|f| &f.location == location)
            .map(Pick::File),
    }
}

/// A track's name: its language, else its title, else its number.
pub fn track_name(s: &StreamInfo) -> String {
    Tag::parse(s.language.as_deref().unwrap_or(""))
        .code
        .and_then(crate::ui::language_name)
        .or_else(|| s.title.clone())
        .unwrap_or_else(|| tr!("tracks-track", number = s.index))
}

/// What tells an embedded subtitle stream apart besides its name.
pub fn stream_detail(s: &StreamInfo) -> String {
    let tag = stream_tag(s);
    let mut parts = Vec::new();
    let title = s.title.as_deref().filter(|t| !t.is_empty());
    if let Some(t) = title.filter(|t| tag.code.is_some() && !t.eq_ignore_ascii_case(&track_name(s)))
    {
        parts.push(t.to_string());
    }
    let said = title.unwrap_or("").to_ascii_lowercase();
    if tag.forced && !said.contains("forced") {
        parts.push(tr!("tracks-forced").into());
    }
    if tag.sdh && !said.contains("sdh") {
        parts.push("SDH".into());
    }
    parts.push(codec_name(&s.codec));
    parts.join(" · ")
}

/// A subtitle file's name and detail. The name is its language, else its
/// own name, else its place in the list (`n` counts from 1); the detail is
/// what tells it from the others: forced, SDH, its own name and its format.
pub fn file_label(f: &SubtitleFile, n: usize) -> (String, String) {
    let tag = Tag::parse(f.language.as_deref().unwrap_or(""));
    let own = f.name.trim();
    let said = own.to_ascii_lowercase();
    let mut parts = Vec::new();
    if tag.forced && !said.contains("forced") {
        parts.push(tr!("tracks-forced").to_string());
    }
    if tag.sdh && !said.contains("sdh") {
        parts.push("SDH".to_string());
    }
    let title = match tag.code.and_then(crate::ui::language_name) {
        Some(language) => {
            // A file named for its language in English or in the
            // interface's says nothing more.
            let english = tag.name().unwrap_or_default();
            if !own.is_empty()
                && !own.eq_ignore_ascii_case(&language)
                && !own.eq_ignore_ascii_case(english)
            {
                parts.push(own.to_string());
            }
            language
        }
        None if !own.is_empty() => own.to_string(),
        None => tr!("tracks-subtitles-file", number = n),
    };
    // A file name says its format; a title or no name at all doesn't.
    if !fp_core::source::is_subtitle_name(own)
        && let Some(format) = [own.to_string(), last_segment(&f.location)]
            .iter()
            .find_map(|s| text_format(s.rsplit_once('.')?.1))
    {
        parts.push(format.to_string());
    }
    (title, parts.join(" · "))
}

/// The name of a text subtitle format, by codec or file extension.
fn text_format(codec: &str) -> Option<&'static str> {
    Some(match codec.to_ascii_lowercase().as_str() {
        "subrip" | "srt" => "SRT",
        "ass" | "ssa" => "ASS",
        "webvtt" | "vtt" => "WebVTT",
        "mov_text" => tr!("tracks-text"),
        _ => return None,
    })
}

fn codec_name(codec: &str) -> String {
    if let Some(name) = text_format(codec) {
        return name.into();
    }
    match codec {
        "hdmv_pgs_subtitle" => "PGS",
        "dvd_subtitle" => "VobSub",
        "dvb_subtitle" => "DVB",
        other => return other.to_uppercase(),
    }
    .into()
}

/// A subtitle file or stream as the audio and subtitles picker lists it.
pub struct Row {
    pub pick: Pick,
    pub title: String,
    pub detail: String,
}

/// The video's subtitle files and embedded streams, labelled: those in
/// `language` first, then those in no known language, then the rest by
/// language name. Files come before streams in the same language.
pub fn rows(language: &str, info: &MediaInfo, files: &[SubtitleFile]) -> Vec<Row> {
    let mut rows: Vec<(Option<&str>, Row)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (title, detail) = file_label(f, i + 1);
            let code = Tag::parse(f.language.as_deref().unwrap_or("")).code;
            (
                code,
                Row {
                    pick: Pick::File(i),
                    title,
                    detail,
                },
            )
        })
        .collect();
    rows.extend(info.subtitle_streams().map(|s| {
        let row = Row {
            pick: Pick::Stream(s.index),
            title: track_name(s),
            detail: stream_detail(s),
        };
        (stream_tag(s).code, row)
    }));
    let group = |code: Option<&str>| match code {
        Some(c) if c == language => 0,
        None => 1,
        Some(_) => 2,
    };
    rows.sort_by_key(|(code, _)| (group(*code), code.and_then(fp_core::language::name)));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// Picture subtitles of a 3D video, which are drawn on the whole frame like
/// the video itself: each eye's copy in that eye's half, a little to the side
/// of the other for depth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoSubs {
    pub layout: StereoLayout,
    /// The frame the pictures are placed on: the video's size.
    pub canvas: (u32, u32),
    pub swap_eyes: bool,
}

impl StereoSubs {
    /// How a 3D video's picture subtitles are drawn. A 3D Blu-ray's or frame
    /// sequential video's are made for one eye's picture, not the two side by
    /// side it's decoded into, so they're drawn like a 2D video's, for both
    /// eyes.
    pub fn of(video: &StreamInfo, format: &VideoFormat, swap_eyes: bool) -> Option<StereoSubs> {
        let paired = video.mvc || video.frame_sequential;
        (format.stereo.is_stereo() && !paired).then_some(StereoSubs {
            layout: format.stereo,
            canvas: (video.width, video.height),
            swap_eyes: format.eyes_swapped ^ swap_eyes,
        })
    }

    /// The size of an eye's half of the frame, and how many times wider and
    /// taller it shows (a half side-by-side eye is squeezed to half width).
    pub fn half(&self) -> ((u32, u32), (f32, f32)) {
        let (w, h) = self.canvas;
        match self.layout {
            StereoLayout::Mono => ((w, h), (1.0, 1.0)),
            StereoLayout::SideBySide => ((w / 2, h), (1.0, 1.0)),
            StereoLayout::HalfSideBySide => ((w / 2, h), (2.0, 1.0)),
            StereoLayout::TopBottom => ((w, h / 2), (1.0, 1.0)),
            StereoLayout::HalfTopBottom => ((w, h / 2), (1.0, 2.0)),
        }
    }

    /// The part of `b` in `eye`'s half (0 left, 1 right), placed within
    /// that half.
    pub fn part(&self, b: &SubBitmap, eye: usize) -> Option<SubBitmap> {
        let ((hw, hh), _) = self.half();
        let half = (eye ^ self.swap_eyes as usize) as i32;
        let (x0, y0) = match self.layout {
            StereoLayout::SideBySide | StereoLayout::HalfSideBySide => (half * hw as i32, 0),
            StereoLayout::TopBottom | StereoLayout::HalfTopBottom => (0, half * hh as i32),
            StereoLayout::Mono => (0, 0),
        };
        let left = b.x.max(x0);
        let top = b.y.max(y0);
        let right = (b.x + b.width as i32).min(x0 + hw as i32);
        let bottom = (b.y + b.height as i32).min(y0 + hh as i32);
        if left >= right || top >= bottom || b.rgba.len() < (b.width * b.height * 4) as usize {
            return None;
        }
        let (w, h) = ((right - left) as usize, (bottom - top) as usize);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for row in (top - b.y) as usize..(bottom - b.y) as usize {
            let start = (row * b.width as usize + (left - b.x) as usize) * 4;
            rgba.extend_from_slice(&b.rgba[start..start + w * 4]);
        }
        Some(SubBitmap {
            x: left - x0,
            y: top - y0,
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_media::StreamKind;

    fn stream(index: usize, kind: StreamKind, language: Option<&str>) -> StreamInfo {
        StreamInfo {
            index,
            kind,
            codec: "subrip".into(),
            language: language.map(str::to_string),
            title: None,
            default: false,
            forced: false,
            hearing_impaired: false,
            width: 0,
            height: 0,
            fps: 0.0,
            bit_depth: 8,
            pixel_format: String::new(),
            interlaced: false,
            transfer: Default::default(),
            rotation: 0,
            pixel_aspect: 1.0,
            mvc: false,
            frame_sequential: false,
            sample_rate: 0,
            channels: 0,
            ambisonic: false,
        }
    }

    /// A video with `audio` and embedded subtitles from index 2.
    fn video(audio: Option<&str>, subs: Vec<StreamInfo>) -> MediaInfo {
        let mut streams = vec![
            stream(0, StreamKind::Video, None),
            stream(1, StreamKind::Audio, audio),
        ];
        streams.extend(subs);
        MediaInfo {
            name: "Movie.mkv".into(),
            container: "matroska".into(),
            duration: 60.0,
            streams,
            chapters: Vec::new(),
            hints: Default::default(),
            video: Some(0),
            audio: Some(1),
            title: None,
        }
    }

    fn sub(index: usize, language: &str) -> StreamInfo {
        stream(index, StreamKind::Subtitle, Some(language))
    }

    fn file(name: &str, language: Option<&str>) -> SubtitleFile {
        SubtitleFile {
            name: name.into(),
            location: format!("/v/{name}"),
            language: language.map(str::to_string),
        }
    }

    use SubtitleMode::{Always, Automatic, Off};

    #[test]
    fn subtitles_turn_off_and_back_on() {
        let info = video(Some("fra"), vec![sub(2, "fra"), sub(3, "eng")]);
        let toggle = |showing, last| toggled(showing, last, "en", &info, &[], "Movie.mkv");
        assert_eq!(toggle(Pick::Stream(2), None), Some(Pick::Off));
        // Back on: those turned off, else those Always would choose.
        assert_eq!(
            toggle(Pick::Off, Some(Pick::Stream(2))),
            Some(Pick::Stream(2))
        );
        assert_eq!(toggle(Pick::Off, None), Some(Pick::Stream(3)));
        // None in your language: the first there are; none at all: nothing.
        let german = video(Some("fra"), vec![sub(2, "deu")]);
        let first = toggled(Pick::Off, None, "en", &german, &[], "Movie.mkv");
        assert_eq!(first, Some(Pick::Stream(2)));
        let none = video(Some("fra"), vec![]);
        assert_eq!(
            toggled(Pick::Off, None, "en", &none, &[], "Movie.mkv"),
            None
        );
    }

    #[test]
    fn what_was_never_picked_follows_the_settings() {
        let mut forced = sub(3, "eng");
        forced.forced = true;
        let info = video(Some("eng"), vec![sub(2, "eng"), forced]);
        let start = |picked, mode| starting(picked, None, mode, "en", &info, &[], "Movie.mkv");
        // Watched with subtitles off in Settings, none picked: once Settings
        // say Automatic, the forced ones come on.
        assert_eq!(start(None, Off), Pick::Off);
        assert_eq!(start(None, Automatic), Pick::Stream(3));
        // Turned off in the player: they stay off whatever Settings say.
        let off = PickedSubtitles::Off;
        assert_eq!(start(Some(&off), Always), Pick::Off);
        assert_eq!(
            start(Some(&PickedSubtitles::Stream(2)), Off),
            Pick::Stream(2)
        );
    }

    #[test]
    fn a_pick_that_is_gone_follows_the_settings() {
        let info = video(Some("jpn"), vec![sub(2, "eng")]);
        let files = [file("Movie.ar.srt", Some("ar"))];
        let start = |picked: &PickedSubtitles| {
            starting(Some(picked), None, Automatic, "en", &info, &files, "M.mkv")
        };
        let ar = saved(Pick::File(0), &files).unwrap();
        assert_eq!(ar, PickedSubtitles::File("/v/Movie.ar.srt".into()));
        assert_eq!(start(&ar), Pick::File(0));
        // The file was removed, the stream isn't in this version of it.
        assert_eq!(
            start(&PickedSubtitles::File("/v/Movie.fr.srt".into())),
            Pick::Stream(2)
        );
        assert_eq!(start(&PickedSubtitles::Stream(7)), Pick::Stream(2));
        assert_eq!(saved(Pick::File(4), &files), None);
    }

    #[test]
    fn subtitles_follow_the_audio_picked() {
        // Picked the Japanese audio of an English film: Automatic gives
        // English subtitles, as for a Japanese one.
        let mut info = video(Some("eng"), vec![sub(3, "eng")]);
        info.streams
            .insert(2, stream(2, StreamKind::Audio, Some("jpn")));
        let start = |audio| starting(None, audio, Automatic, "en", &info, &[], "Movie.mkv");
        assert_eq!(start(None), Pick::Off);
        assert_eq!(start(Some(2)), Pick::Stream(3));
        assert_eq!(start(Some(9)), Pick::Off, "no such audio");
    }

    #[test]
    fn a_first_track_in_another_language_is_not_shown() {
        let info = video(Some("eng"), vec![sub(2, "ara"), sub(3, "eng")]);
        assert_eq!(choose(Automatic, "en", &info, &[], "Movie.mkv"), Pick::Off);
        assert_eq!(
            choose(Always, "en", &info, &[], "Movie.mkv"),
            Pick::Stream(3)
        );
        assert_eq!(choose(Off, "en", &info, &[], "Movie.mkv"), Pick::Off);
    }

    #[test]
    fn your_language_when_the_audio_is_foreign() {
        let info = video(Some("jpn"), vec![sub(2, "ara"), sub(3, "eng")]);
        assert_eq!(
            choose(Automatic, "en", &info, &[], "Movie.mkv"),
            Pick::Stream(3)
        );
        // Unknown audio: nothing to go on.
        let info = video(None, vec![sub(2, "eng")]);
        assert_eq!(choose(Automatic, "en", &info, &[], "Movie.mkv"), Pick::Off);
    }

    #[test]
    fn forced_subtitles_for_the_audio() {
        let mut forced = sub(3, "eng");
        forced.forced = true;
        let mut foreign_forced = sub(4, "ger");
        foreign_forced.forced = true;
        let info = video(Some("eng"), vec![sub(2, "eng"), foreign_forced, forced]);
        assert_eq!(
            choose(Automatic, "en", &info, &[], "Movie.mkv"),
            Pick::Stream(3)
        );
        // Always wants full subtitles over forced ones.
        assert_eq!(
            choose(Always, "en", &info, &[], "Movie.mkv"),
            Pick::Stream(2)
        );
        let mut titled = sub(2, "eng");
        titled.title = Some("English (Forced)".into());
        let info = video(Some("eng"), vec![titled]);
        assert_eq!(
            choose(Automatic, "en", &info, &[], "Movie.mkv"),
            Pick::Stream(2)
        );
    }

    #[test]
    fn files() {
        let info = video(Some("eng"), vec![sub(2, "eng")]);
        let files = [file("Movie.ar.srt", Some("ar")), file("Movie.srt", None)];
        // The untagged file beside the video, not the Arabic one.
        assert_eq!(
            choose(Automatic, "en", &info, &files, "Movie.mkv"),
            Pick::File(1)
        );
        let files = [
            file("Movie.ar.srt", Some("ar")),
            file("Movie.en.srt", Some("en")),
        ];
        assert_eq!(
            choose(Always, "en", &info, &files, "Movie.mkv"),
            Pick::File(1)
        );
        assert_eq!(
            choose(Always, "ar", &info, &files, "Movie.mkv"),
            Pick::File(0)
        );
        // A file named after something else isn't the video's own.
        let files = [file("Stream.subrip", None)];
        assert_eq!(
            choose(Automatic, "en", &info, &files, "Movie.mkv"),
            Pick::Off
        );
    }

    #[test]
    fn sdh_and_default_break_ties() {
        let mut sdh = sub(2, "eng");
        sdh.hearing_impaired = true;
        let mut marked = sub(4, "eng");
        marked.default = true;
        let info = video(Some("jpn"), vec![sdh, sub(3, "eng"), marked]);
        assert_eq!(
            choose(Automatic, "en", &info, &[], "Movie.mkv"),
            Pick::Stream(4)
        );
    }

    #[test]
    fn labels() {
        let mut s = sub(3, "ara");
        s.title = Some("Arabic".into());
        assert_eq!(
            (track_name(&s), stream_detail(&s)),
            ("Arabic".into(), "SRT".into())
        );
        s.title = Some("Commentary".into());
        s.hearing_impaired = true;
        assert_eq!(stream_detail(&s), "Commentary · SDH · SRT");
        let s = stream(5, StreamKind::Subtitle, None);
        assert_eq!(track_name(&s), "Track 5");
        assert_eq!(
            file_label(&file("Movie.en.srt", Some("en")), 1),
            ("English".into(), "Movie.en.srt".into())
        );
        assert_eq!(
            file_label(&file("Movie.srt", None), 1),
            ("Movie.srt".into(), String::new())
        );
        // A server's files, named by its address alone or by a title.
        let served = |name: &str, codec: &str, language: &str| SubtitleFile {
            name: name.into(),
            location: format!("http://jf/Videos/1/m/Subtitles/4/Stream.{codec}"),
            language: Some(language.into()),
        };
        assert_eq!(
            file_label(&served("", "subrip", "ger.forced"), 1),
            ("German".into(), "Forced · SRT".into())
        );
        assert_eq!(
            file_label(&served("", "subrip", ".sdh"), 3),
            ("Subtitles 3".into(), "SDH · SRT".into())
        );
        assert_eq!(
            file_label(&served("Commentary", "ass", "eng"), 1),
            ("English".into(), "Commentary · ASS".into())
        );
    }

    #[test]
    fn rows_put_your_language_first() {
        let info = video(Some("eng"), vec![sub(2, "ara"), sub(3, "eng")]);
        let files = [
            file("Movie.fr.srt", Some("fr")),
            file("Movie.srt", None),
            file("Movie.en.srt", Some("en")),
        ];
        let order: Vec<_> = rows("en", &info, &files)
            .iter()
            .map(|r| (r.pick, r.title.clone()))
            .collect();
        assert_eq!(
            order,
            [
                (Pick::File(2), "English".into()),
                (Pick::Stream(3), "English".into()),
                (Pick::File(1), "Movie.srt".into()),
                (Pick::Stream(2), "Arabic".into()),
                (Pick::File(0), "French".into()),
            ]
        );
    }

    /// A picture `width` wide whose pixels hold their own x position.
    fn strip(x: i32, y: i32, width: u32, height: u32) -> SubBitmap {
        let rgba = (0..height)
            .flat_map(|_| (0..width).flat_map(|i| [(x + i as i32) as u8, 0, 0, 255]))
            .collect();
        SubBitmap {
            x,
            y,
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn a_3d_blu_rays_pictures_are_for_both_eyes() {
        use fp_core::format::Projection;
        let format = VideoFormat::new(Projection::Flat, StereoLayout::SideBySide);
        let mut video = stream(0, StreamKind::Video, None);
        (video.width, video.height) = (3840, 1080);
        assert!(StereoSubs::of(&video, &format, false).is_some());
        video.mvc = true;
        assert_eq!(StereoSubs::of(&video, &format, false), None);
        video.mvc = false;
        video.frame_sequential = true;
        assert_eq!(StereoSubs::of(&video, &format, false), None);
        video.frame_sequential = false;
        let mono = VideoFormat::new(Projection::Flat, StereoLayout::Mono);
        assert_eq!(StereoSubs::of(&video, &mono, false), None);
    }

    #[test]
    fn each_eye_gets_its_copy_of_a_3d_picture() {
        let sbs = StereoSubs {
            layout: StereoLayout::HalfSideBySide,
            canvas: (200, 100),
            swap_eyes: false,
        };
        // Both copies in one picture, across the middle.
        let b = strip(40, 80, 120, 10);
        let left = sbs.part(&b, 0).unwrap();
        let right = sbs.part(&b, 1).unwrap();
        assert_eq!((left.x, left.y, left.width, left.height), (40, 80, 60, 10));
        assert_eq!(
            (right.x, right.y, right.width, right.height),
            (0, 80, 60, 10)
        );
        assert_eq!(&left.rgba[..4], &[40, 0, 0, 255]);
        assert_eq!(&right.rgba[..4], &[100, 0, 0, 255]);
        assert_eq!(sbs.half(), ((100, 100), (2.0, 1.0)));

        let swapped = StereoSubs {
            swap_eyes: true,
            ..sbs
        };
        assert_eq!(swapped.part(&b, 0), Some(right));
        assert!(sbs.part(&strip(110, 0, 20, 5), 0).is_none());

        let tb = StereoSubs {
            layout: StereoLayout::HalfTopBottom,
            ..sbs
        };
        let b = strip(10, 40, 30, 20);
        assert_eq!(tb.part(&b, 0).map(|p| (p.y, p.height)), Some((40, 10)));
        assert_eq!(tb.part(&b, 1).map(|p| (p.y, p.height)), Some((0, 10)));
    }
}
