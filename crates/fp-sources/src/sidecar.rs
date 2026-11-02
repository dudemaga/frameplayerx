//! Matching haptic scripts and subtitles to a video by file name.
//!
//! For `Scene.mp4` in a folder:
//! - `Scene.funscript` is the main script, `Scene.<axis>.funscript` an extra
//!   axis of a multi-axis script (`Scene.roll.funscript`, `Scene.twist.funscript`,
//!   `Scene.L1.funscript`, ...);
//! - `Scene.srt` is a subtitle with no language, `Scene.<lang>.srt` one with a
//!   language tag (`Scene.en.srt`, `Scene.de.forced.ass`).
//!
//! Names compare case-insensitively. A file that matches another video in
//! the folder exactly (`Scene.part2.funscript` next to `Scene.part2.mp4`)
//! belongs to that video, not to `Scene.mp4`.

pub use fp_core::source::SubtitleFile;
use fp_core::source::{Entry, EntryKind, is_script_name, is_subtitle_name, is_video_name};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A haptic script belonging to a video.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFile {
    /// File name or title.
    pub name: String,
    /// Location to open with the same source (or a URL for feeds).
    pub location: String,
    /// Axis of a multi-axis script (`roll`, `twist`, `L1`, ...); `None` for
    /// the main stroke script.
    #[serde(default)]
    pub axis: Option<String>,
}

/// Scripts and subtitles found for one video.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecars {
    /// Main script first, then extra axes by name.
    pub scripts: Vec<ScriptFile>,
    /// Untagged subtitle first, then by language.
    pub subtitles: Vec<SubtitleFile>,
}

fn strip_ext(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// If `candidate` is `stem` or `stem.<suffix>` (case-insensitive), returns
/// `Some(None)` or `Some(Some(suffix))`.
fn suffix_after_stem<'a>(candidate: &'a str, stem: &str) -> Option<Option<&'a str>> {
    if candidate.eq_ignore_ascii_case(stem) {
        return Some(None);
    }
    let head = candidate.get(..stem.len())?;
    let rest = candidate.get(stem.len()..)?;
    if head.eq_ignore_ascii_case(stem)
        && let Some(sfx) = rest.strip_prefix('.')
        && !sfx.is_empty()
    {
        return Some(Some(sfx));
    }
    None
}

/// The subtitle file at `location` for the video named `video_name`, its
/// language tag read from what follows the video's name (`Movie.en.srt`,
/// `Movie_es.vtt`).
pub fn subtitle_file(location: &str, video_name: &str) -> SubtitleFile {
    let name = crate::urlutil::last_segment(location);
    let stem = strip_ext(video_name);
    let file_stem = strip_ext(&name);
    let language = file_stem
        .get(..stem.len())
        .filter(|head| !stem.is_empty() && head.eq_ignore_ascii_case(stem))
        .and_then(|_| file_stem.get(stem.len()..))
        .map(|rest| rest.trim_start_matches(['.', '_', '-', ' ']))
        .filter(|tag| !tag.is_empty())
        .map(str::to_string);
    SubtitleFile {
        name,
        location: location.to_string(),
        language,
    }
}

impl Sidecars {
    /// Wraps the scripts and subtitles an entry already carries (from a
    /// feed, a server or DLNA metadata), deriving axes, and languages the
    /// source didn't give, from the file names.
    pub fn from_entry(video: &Entry) -> Sidecars {
        let mut video_name = crate::urlutil::last_segment(&video.location);
        if video_name.is_empty() {
            video_name = video.name.clone();
        }
        let stem = strip_ext(&video_name).to_string();
        let mut out = Sidecars::default();
        for loc in &video.scripts {
            let name = crate::urlutil::last_segment(loc);
            let axis = suffix_after_stem(strip_ext(&name), &stem)
                .flatten()
                .filter(|a| !a.contains('.'))
                .map(str::to_string);
            out.scripts.push(ScriptFile {
                name,
                location: loc.clone(),
                axis,
            });
        }
        for s in &video.subtitles {
            let mut s = s.clone();
            if s.language.is_none() {
                s.language = subtitle_file(&s.location, &video_name).language;
            }
            out.subtitles.push(s);
        }
        out
    }

