//! Provider object identities resolve through the source's current endpoint.

mod common;

use common::{TestServer, resp, serve_bytes};
use fp_core::{Entry, EntryKind};
use fp_sources::{
    DlnaConfig, DlnaSource, FeedConfig, FeedSource, MediaBrowserConfig, MediaBrowserSource, Source,
    location,
};
use std::sync::{Arc, OnceLock};

fn mb_config(url: &str) -> MediaBrowserConfig {
    MediaBrowserConfig {
        id: "jellyfin/alice".into(),
        name: "Jellyfin".into(),
        url: url.into(),
        credentials: None,
        insecure_tls: false,
        parallel_downloads: false,
        max_height: None,
        access_token: Some("current-token".into()),
        user_id: Some("alice".into()),
    }
}

#[test]
fn mediabrowser_item_version_and_assets_survive_endpoint_changes() {
    let old = MediaBrowserSource::new(&mb_config("http://old.invalid/jellyfin")).unwrap();
    let legacy = "mb:http://old.invalid/jellyfin#item=movie&version=second";
    let stable = old.canonical_location(legacy).unwrap();
    assert_eq!(
        location::decode(&stable).unwrap().key,
        "item=movie&version=second"
    );
    assert_eq!(old.canonical_location("http://old.invalid/jellyfin/Videos/movie/stream?static=true&MediaSourceId=second&api_key=expired"), Some(stable.clone()));
    let poster = old
        .canonical_location(
            "http://old.invalid/jellyfin/Items/movie/Images/Primary?tag=art&api_key=expired",
        )
        .unwrap();
    assert!(!location::decode(&poster).unwrap().key.contains("expired"));

    let server = TestServer::start(|req| {
        assert!(
            req.header("authorization")
                .unwrap_or("")
                .contains("Token=\"current-token\"")
        );
        match req.path() {
            "/moved/Users/alice/Items/movie" => resp(
                200,
                r#"{
                "Id":"movie","Name":"Movie","Type":"Movie",
                "MediaSources":[
                    {"Id":"first","Name":"First","MediaStreams":[{"Index":0,"Type":"Video","Height":1080}]},
                    {"Id":"second","Name":"Second","MediaStreams":[
                        {"Index":0,"Type":"Video","Height":2160},
                        {"Index":2,"Type":"Subtitle","Codec":"srt","Language":"en","IsExternal":true}
                    ]}
                ]
            }"#,
                &[("Content-Type", "application/json")],
            ),
            "/moved/Videos/movie/stream" => {
                assert!(req.url.contains("MediaSourceId=second"));
                serve_bytes(req, b"chosen version", true)
            }
            "/moved/Items/movie/Images/Primary" => serve_bytes(req, b"poster", true),
            "/moved/Videos/movie/second/Subtitles/2/Stream.srt" => {
                serve_bytes(req, b"subtitle", true)
            }
            _ => resp(404, "missing", &[]),
        }
    });
    let moved = MediaBrowserSource::new(&mb_config(&server.url("/moved"))).unwrap();
    let entry = Entry::new("Movie", stable.clone(), EntryKind::Video);
    let full = moved.details(&entry).unwrap();
    assert_eq!(full.location, stable);
    assert_eq!(full.subtitles.len(), 1);
    assert_eq!(
        location::owner(&full.subtitles[0].location).as_deref(),
        Some("jellyfin/alice")
    );
    assert_eq!(moved.open(&stable).unwrap().size(), Some(14));
    assert_eq!(moved.open(&poster).unwrap().size(), Some(6));
    assert_eq!(
        moved.open(&full.subtitles[0].location).unwrap().size(),
        Some(8)
    );
    assert_eq!(moved.canonical_location(legacy), Some(stable.clone()));
    let before = server.requests().len();
    let foreign = location::encode("jellyfin/bob", "mb", "item=movie");
    assert!(moved.open(&foreign).is_err());
    assert!(
        moved
            .details(&Entry::new("Other", foreign, EntryKind::Video))
            .is_err()
    );
    assert_eq!(server.requests().len(), before);
}

