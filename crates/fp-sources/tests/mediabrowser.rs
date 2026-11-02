//! Integration tests for the Jellyfin/Emby MediaBrowser source.

mod common;

use common::{Req, TestServer, pattern, resp, serve_bytes};
use fp_core::format::{Projection, StereoLayout};
use fp_core::source::{Entry, EntryKind};
use fp_sources::{
    Credentials, MediaBrowserConfig, SourceConfig, SourceKind, WatchEvent, WatchKind, build,
};
use std::sync::Arc;

fn mediabrowser_config(url: String, creds: Option<Credentials>) -> MediaBrowserConfig {
    MediaBrowserConfig {
        id: "mb-test".into(),
        name: "Test Jellyfin".into(),
        url,
        credentials: creds,
        insecure_tls: false,
        max_height: None,
        access_token: None,
        user_id: None,
    }
}

#[test]
fn mediabrowser_full_lifecycle_browse_stream_and_progress() {
    let video_bytes = Arc::new(pattern(100_000));
    let v_bytes = video_bytes.clone();

    let server = TestServer::start(move |req: &Req| {
        let auth = req.header("authorization").unwrap_or("");

        // Public info (unauthed)
        if req.path() == "/System/Info/Public" {
            return resp(
                200,
                r#"{"ProductName":"Jellyfin Server","Version":"10.9.11"}"#,
                &[("Content-Type", "application/json")],
            );
        }

        // Login
        if req.path() == "/Users/AuthenticateByName" {
            assert!(auth.contains("MediaBrowser Client=\"FramePlayerX\""));
            assert!(req.body.contains("alice") && req.body.contains("secret123"));
            return resp(
                200,
                r#"{"AccessToken":"test_token_abc","User":{"Id":"usr_42","Name":"alice"}}"#,
                &[("Content-Type", "application/json")],
            );
        }

        // Must be authenticated for subsequent calls
        assert!(auth.contains("Token=\"test_token_abc\""));

        match req.path() {
            // Views (libraries)
            "/Users/usr_42/Views" => resp(
                200,
                r#"{"Items":[{"Id":"view_movies","Name":"Movies","Type":"CollectionFolder"},{"Id":"view_vr","Name":"VR Videos","Type":"CollectionFolder"}],"TotalRecordCount":2}"#,
                &[("Content-Type", "application/json")],
            ),
            // Items under view_vr
            "/Users/usr_42/Items" => {
                assert!(req.url.contains("ParentId=view_vr"));
                resp(
                    200,
                    r#"{"Items":[
                        {"Id":"folder_trips","Name":"Trips","Type":"Folder"},
                        {"Id":"item_rollercoaster","Name":"Rollercoaster","Type":"Movie","RunTimeTicks":600000000,"Path":"/media/VR/Rollercoaster_180_LR.mp4"}
                    ],"TotalRecordCount":2}"#,
                    &[("Content-Type", "application/json")],
                )
            }
            // Item details
            "/Users/usr_42/Items/item_rollercoaster" => resp(
                200,
                r#"{
                    "Id":"item_rollercoaster",
                    "Name":"Rollercoaster",
                    "Type":"Movie",
                    "RunTimeTicks":600000000,
                    "Path":"/media/VR/Rollercoaster_180_LR.mp4",
                    "UserData":{"PlaybackPositionTicks":123000000,"Played":false},
                    "MediaSources":[{
                        "Id":"ms_1",
                        "MediaStreams":[
                            {"Index":0,"Type":"Video","Width":3840,"Height":1920,"Codec":"hevc"},
                            {"Index":1,"Type":"Audio","Codec":"aac"},
                            {"Index":2,"Type":"Subtitle","Codec":"srt","Language":"eng","IsExternal":true}
                        ]
                    }],
                    "Chapters":[
                        {"StartPositionTicks":0,"Name":"Intro"},
                        {"StartPositionTicks":300000000,"Name":"Drop"}
                    ]
                }"#,
                &[("Content-Type", "application/json")],
            ),
            // Direct video streaming
            "/Videos/item_rollercoaster/stream" => serve_bytes(req, &v_bytes, true),
            // Playback session reporting
            "/Sessions/Playing" | "/Sessions/Playing/Progress" | "/Sessions/Playing/Stopped" => {
                resp(204, "", &[])
            }
            _ => resp(404, "", &[]),
        }
    });

    let config = mediabrowser_config(server.url(""), Some(Credentials::new("alice", "secret123")));
    let src = build(&SourceConfig::MediaBrowser(config)).unwrap();
    assert_eq!(src.kind(), SourceKind::MediaBrowser);

    // 1. List views (root)
    let views = src.list(None).unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0].name, "Movies");
    assert_eq!(views[0].kind, EntryKind::Directory);
    assert_eq!(views[1].name, "VR Videos");
    assert_eq!(views[1].kind, EntryKind::Directory);

    // 2. List items in VR Videos
    let vr_items = src.list(Some(&views[1].location)).unwrap();
    assert_eq!(vr_items.len(), 2);
    assert_eq!(vr_items[0].name, "Trips");
    assert_eq!(vr_items[0].kind, EntryKind::Directory);
    assert_eq!(vr_items[1].name, "Rollercoaster");
    assert_eq!(vr_items[1].kind, EntryKind::Video);
    assert_eq!(vr_items[1].duration, Some(60.0));

    // VR format detected from path
    let fmt = vr_items[1].format.unwrap();
    assert_eq!(fmt.projection, Projection::EQUIRECT_180);
    assert_eq!(fmt.stereo, StereoLayout::SideBySide);

    // 3. Get details
    let detailed = src.details(&vr_items[1]).unwrap();
    assert_eq!(detailed.markers.len(), 2);
    assert_eq!(detailed.markers[0], (0.0, "Intro".to_string()));
    assert_eq!(detailed.markers[1], (30.0, "Drop".to_string()));
    assert_eq!(detailed.subtitles.len(), 1);
    assert!(
        fp_sources::location::decode(&detailed.subtitles[0].location)
            .unwrap()
            .key
            .contains("Videos/item_rollercoaster/ms_1/Subtitles/2/Stream.srt")
    );

    // 4. Sidecars
    let sidecars = src.sidecars(&vr_items[1]).unwrap();
    assert_eq!(sidecars.subtitles.len(), 1);
    assert_eq!(sidecars.subtitles[0].language.as_deref(), Some("eng"));

    // 5. Open stream and random access read
    let f = src.open(&vr_items[1].location).unwrap();
    assert_eq!(f.size(), Some(100_000));
    let mut buf = [0u8; 100];
    assert_eq!(f.read_at(50_000, &mut buf).unwrap(), 100);
    assert_eq!(buf, video_bytes[50_000..50_100]);

    // 6. Watch sync: one session's reports, in the server's terms
    assert_eq!(detailed.resume, Some(12.3));
    let session = src.watch_session(&detailed).unwrap();
    assert_eq!(session.item, "item_rollercoaster");
    for (kind, position, paused) in [
        (WatchKind::Start, 0.0, false),
        (WatchKind::Tick, 10.0, false),
        (WatchKind::Pause, 15.0, true),
        (WatchKind::Resume, 15.0, false),
        (WatchKind::Seek, 40.0, false),
        (WatchKind::Stop, 60.0, false),
    ] {
        let event = WatchEvent {
            kind,
            position,
            paused,
            speed: 1.0,
        };
        src.report_watch(&session, event).unwrap();
    }
    let reports: Vec<serde_json::Value> = server
        .requests()
        .iter()
        .filter(|r| r.path().starts_with("/Sessions/Playing"))
        .map(|r| {
            let mut v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            v["path"] = r.path().into();
            v
        })
        .collect();
    let summary: Vec<_> = reports
        .iter()
        .map(|v| {
            (
                v["path"].as_str().unwrap(),
                v["EventName"].as_str().unwrap_or(""),
                v["PositionTicks"].as_i64().unwrap(),
                v["IsPaused"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("/Sessions/Playing", "", 0, false),
            (
                "/Sessions/Playing/Progress",
                "TimeUpdate",
                100_000_000,
                false
            ),
            ("/Sessions/Playing/Progress", "Pause", 150_000_000, true),
            ("/Sessions/Playing/Progress", "Unpause", 150_000_000, false),
            (
                "/Sessions/Playing/Progress",
                "TimeUpdate",
                400_000_000,
                false
            ),
            ("/Sessions/Playing/Stopped", "", 600_000_000, false),
        ]
    );
    assert!(
        reports
            .iter()
            .all(|v| v["ItemId"] == "item_rollercoaster"
                && v["PlaySessionId"] == session.id.as_str())
    );
}

#[test]
fn mediabrowser_search_finds_titles_by_kind_not_episodes() {
    let json = [("Content-Type", "application/json")];
    let server = TestServer::start(move |req: &Req| match req.path() {
        "/Users/AuthenticateByName" => resp(
            200,
            r#"{"AccessToken":"t","User":{"Id":"u1","Name":"alice"}}"#,
            &json,
        ),
        "/Users/u1/Items" => {
            // One request per section, with no order: the server ranks.
            assert!(req.url.contains("SearchTerm=alien&"), "{}", req.url);
            assert!(!req.url.contains("SortBy"), "{}", req.url);
            assert!(!req.url.contains("Episode"), "{}", req.url);
            let body = if req.url.contains("IncludeItemTypes=Movie&") {
                r#"{"Items":[{"Id":"m1","Name":"Alien","Type":"Movie"},
                             {"Id":"m2","Name":"Aliens","Type":"Movie"}]}"#
            } else if req.url.contains("IncludeItemTypes=Series&") {
                r#"{"Items":[{"Id":"s1","Name":"Alien: Earth","Type":"Series"}]}"#
            } else {
                r#"{"Items":[]}"#
            };
            resp(200, body, &json)
        }
        _ => resp(404, "", &[]),
    });
    let mb = fp_sources::MediaBrowserSource::new(&mediabrowser_config(
        server.url(""),
        Some(Credentials::new("alice", "secret123")),
    ))
    .unwrap();
    let catalog = fp_sources::Source::as_catalog(&mb).expect("a rich server catalog");
    assert!(catalog.capabilities().favorites);
    let found = catalog.search(" alien ", 50).unwrap();
    let sections: Vec<(&str, Vec<&str>)> = found
        .iter()
        .map(|(s, items)| (*s, items.iter().map(|i| i.name.as_str()).collect()))
        .collect();
    assert_eq!(
        sections,
        [
            ("Movies", vec!["Alien", "Aliens"]),
            ("Shows", vec!["Alien: Earth"])
        ]
    );
}

