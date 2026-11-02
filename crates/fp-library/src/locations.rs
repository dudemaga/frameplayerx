//! Source-scoped migration of persistent media and asset locations.

use crate::{Error, Library, Result};
use rusqlite::{OptionalExtension, params};

struct Locations {
    id: i64,
    media: String,
    scripts: String,
    subtitles: String,
    thumbnail: Option<String>,
    tracks: Option<String>,
}

fn rewrite_one(
    value: &mut String,
    rewrite: &mut impl FnMut(&str) -> Option<String>,
) -> Result<bool> {
    let Some(new) = rewrite(value).filter(|new| new != value) else {
        return Ok(false);
    };
    if new.is_empty() {
        return Err(Error::InvalidArgument(
            "a rewritten media or asset location cannot be empty".into(),
        ));
    }
    *value = new;
    Ok(true)
}

fn rewrite_list(
    value: &mut String,
    rewrite: &mut impl FnMut(&str) -> Option<String>,
) -> Result<bool> {
    let mut entries: Vec<String> = serde_json::from_str(value)?;
    let mut changed = false;
    for entry in &mut entries {
        changed |= rewrite_one(entry, rewrite)?;
    }
    if changed {
        *value = serde_json::to_string(&entries)?;
    }
    Ok(changed)
}

fn rewrite_tracks(
    value: &str,
    rewrite: &mut impl FnMut(&str) -> Option<String>,
) -> Result<Option<String>> {
    // Preserve unrelated fields, including fields a later app may have added.
    let mut tracks: serde_json::Value = serde_json::from_str(value)?;
    let Some(serde_json::Value::String(file)) = tracks
        .get_mut("subtitles")
        .and_then(|subtitles| subtitles.get_mut("file"))
    else {
        return Ok(None);
    };
    if rewrite_one(file, rewrite)? {
        Ok(Some(serde_json::to_string(&tracks)?))
    } else {
        Ok(None)
    }
}