fn feed_config(id: &str, url: &str) -> FeedConfig {
    FeedConfig {
        id: id.into(),
        name: "Scenes".into(),
        url: url.into(),
        credentials: None,
        insecure_tls: false,
        parallel_downloads: false,
        max_height: None,
    }
}

#[test]
fn heresphere_feed_changes_do_not_rebase_scene_or_asset_urls() {
    let server = TestServer::start(|req| match (req.method.as_str(), req.path()) {
        ("POST", "/new-feed/") => resp(
            200,
            r#"{"library":[{"name":"All","list":["/original/scene/7"]}]}"#,
            &[("Content-Type", "application/json")],
        ),
        ("POST", "/original/scene/7") => resp(
            200,
            r#"{
            "title":"Scene seven", "media":[{"name":"h265","sources":[{"url":"/original/video.mkv"}]}],
            "thumbnailImage":"/original/poster.jpg",
            "scripts":[{"url":"/original/seven.funscript"}],
            "subtitles":[{"url":"/original/seven.en.srt","language":"en"}]
        }"#,
            &[("Content-Type", "application/json")],
        ),
        ("GET", "/original/video.mkv") => serve_bytes(req, b"video", true),
        ("GET", "/original/seven.funscript") => serve_bytes(req, b"script", true),
        ("GET", "/original/seven.en.srt") => serve_bytes(req, b"subtitle", true),
        ("GET", "/original/poster.jpg") => serve_bytes(req, b"poster", true),
        _ => resp(404, "missing", &[]),
    });
    let old = FeedSource::heresphere(&feed_config("hs", &server.url("/original/"))).unwrap();
    let scene_url = server.url("/original/scene/7");
    assert!(old.canonical_location(&scene_url).is_none());
    let moved = FeedSource::heresphere(&feed_config("hs", &server.url("/new-feed/"))).unwrap();
    let group = moved.list(None).unwrap().remove(0);
    let entry = moved.list(Some(&group.location)).unwrap().remove(0);
    assert_eq!(entry.location, scene_url);
    assert!(moved.canonical_location(&scene_url).is_none());
    let full = moved.details(&entry).unwrap();
    assert_eq!(full.name, "Scene seven");
    assert_eq!(full.location, scene_url);
    assert_eq!(full.scripts[0], server.url("/original/seven.funscript"));
    assert_eq!(
        full.subtitles[0].location,
        server.url("/original/seven.en.srt")
    );
    assert_eq!(moved.open(&scene_url).unwrap().size(), Some(5));
    assert_eq!(moved.open(&full.scripts[0]).unwrap().size(), Some(6));
    assert_eq!(
        moved.open(&full.subtitles[0].location).unwrap().size(),
        Some(8)
    );
    assert_eq!(
        full.thumbnail_url.as_deref(),
        Some(server.url("/original/poster.jpg").as_str())
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.path().starts_with("/original/") || r.path() == "/new-feed/")
    );
    let before = server.requests().len();
    assert!(
        moved
            .open(&location::encode("other", "heresphere-id", "7"))
            .is_err()
    );
    assert_eq!(server.requests().len(), before);
}

#[test]
fn deovr_scenes_without_published_ids_keep_their_original_urls() {
    let media = TestServer::start(|req| serve_bytes(req, b"video", true));
    let external = media.url("/independent.mp4?token=unchanged");
    let published = external.clone();
    let server = TestServer::start(move |_| {
        resp(
            200,
            format!(
                r#"{{"scenes":[{{"name":"All","list":[
            {{"title":"Same host","video_url":"/original/7.mp4"}},
            {{"title":"External host","video_url":"{published}"}}
        ]}}]}}"#
            ),
            &[("Content-Type", "application/json")],
        )
    });
    let source = FeedSource::deovr(&feed_config("deo", &server.url("/new-feed/"))).unwrap();
    let group = source.list(None).unwrap().remove(0);
    let scenes = source.list(Some(&group.location)).unwrap();
    assert_eq!(scenes[0].location, server.url("/original/7.mp4"));
    assert_eq!(scenes[1].location, external);
    for scene in &scenes {
        assert!(source.canonical_location(&scene.location).is_none());
    }
    assert_eq!(source.open(&scenes[1].location).unwrap().size(), Some(5));
}