#[test]
fn mediabrowser_quick_connect_flow() {
    let server = TestServer::start(|req: &Req| match req.path() {
        "/QuickConnect/Enabled" => resp(200, "true", &[("Content-Type", "application/json")]),
        "/QuickConnect/Initiate" => resp(
            200,
            r#"{"Code":"849201","Secret":"sec_xyz123"}"#,
            &[("Content-Type", "application/json")],
        ),
        "/QuickConnect/Connect" => {
            if req.url.contains("sec_xyz123") {
                resp(
                    200,
                    r#"{"Authenticated":true,"Code":"849201","Secret":"sec_xyz123"}"#,
                    &[("Content-Type", "application/json")],
                )
            } else {
                resp(
                    200,
                    r#"{"Authenticated":false}"#,
                    &[("Content-Type", "application/json")],
                )
            }
        }
        "/Users/AuthenticateWithQuickConnect" => {
            assert!(req.body.contains("sec_xyz123"));
            resp(
                200,
                r#"{"AccessToken":"qc_token_99","User":{"Id":"usr_bob"}}"#,
                &[("Content-Type", "application/json")],
            )
        }
        _ => resp(404, "", &[]),
    });

    let config = mediabrowser_config(server.url(""), None);
    let mb = fp_sources::MediaBrowserSource::new(&config).unwrap();

    assert!(mb.quick_connect_enabled().unwrap());
    let init = mb.quick_connect_initiate().unwrap();
    assert_eq!(init.code, "849201");
    assert_eq!(init.secret, "sec_xyz123");

    let authed = mb.quick_connect_poll(&init.secret).unwrap();
    assert!(authed);

    let (token, uid) = mb.auth_credentials().unwrap();
    assert_eq!(token, "qc_token_99");
    assert_eq!(uid, "usr_bob");
}