impl Library {
    /// Rewrites the persistent locations of videos owned by `source_id`.
    ///
    /// `rewrite` returns a replacement for locations it recognizes, or `None`
    /// to leave them alone. It also visits scripts, subtitle files, server
    /// thumbnails and the selected subtitle file. It runs while the database
    /// is locked, so it must not perform I/O or call back into the library.
    ///
    /// The transaction preserves media IDs, history, playlists and all other
    /// user data. A row whose new location another row already has keeps its
    /// own, and still opens by it; the rest of the source moves. An empty
    /// replacement aborts the entire rewrite. No records are merged or
    /// deleted. Returns the number of media rows with any changed location.
    pub fn rewrite_source_locations(
        &self,
        source_id: &str,
        mut rewrite: impl FnMut(&str) -> Option<String>,
    ) -> Result<usize> {
        if source_id.is_empty() {
            return Err(Error::InvalidArgument(
                "cannot rewrite locations without a source".into(),
            ));
        }
        self.in_tx(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, location, scripts, subtitles, thumbnail_url, tracks
                 FROM media WHERE source_id = ?1 ORDER BY id",
            )?;
            let rows = statement
                .query_map([source_id], |row| {
                    Ok(Locations {
                        id: row.get(0)?,
                        media: row.get(1)?,
                        scripts: row.get(2)?,
                        subtitles: row.get(3)?,
                        thumbnail: row.get(4)?,
                        tracks: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut count = 0;
            for mut row in rows {
                let original = row.media.clone();
                let mut media_changed = rewrite_one(&mut row.media, &mut rewrite)?;
                if media_changed {
                    let collision: Option<i64> = connection
                        .query_row(
                            "SELECT id FROM media WHERE location = ?1 AND id <> ?2",
                            params![row.media, row.id],
                            |record| record.get(0),
                        )
                        .optional()?;
                    if collision.is_some() {
                        row.media = original;
                        media_changed = false;
                    }
                }
                let mut changed = media_changed;
                changed |= rewrite_list(&mut row.scripts, &mut rewrite)?;
                changed |= rewrite_list(&mut row.subtitles, &mut rewrite)?;
                if let Some(thumbnail) = &mut row.thumbnail {
                    changed |= rewrite_one(thumbnail, &mut rewrite)?;
                }
                if let Some(tracks) = &mut row.tracks
                    && let Some(new) = rewrite_tracks(tracks, &mut rewrite)?
                {
                    *tracks = new;
                    changed = true;
                }
                if changed {
                    connection.execute(
                        "UPDATE media SET location = ?2, scripts = ?3, subtitles = ?4,
                             thumbnail_url = ?5, tracks = ?6 WHERE id = ?1",
                        params![
                            row.id,
                            row.media,
                            row.scripts,
                            row.subtitles,
                            row.thumbnail,
                            row.tracks
                        ],
                    )?;
                    count += 1;
                }
            }
            Ok(count)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hidden, MediaId, MediaUpsert, PickedSubtitles, PickedTracks};
    use fp_core::format::{Projection, StereoLayout, VideoFormat};
    use fp_core::view::Keyframes;
    use fp_core::{Entry, EntryKind, ViewSettings};

    const OLD: &str = "http://old-server/videos/";

    fn stable(location: &str) -> Option<String> {
        location
            .strip_prefix(OLD)
            .map(|path| format!("source://source/server/{path}"))
    }

    fn add(library: &Library, source: &str, name: &str) -> MediaId {
        library
            .upsert(&MediaUpsert::new(format!("{OLD}{name}.mkv"), source))
            .unwrap()
            .id
    }

    #[test]
    fn rewriting_locations_preserves_media_identity_and_every_attached_record() {
        let library = Library::open_in_memory().unwrap();
        let mut upsert = MediaUpsert::new(format!("{OLD}movie.mkv"), "server");
        upsert.scripts = vec![
            format!("{OLD}movie.funscript"),
            "/local/script.funscript".into(),
        ];
        upsert.subtitles = vec![format!("{OLD}movie.srt")];
        upsert.thumbnail_url = Some(format!("{OLD}movie.jpg"));
        upsert.source_markers = Some(vec![(5.0, "Chapter".into())]);
        upsert.size = Some(1234);
        upsert.mtime = Some(77);
        upsert.duration = Some(100.0);
        let id = library.upsert(&upsert).unwrap().id;
        library.set_rating(id, 5).unwrap();
        library.set_favorite(id, true).unwrap();
        library.set_tags(id, &["Favorite scene", "VR"]).unwrap();
        library
            .set_user_format(
                id,
                Some(VideoFormat::new(
                    Projection::EQUIRECT_180,
                    StereoLayout::SideBySide,
                )),
            )
            .unwrap();
        let settings = ViewSettings {
            zoom: 1.3,
            ..Default::default()
        };
        library.set_view_settings(id, Some(&settings)).unwrap();
        let mut keyframes = Keyframes::default();
        keyframes.insert(3.0, settings);
        library.set_keyframes(id, &keyframes).unwrap();
        library.add_marker(id, 7.0, "Bookmark").unwrap();
        library
            .set_tracks(
                id,
                &PickedTracks {
                    audio: Some(2),
                    subtitles: Some(PickedSubtitles::File(format!("{OLD}movie.srt"))),
                },
            )
            .unwrap();
        let completed = library.start_playback(id).unwrap();
        library.update_playback(completed, 95.0, false).unwrap();
        let session = library.start_playback(id).unwrap();
        library.update_playback(session, 33.0, false).unwrap();
        let playlist = library.create_playlist("Tonight").unwrap();
        library.add_to_playlist(playlist, &[id, id]).unwrap();
        library.with(|connection| {
            connection.execute("UPDATE media SET thumbnail = '/cache/thumb.jpg', resume_thumbnail = '/cache/resume.jpg', resume_thumbnail_at = 33, probed = 1, probe_error = 'kept', missing = 1 WHERE id = ?1", [id.0])?;
            Ok(())
        }).unwrap();
        let before = library.get(id).unwrap().unwrap();
        let history = library.history(20).unwrap();
        let markers = library.markers(id).unwrap();

        assert_eq!(
            library.rewrite_source_locations("server", stable).unwrap(),
            1
        );
        let mut expected = before;
        expected.location = stable(&expected.location).unwrap();
        expected.scripts[0] = stable(&expected.scripts[0]).unwrap();
        expected.subtitles[0] = stable(&expected.subtitles[0]).unwrap();
        expected.thumbnail_url = expected.thumbnail_url.and_then(|url| stable(&url));
        expected.tracks.subtitles = Some(PickedSubtitles::File(expected.subtitles[0].clone()));
        assert_eq!(library.get(id).unwrap(), Some(expected.clone()));
        assert_eq!(
            library.get_by_location(&expected.location).unwrap(),
            Some(expected)
        );
        assert!(library.get_by_location(&upsert.location).unwrap().is_none());
        assert_eq!(library.history(20).unwrap(), history);
        assert_eq!(library.markers(id).unwrap(), markers);
        let items = library.playlist_items(playlist).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|entry| entry.record.id)
                .collect::<Vec<_>>(),
            [id, id]
        );
        assert_eq!(
            library.rewrite_source_locations("server", stable).unwrap(),
            0
        );
    }

    #[test]
    fn rewriting_is_scoped_and_includes_locked_sources_without_claiming_detached_records() {
        let library = Library::open_in_memory().unwrap();
        let chosen = add(&library, "server", "a");
        let other = add(&library, "other-source", "b");
        let removed = add(&library, "removed-source", "c");
        let detached = add(&library, "", "d");
        library.set_hidden(Hidden {
            sources: vec!["server".into()],
        });
        let untouched = [other, removed, detached].map(|id| library.get(id).unwrap().unwrap());
        assert_eq!(
            library.rewrite_source_locations("server", stable).unwrap(),
            1
        );
        assert_eq!(
            library.get(chosen).unwrap().unwrap().location,
            stable(&format!("{OLD}a.mkv")).unwrap()
        );
        for record in untouched {
            assert_eq!(library.get(record.id).unwrap(), Some(record));
        }
        assert!(library.rewrite_source_locations("", stable).is_err());
        assert_eq!(
            library
                .rewrite_source_locations("missing-source", stable)
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_collision_keeps_that_row_and_moves_the_rest() {
        for existing_target in [false, true] {
            let library = Library::open_in_memory().unwrap();
            let first = add(&library, "server", "a");
            let second = add(&library, "server", "b");
            let third = add(&library, "server", "c");
            library.set_favorite(first, true).unwrap();
            let session = library.start_playback(second).unwrap();
            library.update_playback(session, 13.0, false).unwrap();
            let target = "source://source/server/collision.mkv";
            let other = existing_target.then(|| {
                library
                    .upsert(&MediaUpsert::new(target, "other-source"))
                    .unwrap()
                    .id
            });
            let kept = library.get(second).unwrap().unwrap();
            let history = library.history(20).unwrap();
            let rewrite = |location: &str| {
                if location == format!("{OLD}b.mkv") {
                    Some(if existing_target {
                        target.into()
                    } else {
                        stable(&format!("{OLD}a.mkv")).unwrap()
                    })
                } else {
                    stable(location)
                }
            };
            assert_eq!(
                library.rewrite_source_locations("server", rewrite).unwrap(),
                2,
                "one collision doesn't hold back the others"
            );
            let moved = library.get(first).unwrap().unwrap();
            assert_eq!(moved.location, stable(&format!("{OLD}a.mkv")).unwrap());
            assert!(moved.favorite);
            assert_eq!(
                library.get(third).unwrap().unwrap().location,
                stable(&format!("{OLD}c.mkv")).unwrap()
            );
            assert_eq!(library.get(second).unwrap(), Some(kept.clone()));
            assert_eq!(
                library.get_by_location(&kept.location).unwrap().unwrap().id,
                second,
                "it still opens by the location it kept"
            );
            assert_eq!(library.history(20).unwrap().len(), history.len());
            if let Some(other) = other {
                assert_eq!(
                    library.get(other).unwrap().unwrap().source_id,
                    "other-source"
                );
            }
            assert_eq!(
                library.rewrite_source_locations("server", rewrite).unwrap(),
                0,
                "nothing to do the next time"
            );
        }
    }

    #[test]
    fn a_row_that_keeps_its_location_still_moves_its_files() {
        let library = Library::open_in_memory().unwrap();
        let target = "source://source/server/taken.mkv";
        library
            .upsert(&MediaUpsert::new(target, "other-source"))
            .unwrap();
        let mut upsert = MediaUpsert::new(format!("{OLD}b.mkv"), "server");
        upsert.subtitles = vec![format!("{OLD}b.srt")];
        let id = library.upsert(&upsert).unwrap().id;
        let changed = library
            .rewrite_source_locations("server", |location| {
                if location.ends_with(".mkv") {
                    Some(target.into())
                } else {
                    stable(location)
                }
            })
            .unwrap();
        assert_eq!(changed, 1);
        let row = library.get(id).unwrap().unwrap();
        assert_eq!(row.location, format!("{OLD}b.mkv"));
        assert_eq!(row.subtitles, [stable(&format!("{OLD}b.srt")).unwrap()]);
    }

    #[test]
    fn an_empty_replacement_rolls_back_every_location() {
        let library = Library::open_in_memory().unwrap();
        let id = add(&library, "server", "a");
        library
            .set_tracks(
                id,
                &PickedTracks {
                    audio: Some(1),
                    subtitles: Some(PickedSubtitles::File(format!("{OLD}a.srt"))),
                },
            )
            .unwrap();
        let before = library.get(id).unwrap();
        assert!(
            library
                .rewrite_source_locations("server", |location| {
                    if location.ends_with(".srt") {
                        Some(String::new())
                    } else {
                        stable(location)
                    }
                })
                .is_err()
        );
        assert_eq!(library.get(id).unwrap(), before);
    }

    #[test]
    fn asset_only_changes_preserve_unknown_track_fields_and_nullable_choices() {
        let library = Library::open_in_memory().unwrap();
        let id = library
            .upsert(&MediaUpsert::new(
                "source://source/server/movie.mkv",
                "server",
            ))
            .unwrap()
            .id;
        let untouched = library
            .upsert(&MediaUpsert::new(
                "source://source/server/other.mkv",
                "server",
            ))
            .unwrap()
            .id;
        library
            .with(|connection| {
                connection.execute(
                    "UPDATE media SET tracks = ?2 WHERE id = ?1",
                    params![
                        id.0,
                        serde_json::json!({
                            "audio":2,
                            "subtitles":{"file":format!("{OLD}movie.srt")},
                            "extra":{"keep":"unchanged"}
                        })
                        .to_string()
                    ],
                )?;
                Ok(())
            })
            .unwrap();
        let before = library.get(id).unwrap().unwrap();
        assert_eq!(
            library.rewrite_source_locations("server", stable).unwrap(),
            1
        );
        let after = library.get(id).unwrap().unwrap();
        assert_eq!(after.location, before.location);
        assert_eq!(after.tracks.audio, Some(2));
        assert_eq!(
            after.tracks.subtitles,
            Some(PickedSubtitles::File(
                "source://source/server/movie.srt".into()
            ))
        );
        library
            .with(|connection| {
                let tracks: String = connection.query_row(
                    "SELECT tracks FROM media WHERE id = ?1",
                    [id.0],
                    |row| row.get(0),
                )?;
                let tracks: serde_json::Value = serde_json::from_str(&tracks)?;
                assert_eq!(tracks["extra"]["keep"], "unchanged");
                let no_tracks: Option<String> = connection.query_row(
                    "SELECT tracks FROM media WHERE id = ?1",
                    [untouched.0],
                    |row| row.get(0),
                )?;
                assert!(no_tracks.is_none());
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn stable_server_thumbnails_are_kept_but_generated_files_are_not_source_assets() {
        for thumbnail in [
            "source://source/server/movie.jpg",
            "plex://source/server/machine/image/resource",
        ] {
            let mut entry = Entry::new(
                "Movie",
                "source://source/server/movie.mkv",
                EntryKind::Video,
            );
            entry.thumbnail_url = Some(thumbnail.into());
            assert_eq!(
                MediaUpsert::from_entry("server", &entry)
                    .unwrap()
                    .thumbnail_url
                    .as_deref(),
                Some(thumbnail)
            );
            entry.thumbnail_url = Some("/cache/generated.jpg".into());
            assert!(
                MediaUpsert::from_entry("server", &entry)
                    .unwrap()
                    .thumbnail_url
                    .is_none()
            );
        }
    }
}