#[test]
fn deovr_published_scene_id_refreshes_signed_external_media_after_restart() {
    let old_media = TestServer::start(|req| serve_bytes(req, b"old video", true));
    let new_media = TestServer::start(|req| serve_bytes(req, b"new video", true));
    let old_address = old_media.url("/different-name.mp4?token=expired");
    let old_feed = TestServer::start(move |_| {
        resp(
            200,
            format!(
                r#"{{"scenes":[{{"name":"All","list":[
        {{"id":"scene-7","title":"Seven","video_url":"{old_address}"}}
    ]}}]}}"#
            ),
            &[("Content-Type", "application/json")],
        )
    });
    let old = FeedSource::deovr(&feed_config("deo", &old_feed.url("/feed"))).unwrap();
    let group = old.list(None).unwrap().remove(0);
    let entry = old.list(Some(&group.location)).unwrap().remove(0);
    let reference = location::decode(&entry.location).unwrap();
    assert_eq!(
        (reference.kind.as_str(), reference.key.as_str()),
        ("deovr-id", "scene-7")
    );
    assert!(!entry.location.contains("expired"));
    let new_address = new_media.url("/renamed.mp4?token=fresh");
    let current_feed = TestServer::start(move |_| {
        resp(
            200,
            format!(
                r#"{{"scenes":[{{"name":"Renamed group","list":[
        {{"id":"scene-7","title":"Seven renamed","video_url":"{new_address}"}}
    ]}}]}}"#
            ),
            &[("Content-Type", "application/json")],
        )
    });
    let current = FeedSource::deovr(&feed_config("deo", &current_feed.url("/new-feed"))).unwrap();
    assert_eq!(current.details(&entry).unwrap().name, "Seven renamed");
    assert_eq!(current.open(&entry.location).unwrap().size(), Some(9));
    assert_eq!(old_media.requests().len(), 0);
    assert!(
        new_media
            .requests()
            .iter()
            .all(|req| req.url.contains("token=fresh"))
    );
    assert_eq!(
        current_feed.requests().len(),
        1,
        "resolved afresh once, then reused while that address is fresh"
    );
    let before = current_feed.requests().len();
    assert!(
        current
            .open(&location::encode(
                "different-account",
                "deovr-id",
                "scene-7"
            ))
            .is_err()
    );
    assert_eq!(current_feed.requests().len(), before);
}

#[test]
fn looking_up_every_scene_of_a_feed_reads_it_once() {
    let media = TestServer::start(|req| serve_bytes(req, b"a video", true));
    let scenes: Vec<String> = (0..40)
        .map(|i| {
            format!(
                r#"{{"id":"{i}","title":"Scene {i}","video_url":"{}"}}"#,
                media.url(&format!("/{i}.mp4?token=fresh"))
            )
        })
        .collect();
    let body = format!(
        r#"{{"scenes":[{{"name":"All","list":[{}]}}]}}"#,
        scenes.join(",")
    );
    let server = TestServer::start(move |_| {
        resp(200, body.clone(), &[("Content-Type", "application/json")])
    });
    let feed = Arc::new(FeedSource::deovr(&feed_config("deo", &server.url("/feed"))).unwrap());
    let locations: Vec<String> = (0..40)
        .map(|i| location::encode("deo", "deovr-id", &i.to_string()))
        .collect();
    // As a group's details fill in: several workers, none of them first.
    std::thread::scope(|scope| {
        for part in locations.chunks(7) {
            let feed = feed.clone();
            scope.spawn(move || {
                for scene in part {
                    assert!(feed.as_scene_feed().unwrap().scene(scene, false).is_ok());
                }
            });
        }
    });
    assert_eq!(server.requests().len(), 1);

    // As one scene opens.
    let video = Entry::new("Scene 3", &locations[3], EntryKind::Video);
    let details = feed.details(&video).unwrap();
    feed.sidecars(&details).unwrap();
    feed.transcode_targets(&details);
    assert_eq!(feed.open(&locations[3]).unwrap().size(), Some(7));
    assert_eq!(server.requests().len(), 1);
}