    /// True when nothing was found.
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty() && self.subtitles.is_empty()
    }

    /// Adds the files of `other` that are not already present (by location).
    pub fn merge(&mut self, other: Sidecars) {
        for s in other.scripts {
            if !self.scripts.iter().any(|x| x.location == s.location) {
                self.scripts.push(s);
            }
        }
        for s in other.subtitles {
            if !self.subtitles.iter().any(|x| x.location == s.location) {
                self.subtitles.push(s);
            }
        }
        self.sort();
    }

    fn sort(&mut self) {
        self.scripts.sort_by(|a, b| {
            a.axis
                .is_some()
                .cmp(&b.axis.is_some())
                .then_with(|| a.axis.cmp(&b.axis))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.subtitles.sort_by(|a, b| {
            a.language
                .is_some()
                .cmp(&b.language.is_some())
                .then_with(|| a.language.cmp(&b.language))
                .then_with(|| a.name.cmp(&b.name))
        });
    }
}

/// Picks the scripts and subtitles for the video named `video_name` among
/// the entries of its folder.
pub fn match_sidecars(video_name: &str, siblings: &[Entry]) -> Sidecars {
    let stem = strip_ext(video_name);
    // Stems of the other videos: their exact sidecars are not ours.
    let other_stems: HashSet<String> = siblings
        .iter()
        .filter(|e| e.kind != EntryKind::Directory && is_video_name(&e.name))
        .map(|e| strip_ext(&e.name).to_lowercase())
        .filter(|s| !s.eq_ignore_ascii_case(stem))
        .collect();
    let mut out = Sidecars::default();
    for e in siblings.iter().filter(|e| e.kind != EntryKind::Directory) {
        let cand = strip_ext(&e.name);
        let Some(suffix) = suffix_after_stem(cand, stem) else {
            continue;
        };
        if suffix.is_some() && other_stems.contains(&cand.to_lowercase()) {
            continue;
        }
        if is_script_name(&e.name) {
            // Axis names are single tokens; `a.b.c.funscript` is not ours.
            if suffix.is_some_and(|s| s.contains('.')) {
                continue;
            }
            out.scripts.push(ScriptFile {
                name: e.name.clone(),
                location: e.location.clone(),
                axis: suffix.map(str::to_string),
            });
        } else if is_subtitle_name(&e.name) {
            out.subtitles.push(SubtitleFile {
                name: e.name.clone(),
                location: e.location.clone(),
                language: suffix.map(str::to_string),
            });
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_entry;

    fn folder(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| file_entry(*n, format!("/v/{n}"), None, None))
            .collect()
    }

    #[test]
    fn owned_sidecars_keep_axes_and_subtitle_languages() {
        let uri =
            |file: &str| crate::location::encode("folder", "http", &format!("folder/{file}?v=2"));
        let mut entry = Entry::new("Movie", uri("Movie%20one.mp4"), EntryKind::Video);
        entry.scripts.push(uri("Movie%20one.roll.funscript"));
        entry
            .subtitles
            .push(subtitle_file(&uri("Movie%20one.en.srt"), "Movie one.mp4"));
        let sidecars = Sidecars::from_entry(&entry);
        assert_eq!(sidecars.scripts[0].name, "Movie one.roll.funscript");
        assert_eq!(sidecars.scripts[0].axis.as_deref(), Some("roll"));
        assert_eq!(sidecars.subtitles[0].language.as_deref(), Some("en"));
    }

    #[test]
    fn single_and_multi_axis_scripts() {
        let f = folder(&[
            "Scene_180_LR.mp4",
            "Scene_180_LR.funscript",
            "scene_180_lr.roll.funscript",
            "Scene_180_LR.twist.funscript",
            "Scene_180_LR.L1.funscript",
            "Scene_180_LR.a.b.funscript",
            "Other.funscript",
            "Scene_180_LR_extra.funscript",
        ]);
        let s = match_sidecars("Scene_180_LR.mp4", &f);
        let got: Vec<_> = s
            .scripts
            .iter()
            .map(|x| (x.name.as_str(), x.axis.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("Scene_180_LR.funscript", None),
                ("Scene_180_LR.L1.funscript", Some("L1")),
                ("scene_180_lr.roll.funscript", Some("roll")),
                ("Scene_180_LR.twist.funscript", Some("twist")),
            ]
        );
        assert_eq!(s.scripts[0].location, "/v/Scene_180_LR.funscript");
    }

    #[test]
    fn subtitles_with_languages() {
        let f = folder(&[
            "Movie.mkv",
            "Movie.srt",
            "Movie.en.srt",
            "movie.de.forced.ass",
            "Movie.vtt",
            "Moviex.srt",
            "Movie.nfo",
        ]);
        let s = match_sidecars("Movie.mkv", &f);
        let got: Vec<_> = s
            .subtitles
            .iter()
            .map(|x| (x.name.as_str(), x.language.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("Movie.srt", None),
                ("Movie.vtt", None),
                ("movie.de.forced.ass", Some("de.forced")),
                ("Movie.en.srt", Some("en")),
            ]
        );
        assert!(s.scripts.is_empty());
    }

    #[test]
    fn other_videos_keep_their_sidecars() {
        let f = folder(&[
            "Scene.mp4",
            "Scene.part2.mp4",
            "Scene.funscript",
            "Scene.part2.funscript",
            "Scene.part2.srt",
            "Scene.roll.funscript",
        ]);
        let s = match_sidecars("Scene.mp4", &f);
        let names: Vec<_> = s.scripts.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Scene.funscript", "Scene.roll.funscript"]);
        assert!(s.subtitles.is_empty());
        let s = match_sidecars("Scene.part2.mp4", &f);
        let names: Vec<_> = s.scripts.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Scene.part2.funscript"]);
        assert_eq!(s.subtitles.len(), 1);
    }

    #[test]
    fn unicode_names() {
        let f = folder(&[
            "Szene ü 日本.mp4",
            "Szene ü 日本.funscript",
            "Szene ü 日本.ja.srt",
        ]);
        let s = match_sidecars("Szene ü 日本.mp4", &f);
        assert_eq!(s.scripts.len(), 1);
        assert_eq!(s.subtitles[0].language.as_deref(), Some("ja"));
    }

    #[test]
    fn subtitle_language_from_name() {
        let lang = |loc: &str, video: &str| subtitle_file(loc, video).language;
        assert_eq!(lang("/v/Movie.en.srt", "Movie.mkv").as_deref(), Some("en"));
        assert_eq!(lang("/v/movie_es.vtt", "Movie.mkv").as_deref(), Some("es"));
        assert_eq!(
            lang("http://h/Movie%20One.de.forced.ass", "Movie One.mp4").as_deref(),
            Some("de.forced")
        );
        assert_eq!(lang("/v/Movie.srt", "Movie.mkv"), None);
        assert_eq!(lang("/v/Other.en.srt", "Movie.mkv"), None);
        assert_eq!(
            subtitle_file("http://h/Movie%20One.srt", "").name,
            "Movie One.srt"
        );
    }

    #[test]
    fn from_entry_and_merge() {
        let mut e = file_entry("Clip", "http://h/v/Clip.mp4", None, None);
        e.scripts = vec![
            "http://h/v/Clip.funscript".into(),
            "http://h/v/Clip.surge.funscript".into(),
        ];
        e.subtitles = vec![subtitle_file("http://h/v/Clip.en.srt", "")];
        let mut s = Sidecars::from_entry(&e);
        assert_eq!(s.scripts[1].axis.as_deref(), Some("surge"));
        assert_eq!(s.subtitles[0].language.as_deref(), Some("en"));
        let extra = Sidecars {
            scripts: vec![ScriptFile {
                name: "Clip.funscript".into(),
                location: "http://h/v/Clip.funscript".into(),
                axis: None,
            }],
            subtitles: vec![SubtitleFile {
                name: "Clip.srt".into(),
                location: "http://h/v/Clip.srt".into(),
                language: None,
            }],
        };
        s.merge(extra);
        assert_eq!(s.scripts.len(), 2);
        assert_eq!(s.subtitles[0].name, "Clip.srt");
        assert!(!s.is_empty());
    }
}