/// Emby lists its tags at `/Tags` (it has no `/Items/Filters`), gives an
/// item's tags as `TagItems`, and has no Quick Connect.
#[test]
fn emby_tags_and_no_quick_connect() {
    let json = [("Content-Type", "application/json")];
    let server = TestServer::start(move |req: &Req| match req.path() {
        "/Users/AuthenticateByName" => resp(
            200,
            r#"{"AccessToken":"t","User":{"Id":"u1","Name":"frame"}}"#,
            &json,
        ),
        "/Tags" => resp(
            200,
            r#"{"Items":[{"Name":"Beach"},{"Name":"Sunset"}]}"#,
            &json,
        ),
        "/Users/u1/Items" if req.url.contains("&Tags=Beach") => resp(
            200,
            r#"{"Items":[{"Id":"9","Name":"Sunset Walk","Type":"Video",
                          "TagItems":[{"Name":"Beach","Id":13}]}]}"#,
            &json,
        ),
        "/Users/u1/Items" => resp(200, r#"{"Items":[]}"#, &json),
        _ => resp(404, "", &[]),
    });
    let mb = fp_sources::MediaBrowserSource::new(&mediabrowser_config(
        server.url(""),
        Some(Credentials::new("frame", "pw")),
    ))
    .unwrap();
    let found = mb.search("beach", 50).unwrap();
    let tagged: Vec<(&str, &[String])> = found
        .iter()
        .flat_map(|(s, items)| items.iter().map(move |i| (*s, i.tags.as_slice())))
        .collect();
    assert_eq!(tagged, [("Tagged", &["Beach".to_string()][..])]);

    let err = mb.quick_connect_initiate().unwrap_err().to_string();
    assert!(err.contains("Emby has none"), "{err}");
}