fn dlna_config(base: &str) -> DlnaConfig {
    DlnaConfig {
        id: "nas".into(),
        name: "NAS".into(),
        location: format!("{base}/description"),
        control_url: Some(format!("{base}/control")),
        service_type: None,
    }
}

fn dlna_identity_server(resource_path: &'static str) -> TestServer {
    let base: Arc<OnceLock<String>> = Arc::default();
    let current_base = base.clone();
    let server = TestServer::start(move |req| match (req.method.as_str(), req.path()) {
        ("POST", "/control") => {
            assert!(
                req.body
                    .contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>")
                    || req.body.contains("<BrowseFlag>BrowseMetadata</BrowseFlag>")
            );
            let base = current_base.get().unwrap();
            let video = format!(
                r#"<item id="item:7" parentID="0">
                <dc:title>Seven.mp4</dc:title><upnp:class>object.item.videoItem</upnp:class>
                <upnp:albumArtURI>{base}/poster.jpg</upnp:albumArtURI>
                <res protocolInfo="http-get:*:video/mp4:*">{base}/{resource_path}/video-opaque</res>
                <res protocolInfo="http-get:*:text/srt:*">{base}/{resource_path}/Seven.en.srt</res>
            </item>"#
            );
            // DLNA resource URLs need not include the script's filename.
            let script = format!(
                r#"<item id="script:roll:7" parentID="0">
                <dc:title>Seven.roll.funscript</dc:title><upnp:class>object.item</upnp:class>
                <res protocolInfo="http-get:*:application/json:*">{base}/{resource_path}/script-opaque</res>
            </item>"#
            );
            let metadata = req.body.contains("<BrowseFlag>BrowseMetadata</BrowseFlag>");
            let items = if !metadata {
                format!("{video}{script}")
            } else if req.body.contains("<ObjectID>script:roll:7</ObjectID>") {
                script
            } else {
                assert!(req.body.contains("<ObjectID>item:7</ObjectID>"));
                video
            };
            let count = if metadata { 1 } else { 2 };
            let didl = format!(r#"<DIDL-Lite xmlns:dc="dc" xmlns:upnp="upnp">{items}</DIDL-Lite>"#);
            let escaped = didl
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            resp(
                200,
                format!(
                    r#"<s:Envelope xmlns:s="s"><s:Body><u:BrowseResponse xmlns:u="u">
                <Result>{escaped}</Result><NumberReturned>{count}</NumberReturned><TotalMatches>{count}</TotalMatches>
            </u:BrowseResponse></s:Body></s:Envelope>"#
                ),
                &[("Content-Type", "text/xml")],
            )
        }
        ("GET", path) if path == format!("/{resource_path}/video-opaque") => {
            serve_bytes(req, b"video", true)
        }
        ("GET", path) if path == format!("/{resource_path}/Seven.en.srt") => {
            serve_bytes(req, b"subtitle", true)
        }
        ("GET", path) if path == format!("/{resource_path}/script-opaque") => {
            serve_bytes(req, br#"{"actions":[{"at":0,"pos":20}]}"#, true)
        }
        ("GET", "/poster.jpg") => serve_bytes(req, b"poster", true),
        _ => resp(404, "missing", &[]),
    });
    base.set(server.base.clone()).unwrap();
    server
}

#[test]
fn dlna_object_and_asset_references_refresh_resources_and_learn_legacy_aliases() {
    let original = dlna_identity_server("original");
    let current = DlnaSource::new(&dlna_config(&original.base)).unwrap();
    let raw = original.url("/original/video-opaque");
    assert!(current.canonical_location(&raw).is_none());
    let entries = current.list(None).unwrap();
    let listed = entries.iter().find(|e| e.kind == EntryKind::Video).unwrap();
    let script = entries.iter().find(|e| e.kind == EntryKind::Other).unwrap();
    assert_eq!(
        location::decode(&listed.location).unwrap().key,
        "item:item:7"
    );
    assert_eq!(
        current.canonical_location(&raw),
        Some(listed.location.clone())
    );
    assert_eq!(
        current.parent(&listed.location),
        Some(location::encode("nas", "dlna", "folder:0"))
    );
    let script_ref = location::decode(&script.location).unwrap();
    assert_eq!(script_ref.kind, "dlna-file");
    assert_eq!(
        serde_json::from_str::<(String, String)>(&script_ref.key).unwrap(),
        ("script:roll:7".into(), "Seven.roll.funscript".into())
    );
    assert_eq!(
        fp_sources::urlutil::last_segment(&script.location),
        "Seven.roll.funscript"
    );
    assert_eq!(listed.subtitles[0].name, "Seven.en.srt");
    assert_eq!(listed.subtitles[0].language.as_deref(), Some("en"));
    assert_eq!(
        fp_sources::urlutil::last_segment(&listed.subtitles[0].location),
        "Seven.en.srt"
    );
    let sidecars = current.sidecars(listed).unwrap();
    assert_eq!(sidecars.scripts[0].axis.as_deref(), Some("roll"));
    assert_eq!(sidecars.scripts[0].location, script.location);

    // A fresh source with the same ID must resolve every saved locator
    // through the changed server address and its newly advertised URLs.
    let server = dlna_identity_server("moved");
    assert_ne!(original.base, server.base);
    let old_requests = original.requests().len();
    let reopened = DlnaSource::new(&dlna_config(&server.base)).unwrap();
    assert_eq!(reopened.open(&listed.location).unwrap().size(), Some(5));
    assert_eq!(
        reopened
            .open(listed.thumbnail_url.as_deref().unwrap())
            .unwrap()
            .size(),
        Some(6)
    );
    assert_eq!(
        reopened.open(&listed.subtitles[0].location).unwrap().size(),
        Some(8)
    );
    let opened_script = reopened.open(&script.location).unwrap();
    let mut bytes = vec![0; opened_script.size().unwrap() as usize];
    let n = opened_script.read_at(0, &mut bytes).unwrap();
    assert_eq!(&bytes[..n], br#"{"actions":[{"at":0,"pos":20}]}"#);
    assert_eq!(reopened.details(script).unwrap().location, script.location);
    assert_eq!(reopened.details(listed).unwrap().location, listed.location);
    let refreshed = reopened
        .sidecars(&reopened.details(listed).unwrap())
        .unwrap();
    assert_eq!(refreshed.scripts[0].axis.as_deref(), Some("roll"));
    assert_eq!(refreshed.scripts[0].location, script.location);
    assert_eq!(refreshed.subtitles[0].name, "Seven.en.srt");
    assert_eq!(original.requests().len(), old_requests);
    assert!(server.count(|req| req.body.contains("BrowseMetadata")) >= 6);
    assert!(server.count(|req| req.path() == "/moved/script-opaque") >= 1);
    assert!(server.count(|req| req.path() == "/moved/Seven.en.srt") >= 1);
    let before = server.requests().len();
    assert!(
        reopened
            .open(&location::encode("other-nas", "dlna", "item:item:7"))
            .is_err()
    );
    for (owner, kind, key) in [
        ("other-nas", "dlna-file", script_ref.key.as_str()),
        ("nas", "dlna-file", "not-json"),
        ("nas", "dlna-file", r#"["","Seven.roll.funscript"]"#),
        ("nas", "dlna-asset", r#"["item:7","foreign",0,"x.srt"]"#),
    ] {
        assert!(reopened.open(&location::encode(owner, kind, key)).is_err());
    }
    assert!(
        reopened
            .details(&Entry::new(
                "Other",
                location::encode("other-nas", "dlna", "item:item:7"),
                EntryKind::Video,
            ))
            .is_err()
    );
    assert_eq!(server.requests().len(), before);
}