/// A Quick Connect sign-in the server revoked (the password changed, or the
/// device was signed out) can't be renewed: no password is kept.
#[test]
fn revoked_quick_connect_sign_in_says_sign_in_again() {
    let server = TestServer::start(|_| resp(401, "", &[]));
    let mut config = mediabrowser_config(server.url(""), None);
    config.access_token = Some("revoked".into());
    config.user_id = Some("usr_bob".into());
    let src = build(&SourceConfig::MediaBrowser(config)).unwrap();
    for _ in 0..2 {
        let err = src.list(None).unwrap_err();
        assert!(matches!(err, fp_sources::Error::SignedOut(_)), "{err}");
        assert!(err.to_string().contains("sign in again"), "{err}");
    }
}

/// A Jellyfin answering sign-in and `routes` (path, JSON), streaming
/// `video` for any `/Videos/.../stream`.
fn jellyfin(routes: Vec<(String, String)>, video: Arc<Vec<u8>>) -> TestServer {
    TestServer::start(move |req: &Req| {
        let json = |body: &str| resp(200, body, &[("Content-Type", "application/json")]);
        match req.path() {
            "/System/Info/Public" => {
                json(r#"{"ProductName":"Jellyfin Server","Version":"10.9.11"}"#)
            }
            "/Users/AuthenticateByName" => json(r#"{"AccessToken":"tok","User":{"Id":"u1"}}"#),
            p if p.starts_with("/Videos/") && p.ends_with("/stream") => {
                serve_bytes(req, &video, true)
            }
            p => match routes.iter().find(|(path, _)| path == p) {
                Some((_, body)) => json(body),
                None => resp(404, "", &[]),
            },
        }
    })
}

#[test]
fn max_height_picks_the_version_that_plays() {
    let item = r#"{"Id":"beach","Name":"Beach","Type":"Movie","MediaSources":[
        {"Id":"ms_8k","MediaStreams":[{"Index":0,"Type":"Video","Width":8192,"Height":4096}]},
        {"Id":"ms_4k","MediaStreams":[{"Index":0,"Type":"Video","Width":4096,"Height":2048}]}]}"#;
    let server = jellyfin(
        vec![("/Users/u1/Items/beach".into(), item.into())],
        Arc::new(pattern(10_000)),
    );
    let mut config = mediabrowser_config(server.url(""), Some(Credentials::new("a", "b")));
    config.max_height = Some(2160);
    let src = build(&SourceConfig::MediaBrowser(config)).unwrap();
    let f = src
        .open(&format!("mb:{}#item=beach", server.url("")))
        .unwrap();
    assert_eq!(f.size(), Some(10_000));
    let streams: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path() == "/Videos/beach/stream")
        .map(|r| r.url)
        .collect();
    assert!(!streams.is_empty());
    assert!(
        streams.iter().all(|u| u.contains("MediaSourceId=ms_4k")),
        "{streams:?}"
    );
}

#[test]
fn a_version_location_streams_that_version() {
    let item = r#"{"Id":"beach","Name":"Beach","Type":"Movie","MediaSources":[
        {"Id":"ms_8k","MediaStreams":[{"Index":0,"Type":"Video","Width":8192,"Height":4096}]},
        {"Id":"ms_4k","MediaStreams":[{"Index":0,"Type":"Video","Width":4096,"Height":2048}]}]}"#;
    let server = jellyfin(
        vec![("/Users/u1/Items/beach".into(), item.into())],
        Arc::new(pattern(10_000)),
    );
    let config = mediabrowser_config(server.url(""), Some(Credentials::new("a", "b")));
    let src = build(&SourceConfig::MediaBrowser(config)).unwrap();
    let mb = src.as_media_browser().unwrap();
    let plain = format!("mb:{}#item=beach", server.url(""));

    // The details name the version that plays: the tallest, else the one chosen.
    let entry = Entry::new("Beach", plain.clone(), EntryKind::Video);
    let tallest = src.details(&entry).unwrap();
    assert_eq!(
        tallest.location,
        src.canonical_location(&format!("{plain}&version=ms_8k"))
            .unwrap()
    );
    mb.set_version(&plain, Some("ms_4k"));
    let chosen = src.details(&entry).unwrap();
    assert_eq!(
        chosen.location,
        src.canonical_location(&format!("{plain}&version=ms_4k"))
            .unwrap()
    );

    // A version's location plays it, whatever is chosen now.
    mb.set_version(&plain, None);
    src.open(&chosen.location).unwrap();
    let streams: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path() == "/Videos/beach/stream")
        .map(|r| r.url)
        .collect();
    assert!(!streams.is_empty());
    assert!(
        streams.iter().all(|u| u.contains("MediaSourceId=ms_4k")),
        "{streams:?}"
    );
}

#[test]
fn folders_page_past_items_that_are_not_videos() {
    // A first page of nothing but photos, then the video.
    let photos: Vec<String> = (0..200)
        .map(|i| format!(r#"{{"Id":"p{i}","Name":"Photo {i}","Type":"Photo"}}"#))
        .collect();
    let server = TestServer::start(move |req: &Req| {
        let json = |body: String| resp(200, body, &[("Content-Type", "application/json")]);
        match req.path() {
            "/System/Info/Public" => {
                json(r#"{"ProductName":"Jellyfin Server","Version":"10.9.11"}"#.into())
            }
            "/Users/AuthenticateByName" => {
                json(r#"{"AccessToken":"tok","User":{"Id":"u1"}}"#.into())
            }
            "/Users/u1/Items" if req.url.contains("StartIndex=0") => json(format!(
                r#"{{"Items":[{}],"TotalRecordCount":201}}"#,
                photos.join(",")
            )),
            "/Users/u1/Items" => json(
                r#"{"Items":[{"Id":"v1","Name":"Trip","Type":"Video"}],"TotalRecordCount":201}"#
                    .into(),
            ),
            _ => resp(404, "", &[]),
        }
    });
    let config = mediabrowser_config(server.url(""), Some(Credentials::new("a", "b")));
    let src = build(&SourceConfig::MediaBrowser(config)).unwrap();
    let entries = src
        .list(Some(&format!("mb:{}#folder=f1", server.url(""))))
        .unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Trip"]);
}
