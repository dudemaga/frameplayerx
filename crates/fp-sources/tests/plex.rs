//! Plex protocol integration over local HTTP: real ranges, catalog and sessions.

mod common;

use common::{Req, Resp, TestServer, resp, serve_bytes};
use fp_sources::plex::{PlexSource, owner};
use fp_sources::{
    Catalog, CatalogKind, CatalogQuery, CatalogSort, PlexConfig, Source, SourceConfig, SourceKind,
    WatchEvent, WatchKind,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

fn movie(id: &str) -> Value {
    json!({"ratingKey":id,"type":"movie","title":format!("Movie {id}"),"duration":1000000,
        "viewOffset":12500,"viewCount":0,"year":2024,"summary":"A movie","thumb":"/library/metadata/100/thumb/1",
        "Chapter":[{"startTimeOffset":4000,"tag":"Chapter one"}],
        "Media":[{"id":10,"height":1080,"width":1920,"bitrate":10000,"videoCodec":"h264","Part":[{
            "id":101,"key":"/library/parts/101/file.mp4","file":"/movies/Example_180_LR.mp4","size":4096,
            "Stream":[{"id":201,"index":1,"streamType":2,"codec":"aac","selected":true},{"id":301,"streamType":3,"codec":"srt","languageCode":"en","key":"/library/streams/301.srt"}]}]},
            {"id":11,"height":2160,"width":3840,"bitrate":20000,"videoCodec":"hevc","Part":[{"id":111,"key":"/library/parts/111/file.mp4","file":"/movies/Example_180_LR.mp4","size":4096}]}]})
}

fn reply(request: &Req) -> Resp {
    if request.path() != "/identity" && request.header("X-Plex-Token") != Some("server-secret") {
        return resp(401, b"unauthorized".to_vec(), &[]);
    }
    let url = url::Url::parse(&format!("http://test{}", request.url)).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let value = match request.path() {
        "/identity" => json!({"MediaContainer":{"machineIdentifier":"machine-1"}}),
        "/" => json!({"MediaContainer":{"friendlyName":"My Plex"}}),
        "/library/sections" => {
            json!({"MediaContainer":{"Directory":[{"key":"1","type":"movie","title":"Movies"},{"key":"3","type":"artist","title":"Music"}]}})
        }
        "/library/sections/1/all" => {
            let start = params
                .get("X-Plex-Container-Start")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            json!({"MediaContainer":{"offset":start,"totalSize":2,"Metadata":if start < 2 {vec![movie(if start==0 {"100"} else {"101"})]} else {vec![]}}})
        }
        "/hubs/continueWatching/items" | "/library/metadata/100" => {
            json!({"MediaContainer":{"Metadata":[movie("100")]}})
        }
        // As a real server answers it: the videos are inside a hub.
        "/hubs/continueWatching" => {
            json!({"MediaContainer":{"size":1,"Hub":[{"key":"/hubs/continueWatching/items","type":"mixed","size":1,"Metadata":[movie("100")]}]}})
        }
        "/hubs/search" => {
            json!({"MediaContainer":{"Hub":[{"type":"movie","Metadata":[movie("100")]},{"type":"artist","Metadata":[]}]}})
        }
        "/library/metadata/101,100" => {
            json!({"MediaContainer":{"Metadata":[movie("100"),movie("101")]}})
        }
        "/library/metadata/101" => json!({"MediaContainer":{"Metadata":[movie("101")]}}),
        "/photo/:/transcode" => return serve_bytes(request, b"picture", true),
        "/library/streams/301.srt" => {
            return serve_bytes(request, b"1\n00:00:00,000 --> 00:00:05,000\nHello\n", true);
        }
        "/library/parts/101/file.mp4" | "/library/parts/111/file.mp4" => {
            return serve_bytes(request, &common::pattern(4096), true);
        }
        "/video/:/transcode/universal/decision" => {
            // As a real server does: the profile's name is case-sensitive,
            // and without a profile it answers 400.
            if params.get("X-Plex-Client-Profile-Name").map(String::as_str) != Some("Generic") {
                return resp(400, b"Bad Request".to_vec(), &[]);
            }
            json!({"MediaContainer":{"transcodeDecisionCode":1000}})
        }
        "/video/:/transcode/universal/start.mkv" => {
            return resp(
                200,
                b"streamed-media".to_vec(),
                &[("Content-Type", "video/x-matroska")],
            );
        }
        "/video/:/transcode/universal/stop" | "/:/timeline" | "/:/scrobble" | "/:/unscrobble" => {
            return resp(200, Vec::new(), &[]);
        }
        _ => return resp(404, Vec::new(), &[]),
    };
    resp(
        200,
        serde_json::to_vec(&value).unwrap(),
        &[("Content-Type", "application/json")],
    )
}

fn config(server: &TestServer) -> PlexConfig {
    PlexConfig {
        id: "plex-one".into(),
        name: "My Plex".into(),
        url: server.base.clone(),
        machine_identifier: "machine-1".into(),
        access_token: "server-secret".into(),
        connections: Vec::new(),
        auth: None,
        insecure_tls: false,
        max_height: None,
        local_only: false,
        allow_relay: true,
    }
}

#[test]
fn configuration_is_private_and_construction_does_not_connect() {
    let server = TestServer::start(reply);
    let config = config(&server);
    let source = PlexSource::new(&config).unwrap();
    assert!(server.requests().is_empty());
    assert_eq!(source.kind(), SourceKind::Plex);
    let value = SourceConfig::Plex(config.clone());
    assert_eq!(
        serde_json::from_str::<SourceConfig>(&serde_json::to_string(&value).unwrap()).unwrap(),
        value
    );
    assert!(!format!("{config:?} {source:?}").contains("server-secret"));
    assert_eq!(source.probe().unwrap().machine_identifier, "machine-1");
}

#[test]
fn traditional_link_discovers_the_server_token_and_reads_authenticated_media() {
    use fp_sources::plex::auth::PlexAuthClient;

    let server = TestServer::start(reply);
    let address = server.base.clone();
    let account = TestServer::start(move |request| {
        let body = match (request.method.as_str(), request.path()) {
            ("POST", "/api/v2/pins") => {
                let registration: Value = serde_json::from_str(&request.body).unwrap();
                if registration.get("jwk").is_some() || registration["strong"] != false {
                    return resp(400, "a compatible short-code link is required", &[]);
                }
                json!({"id":42,"code":"AB12","expiresIn":900})
            }
            ("GET", "/api/v2/pins/42") if !request.url.contains('?') => {
                json!({"authToken":"account-secret"})
            }
            ("GET", "/api/v2/resources")
                if request.header("X-Plex-Token") == Some("account-secret") =>
            {
                json!([{
                    "name":"My Plex", "provides":"server", "owned":true,
                    "clientIdentifier":"machine-1", "accessToken":"server-secret",
                    "connections":[{"uri":address,"local":true,"relay":false}]
                }])
            }
            _ => return resp(401, "unauthorized", &[]),
        };
        resp(
            200,
            serde_json::to_vec(&body).unwrap(),
            &[("Content-Type", "application/json")],
        )
    });
    let api = PlexAuthClient::with_test_endpoint(&account.url("/api/v2")).unwrap();
    let pending = api.initiate("installation").unwrap();
    let credentials = api.poll(&pending).unwrap().unwrap();
    assert!(credentials.device_key.is_none());
    assert!(credentials.expires_at.is_none());
    let resource = api.resources(&credentials).unwrap().remove(0);
    let client_id = credentials.client_identifier.clone();
    let mut cfg = config(&server);
    cfg.machine_identifier = resource.machine_identifier;
    cfg.access_token = resource.access_token.unwrap();
    cfg.connections = resource.connections;
    cfg.auth = Some(credentials);
    let source = PlexSource::new(&cfg).unwrap();
    assert_eq!(source.probe().unwrap().url, server.base);
    let video = source.item("100").unwrap();
    let media = source.open(&video.location).unwrap();
    let mut bytes = [0; 16];
    assert_eq!(media.read_at(64, &mut bytes).unwrap(), bytes.len());
    assert_eq!(bytes, common::pattern(4096)[64..80]);

    let requests = server.requests();
    assert!(requests.iter().any(|r| r.header("range").is_some()));
    for request in &requests {
        assert!(!request.url.contains("secret"));
        if request.path() == "/identity" {
            assert!(request.header("X-Plex-Token").is_none());
        } else {
            assert_eq!(request.header("X-Plex-Token"), Some("server-secret"));
            assert_eq!(
                request.header("X-Plex-Client-Identifier"),
                Some(client_id.as_str())
            );
        }
    }
    assert_eq!(account.requests().len(), 3, "no JWT refresh is attempted");
}

#[test]
fn a_rejected_traditional_link_requires_relinking_without_a_jwt_refresh() {
    use fp_sources::plex::auth::PlexCredentials;

    for status in [401, 498] {
        let server = TestServer::start(move |request| {
            if request.path() == "/identity" {
                reply(request)
            } else {
                resp(status, "token rejected", &[])
            }
        });
        let mut cfg = config(&server);
        cfg.auth = Some(PlexCredentials::manual(
            "account-secret".into(),
            "linked-client".into(),
        ));
        assert!(matches!(
            PlexSource::new(&cfg).unwrap().probe(),
            Err(fp_sources::Error::SignedOut(_))
        ));
        assert_eq!(server.requests().len(), 2, "no repeated server requests");
    }
}

#[test]
fn a_lost_local_connection_reconnects_to_relay_with_policy_limits() {
    let offline = Arc::new(AtomicBool::new(false));
    let switched = offline.clone();
    let local = TestServer::start(move |r| {
        if switched.load(Ordering::Acquire) {
            resp(503, Vec::new(), &[])
        } else {
            reply(r)
        }
    });
    let relay = TestServer::start(reply);
    let mut cfg = config(&local);
    cfg.connections = vec![
        fp_sources::plex::auth::PlexConnection {
            uri: local.base.clone(),
            address: None,
            port: None,
            local: true,
            relay: false,
            protocol: "http".into(),
        },
        fp_sources::plex::auth::PlexConnection {
            uri: relay.base.clone(),
            address: None,
            port: None,
            local: false,
            relay: true,
            protocol: "http".into(),
        },
    ];
    let source = PlexSource::new(&cfg).unwrap();
    let entry = source.item("100").unwrap().to_entry();
    assert!(relay.requests().is_empty());
    offline.store(true, Ordering::Release);
    source.item("100").unwrap();
    assert_eq!(source.probe().unwrap().url, relay.base);
    let video = source.details(&entry).unwrap();
    let session = source.watch_session(&video).unwrap();
    assert!(
        matches!(
            source.open_playback(&video.location, &session),
            Err(fp_sources::Error::Unsupported(_))
        ),
        "Automatic must transcode above Relay's bandwidth cap"
    );
    assert!(
        source
            .transcode_targets(&entry)
            .iter()
            .all(|t| t.height.is_some_and(|h| h <= 720))
    );
    let before = relay.requests().len();
    cfg.local_only = true;
    assert!(PlexSource::new(&cfg).unwrap().probe().is_err());
    assert_eq!(
        relay.requests().len(),
        before,
        "local-only must never probe Relay"
    );
    cfg.local_only = false;
    cfg.allow_relay = false;
    assert!(PlexSource::new(&cfg).unwrap().probe().is_err());
    assert_eq!(
        relay.requests().len(),
        before,
        "disabled Relay must never receive credentials"
    );
}

#[test]
fn connection_modes_limit_candidates_and_try_direct_remote_before_relay() {
    for (local_only, allow_relay, remote_available, expected) in [
        (true, false, true, vec!["local"]),
        (false, false, true, vec!["local", "remote"]),
        (false, false, false, vec!["local", "remote"]),
        (false, true, true, vec!["local", "remote"]),
        (false, true, false, vec!["local", "remote", "relay"]),
    ] {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let record = attempts.clone();
        let local = TestServer::start(move |request| {
            record.lock().unwrap().push("local");
            assert_eq!(request.path(), "/identity");
            assert!(request.header("X-Plex-Token").is_none());
            resp(404, "local endpoint unavailable", &[])
        });
        let record = attempts.clone();
        let remote = TestServer::start(move |request| {
            if request.path() == "/identity" {
                record.lock().unwrap().push("remote");
            }
            if remote_available {
                reply(request)
            } else {
                assert_eq!(request.path(), "/identity");
                assert!(request.header("X-Plex-Token").is_none());
                resp(404, "remote endpoint unavailable", &[])
            }
        });
        let record = attempts.clone();
        let relay = TestServer::start(move |request| {
            if request.path() == "/identity" {
                record.lock().unwrap().push("relay");
            }
            reply(request)
        });
        let mut cfg = config(&relay);
        cfg.local_only = local_only;
        cfg.allow_relay = allow_relay;
        // Even a saved Relay URL and reversed advertisements must not
        // override the selected policy or the direct-connection priority.
        cfg.connections = [
            (&relay, false, true),
            (&remote, false, false),
            (&local, true, false),
        ]
        .into_iter()
        .map(
            |(server, local, relay)| fp_sources::plex::auth::PlexConnection {
                uri: server.base.clone(),
                address: None,
                port: None,
                local,
                relay,
                protocol: "http".into(),
            },
        )
        .collect();
        let result = PlexSource::new(&cfg).unwrap().probe();
        if local_only || (!remote_available && !allow_relay) {
            assert!(result.is_err());
        } else {
            assert_eq!(
                result.unwrap().url,
                if remote_available {
                    &remote.base
                } else {
                    &relay.base
                }
                .as_str()
            );
        }
        assert_eq!(*attempts.lock().unwrap(), expected);
        if local_only {
            assert!(
                remote.requests().is_empty(),
                "Local only must not contact a direct remote address"
            );
        }
        if local_only || !allow_relay || remote_available {
            assert!(
                relay.requests().is_empty(),
                "Relay is excluded or unnecessary after a direct connection succeeds"
            );
        }
    }
}

#[test]
fn hevc_matroska_uses_local_codec_decisions_and_authenticated_original_ranges() {
    let server = TestServer::start(|request| {
        let value = match request.path() {
            "/library/metadata/100" => {
                let mut video = movie("100");
                video["Media"][1]["container"] = "mkv".into();
                video["Media"][1]["Part"][0]["key"] = "/library/parts/111/file.mkv".into();
                video["Media"][1]["Part"][0]["file"] = "/movies/Example_180_LR.mkv".into();
                video["Media"][1]["Part"][0]["Stream"] = json!([
                    {"id":113,"streamType":3,"codec":"srt","selected":true}
                ]);
                json!({"MediaContainer":{"Metadata":[video]}})
            }
            "/video/:/transcode/universal/decision" => {
                let url = url::Url::parse(&format!("http://test{}", request.url)).unwrap();
                let params: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                if params.get("hasMDE").map(String::as_str) == Some("1")
                    && params.get("directPlay").map(String::as_str) == Some("1")
                    && params.get("mediaIndex").map(String::as_str) == Some("1")
                    && params.get("partIndex").map(String::as_str) == Some("0")
                {
                    if params.get("subtitles").map(String::as_str) != Some("none") {
                        return resp(
                            400,
                            "hasMDE with a selected subtitle requires an explicit subtitle setting",
                            &[],
                        );
                    }
                    json!({"MediaContainer":{"mdeDecisionCode":1000,"mdeDecisionText":"Direct play OK."}})
                } else {
                    json!({"MediaContainer":{
                        "directPlayDecisionCode":3000,
                        "directPlayDecisionText":"App cannot direct play this item. No direct play video profile exists for protocol http, with container mkv, and video codec hevc."
                    }})
                }
            }
            "/library/parts/111/file.mkv" => {
                return serve_bytes(request, &common::pattern(4096), true);
            }
            _ => return reply(request),
        };
        resp(
            200,
            serde_json::to_vec(&value).unwrap(),
            &[("Content-Type", "application/json")],
        )
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    let video = source
        .details(&source.item("100").unwrap().to_entry())
        .unwrap();
    assert!(
        !video.location.contains("media="),
        "the version chosen automatically stays out of the location"
    );
    let session = source.watch_session(&video).unwrap();
    let original = source.open_playback(&video.location, &session).unwrap();
    let mut bytes = [0; 16];
    assert_eq!(original.read_at(64, &mut bytes).unwrap(), bytes.len());
    assert_eq!(bytes, common::pattern(4096)[64..80]);
    let requests = server.requests();
    let decision = requests
        .iter()
        .find(|r| r.path().ends_with("/decision"))
        .unwrap();
    let url = url::Url::parse(&format!("http://test{}", decision.url)).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["protocol"], "http");
    assert_eq!(params["X-Plex-Session-Identifier"], session.id);
    assert_eq!(params["session"], session.id);
    assert!(
        requests
            .iter()
            .any(|r| r.path().ends_with("file.mkv") && r.header("range").is_some())
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.path().contains("/universal/start"))
    );
    assert!(
        requests
            .iter()
            .filter(|r| r.path() != "/identity")
            .all(|r| r.header("X-Plex-Token") == Some("server-secret"))
    );
}

#[test]
fn local_codec_decisions_still_respect_server_playback_denials() {
    for (code, reason) in [
        (
            2011,
            "Remote playback requires a Remote Watch Pass or Plex Pass.",
        ),
        (3001, "Not enough bandwidth for direct play of this item."),
        (1001, "Direct play not available; Conversion OK."),
    ] {
        let server = TestServer::start(move |request| {
            if request.path() == "/video/:/transcode/universal/decision" {
                return resp(
                    200,
                    serde_json::to_vec(&json!({"MediaContainer":{
                        "directPlayDecisionCode":1000,
                        "directPlayDecisionText":"Direct play is possible.",
                        "generalDecisionCode":if code == 2011 { 2011 } else { 1000 },
                        "generalDecisionText":reason,
                        "mdeDecisionCode":code,
                        "mdeDecisionText":reason
                    }}))
                    .unwrap(),
                    &[("Content-Type", "application/json")],
                );
            }
            reply(request)
        });
        let source = PlexSource::new(&config(&server)).unwrap();
        let video = source
            .details(&source.item("100").unwrap().to_entry())
            .unwrap();
        let session = source.watch_session(&video).unwrap();
        let error = source
            .open_playback(&video.location, &session)
            .err()
            .unwrap();
        assert!(error.to_string().contains(reason));
        assert!(
            !server
                .requests()
                .iter()
                .any(|r| r.path().contains("/parts/"))
        );
    }
}

#[test]
fn server_decisions_and_multipart_are_not_opened_as_original_files() {
    let server =
        TestServer::start(|r| {
            if r.path() == "/video/:/transcode/universal/decision" {
                return resp(200, serde_json::to_vec(&json!({"MediaContainer": {
                "directPlayDecisionCode": 1001, "directPlayDecisionText": "Conversion required",
                "generalDecisionCode": 2011, "generalDecisionText": "Transcoding unavailable"
            }})).unwrap(), &[("Content-Type", "application/json")]);
            }
            reply(r)
        });
    let source = PlexSource::new(&config(&server)).unwrap();
    let video = source
        .details(&source.item("100").unwrap().to_entry())
        .unwrap();
    let session = source.watch_session(&video).unwrap();
    assert!(matches!(
        source.open_playback(&video.location, &session),
        Err(fp_sources::Error::Unsupported(_))
    ));
    let target = source.transcode_targets(&video).remove(0);
    assert!(
        source
            .open_transcode_session(&video.location, &target.id, 0.0, &session)
            .is_err()
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|r| r.path().contains("/parts/") || r.path().ends_with("start.mkv"))
    );

    let multipart = TestServer::start(|r| {
        if r.path() == "/library/metadata/100" {
            let mut movie = movie("100");
            let part = movie["Media"][0]["Part"][0].clone();
            movie["Media"][0]["Part"].as_array_mut().unwrap().push(part);
            movie["Media"].as_array_mut().unwrap().truncate(1);
            return resp(
                200,
                serde_json::to_vec(&json!({"MediaContainer":{"Metadata":[movie]}})).unwrap(),
                &[("Content-Type", "application/json")],
            );
        }
        reply(r)
    });
    let source = PlexSource::new(&config(&multipart)).unwrap();
    let video = source
        .details(&source.item("100").unwrap().to_entry())
        .unwrap();
    assert!(matches!(
        source.open(&video.location),
        Err(fp_sources::Error::Unsupported(_))
    ));
}

#[test]
fn catalog_versions_media_assets_and_watch_keep_source_ownership() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let home = source.home().unwrap();
    assert_eq!(home.libraries.len(), 1, "music libraries are excluded");
    let page = source
        .items(&CatalogQuery {
            parent: Some(home.libraries[0].id.clone()),
            types: vec![CatalogKind::Movie],
            ..Default::default()
        })
        .unwrap();
    assert_eq!((page.total, page.items.len()), (2, 2));
    let item = source.item("100").unwrap();
    assert_eq!(item.versions.len(), 2);
    assert_eq!(owner(&item.location).as_deref(), Some("plex-one"));
    assert!(!item.location.contains("server-secret"));
    source.set_version(&item.location, Some("10"));
    let video = source.details(&item.to_entry()).unwrap();
    assert!(video.location.contains("media=10"));
    assert_eq!(video.resume, Some(12.5));
    assert_eq!(video.markers, vec![(4.0, "Chapter one".into())]);
    assert_eq!(video.subtitles.len(), 1);
    let session = source.watch_session(&video).unwrap();
    let tracks = source.playback_tracks(&video).unwrap();
    assert!(
        tracks
            .iter()
            .any(|t| t.id == "201" && !t.subtitle && t.index == Some(1))
    );
    assert!(tracks.iter().any(|t| t.id == "301"
        && t.subtitle
        && t.location.as_ref() == Some(&video.subtitles[0].location)));
    let file = source.open_playback(&video.location, &session).unwrap();
    let mut buf = [0; 16];
    assert_eq!(file.read_at(64, &mut buf).unwrap(), 16);
    assert_eq!(buf, common::pattern(4096)[64..80]);
    assert!(source.open(video.thumbnail_url.as_deref().unwrap()).is_ok());
    assert!(source.open(&video.subtitles[0].location).is_ok());
    for kind in [WatchKind::Start, WatchKind::Pause, WatchKind::Stop] {
        source
            .report_watch(
                &session,
                WatchEvent {
                    kind,
                    position: 14.25,
                    paused: kind == WatchKind::Pause,
                    speed: 1.0,
                },
            )
            .unwrap();
    }
    source.set_played("100", true).unwrap();
    let mut other = config(&server);
    other.id = "other-user".into();
    let other = PlexSource::new(&other).unwrap();
    let before = server.requests().len();
    assert!(other.open(&video.location).is_err());
    assert_eq!(
        server.requests().len(),
        before,
        "foreign locations must not send credentials"
    );
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .filter(|r| r.path() != "/identity")
            .all(|r| r.header("X-Plex-Token") == Some("server-secret"))
    );
    let timeline = requests.iter().find(|r| r.path() == "/:/timeline").unwrap();
    assert_eq!(timeline.method, "POST");
    assert!(timeline.url.contains("time=14250") && timeline.url.contains("duration=1000000"));
    assert!(timeline.url.contains(&session.id));
    assert!(
        requests
            .iter()
            .any(|r| r.method == "PUT" && r.path() == "/:/scrobble")
    );
    source.set_version(&item.location, None);
    assert!(
        source
            .details(&item.to_entry())
            .unwrap()
            .location
            .ends_with("/item/100"),
        "back to the location it is listed under"
    );
}

#[test]
fn tv_collections_facets_and_reset_resume_use_plex_metadata() {
    let server = TestServer::start(|r| {
        let show = json!({"ratingKey":"200","type":"show","title":"A show","leafCount":2,"viewedLeafCount":1});
        let season = json!({"ratingKey":"300","type":"season","title":"Season 1","parentRatingKey":"200","parentTitle":"A show","index":1});
        let episode = json!({"ratingKey":"400","type":"episode","title":"Episode 2","parentRatingKey":"300","grandparentRatingKey":"200","grandparentTitle":"A show","index":2,"parentIndex":1,"duration":42000,"thumb":"/episode-thumb"});
        let metadata = match r.path() {
            "/library/metadata/200" => Some(vec![show]),
            "/library/metadata/200/children" => Some(vec![
                json!({"ratingKey":"301","type":"season","title":"Another season","index":2}),
                season,
            ]),
            "/library/metadata/300/children" => {
                let mut first = episode.clone();
                first["ratingKey"] = "401".into();
                first["index"] = 1.into();
                Some(vec![episode, first])
            }
            "/library/metadata/301/children" => Some(Vec::new()),
            "/hubs/continueWatching/items" => Some(vec![episode]),
            "/library/metadata/500" => Some(vec![
                json!({"ratingKey":"500","type":"collection","title":"Collection"}),
            ]),
            "/library/metadata/500/children" => Some(vec![movie("100")]),
            "/library/metadata/100" => {
                let mut v = movie("100");
                v.as_object_mut().unwrap().remove("viewOffset");
                Some(vec![v])
            }
            _ => None,
        };
        let value = if let Some(metadata) = metadata {
            json!({"MediaContainer":{"totalSize":metadata.len(),"Metadata":metadata}})
        } else if r.path().starts_with("/library/sections/1/") && !r.path().ends_with("/all") {
            let (title, key) = match r.path().rsplit('/').next().unwrap() {
                "genre" => ("Drama", "42"),
                "studio" => ("Studio", "Studio"),
                "year" => ("2024", "2024"),
                _ => return resp(404, Vec::new(), &[]),
            };
            json!({"MediaContainer":{"Directory":[{"title":title,"key":key}]}})
        } else {
            return reply(r);
        };
        resp(
            200,
            serde_json::to_vec(&value).unwrap(),
            &[("Content-Type", "application/json")],
        )
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    assert_eq!(source.item("200").unwrap().unplayed, Some(1));
    assert_eq!(
        source.seasons("200").unwrap()[0].series_id.as_deref(),
        Some("200")
    );
    let mut episodes = source.episodes("200", None).unwrap();
    assert_eq!(
        episodes.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["401", "400"],
        "episode indexes determine watch order rather than title sorting"
    );
    let episode = episodes.remove(1);
    assert_eq!(
        (
            episode.season_id.as_deref(),
            episode.parent_index,
            episode.index
        ),
        (Some("300"), Some(1), Some(2))
    );
    assert_eq!(source.next_up(Some("200"), 1).unwrap()[0].id, "400");
    assert!(source.next_up(Some("other-show"), 1).unwrap().is_empty());
    assert_eq!(source.item("500").unwrap().kind, CatalogKind::BoxSet);
    assert_eq!(
        source
            .items(&CatalogQuery {
                parent: Some("500".into()),
                ..Default::default()
            })
            .unwrap()
            .items[0]
            .id,
        "100"
    );
    let mut query = CatalogQuery {
        parent: Some("section:1".into()),
        ..Default::default()
    };
    let facets = source.facets(&query).unwrap();
    assert_eq!(facets.genres, ["Drama"]);
    assert_eq!(facets.years, [2024]);
    query.genres = facets.genres;
    source.items(&query).unwrap();
    assert!(server.requests().iter().any(|r| r.url.contains("genre=42")));
    let video = source
        .details(&source.item("100").unwrap().to_entry())
        .unwrap();
    assert_eq!(
        video.resume,
        Some(0.0),
        "a reset server resume must override local history"
    );
    assert!(!source.capabilities().favorites);
}

#[test]
fn studio_filters_use_exact_names_once_encoded_and_keep_server_pagination() {
    let studio_names = [
        "North & South Pictures",
        "North & South Pictures Classics",
        "Cinéma 100%",
        "Foo, Inc.",
    ];
    let server = TestServer::start(move |r| {
        let value = match r.path() {
            "/library/sections/1/studio" => {
                json!({"MediaContainer":{"Directory":studio_names.iter().map(|name| {
                // PMS studio choice keys are escaped strings, unlike genre IDs.
                json!({"title":name,"key":fp_sources::urlutil::encode_form_value(name)})
            }).collect::<Vec<_>>()}})
            }
            "/library/sections/1/genre" => {
                json!({"MediaContainer":{"Directory":[{"title":"Drama","key":"42"}]}})
            }
            "/library/sections/1/year" => json!({"MediaContainer":{"Directory":[]}}),
            "/library/sections/1/all" => {
                let url = url::Url::parse(&format!("http://test{}", r.url)).unwrap();
                let params: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                let mut items: Vec<_> = studio_names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, name)| {
                        let studio_matches = if let Some(exact) = params.get("studio=") {
                            // Like PMS: split at commas, then decode each.
                            exact.split(',').any(|selected| {
                                percent_encoding::percent_decode_str(selected).decode_utf8_lossy()
                                    == *name
                            })
                        } else if let Some(contains) = params.get("studio") {
                            contains.split(',').any(|selected| name.contains(selected))
                        } else {
                            true
                        };
                        if !studio_matches || params.get("genre").is_some_and(|genre| genre != "42")
                        {
                            return None;
                        }
                        let mut item = movie(&(100 + i).to_string());
                        item["studio"] = (*name).into();
                        Some(item)
                    })
                    .collect();
                let total = items.len();
                let start = params["X-Plex-Container-Start"].parse::<usize>().unwrap();
                let limit = params["X-Plex-Container-Size"].parse::<usize>().unwrap();
                items = items.into_iter().skip(start).take(limit).collect();
                json!({"MediaContainer":{"offset":start,"totalSize":total,"Metadata":items}})
            }
            _ => return reply(r),
        };
        assert_eq!(r.header("X-Plex-Token"), Some("server-secret"));
        resp(
            200,
            serde_json::to_vec(&value).unwrap(),
            &[("Content-Type", "application/json")],
        )
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    let mut query = CatalogQuery {
        parent: Some("section:1".into()),
        ..Default::default()
    };
    assert_eq!(source.items(&query).unwrap().total, 4);
    let facets = source.facets(&query).unwrap();
    assert_eq!(facets.studios, studio_names);
    query.genres = facets.genres;
    query.studios = vec![studio_names[0].into()];
    let page = source.items(&query).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].studios, [studio_names[0]]);

    query.studios.push(studio_names[2].into());
    query.limit = 1;
    let first = source.items(&query).unwrap();
    assert_eq!((first.total, first.items[0].id.as_str()), (2, "100"));
    query.start = 1;
    let second = source.items(&query).unwrap();
    assert_eq!((second.total, second.items[0].id.as_str()), (2, "102"));
    assert_eq!(second.items[0].studios, [studio_names[2]]);

    query.start = 0;
    query.studios = vec![studio_names[3].into()];
    let page = source.items(&query).unwrap();
    assert_eq!(
        (page.total, page.items[0].id.as_str()),
        (1, "103"),
        "a comma inside a name doesn't split it"
    );

    query.studios.clear();
    assert_eq!(source.items(&query).unwrap().total, 4);

    // Rebuilding a source must not require warming its facet cache again.
    let rebuilt = PlexSource::new(&config(&server)).unwrap();
    query.genres.clear();
    query.studios = vec![studio_names[2].into()];
    let page = rebuilt.items(&query).unwrap();
    assert_eq!((page.total, page.items[0].id.as_str()), (1, "102"));
}

#[test]
fn shuffle_snapshots_a_stable_listing_and_search_has_video_groups() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let mut ids = source
        .shuffled(&CatalogQuery {
            parent: Some("section:1".into()),
            sort: CatalogSort::Random,
            ..Default::default()
        })
        .unwrap();
    ids.sort();
    assert_eq!(ids, ["100", "101"]);
    assert!(
        server
            .requests()
            .iter()
            .filter(|r| r.path() == "/library/sections/1/all")
            .all(|r| r.url.contains("titleSort"))
    );
    let results = source.search("Movie", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "Movies", "headed as on Jellyfin");
    assert_eq!(results[0].1[0].id, "100");
    assert!(!source.capabilities().favorites);
    let page = source
        .items(&CatalogQuery {
            ids: vec!["101".into(), "100".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ["101", "100"]
    );
    assert_eq!(server.count(|r| r.path() == "/library/metadata/101,100"), 1);
}

#[test]
fn server_identity_is_verified_before_sending_the_token() {
    let server = TestServer::start(reply);
    let mut cfg = config(&server);
    cfg.machine_identifier = "another-machine".into();
    let source = PlexSource::new(&cfg).unwrap();
    assert!(source.probe().is_err());
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.path() == "/identity" && r.header("X-Plex-Token").is_none())
    );
}

#[test]
fn an_invalid_identity_does_not_prevent_trying_another_connection() {
    for missing_machine in [true, false] {
        let invalid = TestServer::start(move |_| {
            let identity = if missing_machine {
                json!({"MediaContainer":{}})
            } else {
                json!({"MediaContainer":{"machineIdentifier":"another-server"}})
            };
            resp(
                200,
                serde_json::to_vec(&identity).unwrap(),
                &[("Content-Type", "application/json")],
            )
        });
        let valid = TestServer::start(reply);
        let mut cfg = config(&invalid);
        cfg.connections = [&invalid, &valid]
            .into_iter()
            .map(|server| fp_sources::plex::auth::PlexConnection {
                uri: server.base.clone(),
                address: None,
                port: None,
                local: true,
                relay: false,
                protocol: "http".into(),
            })
            .collect();
        let source = PlexSource::new(&cfg).unwrap();
        assert_eq!(source.probe().unwrap().url, valid.base);
        assert!(!invalid.requests().is_empty());
        assert!(
            invalid
                .requests()
                .iter()
                .all(|r| r.path() == "/identity" && r.header("X-Plex-Token").is_none())
        );
        assert!(
            valid
                .requests()
                .iter()
                .any(|r| r.header("X-Plex-Token") == Some("server-secret"))
        );
    }
}

#[test]
fn progressive_encoding_uses_seconds_kilobits_and_stops_on_drop() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let video = source
        .details(&source.item("100").unwrap().to_entry())
        .unwrap();
    let session = source.watch_session(&video).unwrap();
    let targets = source.transcode_targets(&video);
    assert!(!targets.is_empty());
    let stream = source
        .open_transcode_session(&video.location, &targets.last().unwrap().id, 12.5, &session)
        .unwrap();
    let mut buf = [0; 8];
    assert_eq!(stream.read_at(0, &mut buf).unwrap(), 8);
    assert_eq!(&buf, b"streamed");
    let start = server
        .requests()
        .into_iter()
        .find(|r| r.path().ends_with("start.mkv"))
        .unwrap();
    let url = url::Url::parse(&format!("http://test{}", start.url)).unwrap();
    let p: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(p["protocol"], "http");
    assert_eq!(p["offset"], "12.5");
    assert_eq!(p["X-Plex-Session-Identifier"], session.id);
    assert!(p["videoBitrate"].parse::<u64>().unwrap() < 100000);
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(2);
    while server.count(|r| r.path().ends_with("/stop")) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(server.count(|r| r.path().ends_with("/stop")) > 0);
}

fn json_reply(value: &Value) -> Resp {
    resp(
        200,
        serde_json::to_vec(value).unwrap(),
        &[("Content-Type", "application/json")],
    )
}

#[test]
fn continue_watching_comes_from_the_listing_inside_its_hub() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let home = source.home().unwrap();
    assert_eq!(home.resume.len(), 1, "a real server wraps the plain path");
    assert_eq!(home.resume[0].id, "100");
}

#[test]
fn a_blip_on_the_only_connection_ends_when_the_server_is_back() {
    let offline = Arc::new(AtomicBool::new(false));
    let down = offline.clone();
    let server = TestServer::start(move |r| {
        if down.load(Ordering::Acquire) {
            resp(503, Vec::new(), &[])
        } else {
            reply(r)
        }
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    source.item("100").unwrap();
    offline.store(true, Ordering::Release);
    let during = source.items(&CatalogQuery {
        ids: vec!["100".into()],
        ..Default::default()
    });
    assert!(
        !matches!(during, Ok(_) | Err(fp_sources::Error::NotFound(_))),
        "a server that is down hasn't deleted anything: {during:?}"
    );
    offline.store(false, Ordering::Release);
    assert_eq!(
        source.probe().unwrap().machine_identifier,
        "machine-1",
        "back at once, not after a minute"
    );
}

#[test]
fn a_show_is_watched_when_its_episodes_are() {
    // viewCount as a real server sends it: it stays up when episodes are
    // marked unwatched, so 1 of 3 can read 3.
    let item = |id: &str, kind: &str, plays: u32, seen: u32| json!({"ratingKey":id,"type":kind,"title":id,"viewCount":plays,"leafCount":3,"viewedLeafCount":seen});
    let server = TestServer::start(move |r| {
        let metadata = match r.path() {
            "/library/metadata/1" => item("1", "show", 0, 0),
            "/library/metadata/2" => item("2", "show", 3, 1),
            "/library/metadata/3" => item("3", "show", 3, 3),
            "/library/metadata/4" => item("4", "season", 2, 1),
            "/library/metadata/5" => item("5", "season", 3, 3),
            "/library/metadata/6" => {
                json!({"ratingKey":"6","type":"show","title":"Empty","viewCount":0,"leafCount":0,"viewedLeafCount":0})
            }
            "/library/metadata/7" => {
                json!({"ratingKey":"7","type":"episode","title":"Seen","viewCount":1})
            }
            _ => return reply(r),
        };
        json_reply(&json!({"MediaContainer":{"Metadata":[metadata]}}))
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    for (id, played, unplayed) in [
        ("1", false, Some(3)),
        ("2", false, Some(2)),
        ("3", true, Some(0)),
        ("4", false, Some(2)),
        ("5", true, Some(0)),
        ("6", false, Some(0)),
        ("7", true, None),
    ] {
        let item = source.item(id).unwrap();
        assert_eq!((item.played, item.unplayed), (played, unplayed), "{id}");
    }
}

#[test]
fn a_video_keeps_the_location_it_is_listed_under_until_a_version_is_picked() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let listed = source.item("100").unwrap().to_entry();
    assert_eq!(source.details(&listed).unwrap().location, listed.location);

    source.set_version(&listed.location, Some("10"));
    let picked = source.details(&listed).unwrap();
    assert!(
        picked.location.ends_with("?media=10"),
        "{}",
        picked.location
    );
}

#[test]
fn a_saved_version_whose_file_was_replaced_still_plays() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let listed = source.item("100").unwrap().to_entry();
    let mut saved = listed.clone();
    saved.location = format!("{}?media=99", listed.location);

    let video = source.details(&saved).unwrap();
    assert_eq!(
        video.location, saved.location,
        "the same library row, with its history"
    );
    let file = source.open(&video.location).unwrap();
    let mut bytes = [0; 16];
    assert_eq!(file.read_at(64, &mut bytes).unwrap(), bytes.len());
}

#[test]
fn a_grid_reloading_more_than_a_page_gets_all_of_it() {
    const TOTAL: usize = 450;
    let server = TestServer::start(|r| {
        if r.path() != "/library/sections/1/all" {
            return reply(r);
        }
        let url = url::Url::parse(&format!("http://test{}", r.url)).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let start: usize = params["X-Plex-Container-Start"].parse().unwrap();
        let size: usize = params["X-Plex-Container-Size"].parse().unwrap();
        assert!(size <= 200, "Plex is asked for a page at a time");
        let items: Vec<_> = (start..TOTAL.min(start + size))
            .map(|i| movie(&i.to_string()))
            .collect();
        json_reply(&json!({"MediaContainer":{"offset":start,"totalSize":TOTAL,"Metadata":items}}))
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    let page = source
        .items(&CatalogQuery {
            parent: Some("section:1".into()),
            limit: 450,
            ..Default::default()
        })
        .unwrap();
    assert_eq!((page.items.len(), page.total), (450, 450));
    assert_eq!(page.items[449].id, "449");
    assert_eq!(server.count(|r| r.path() == "/library/sections/1/all"), 3);

    let page = source
        .items(&CatalogQuery {
            parent: Some("section:1".into()),
            limit: 60,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.items.len(), 60, "a normal page is still one request");
    assert_eq!(server.count(|r| r.path() == "/library/sections/1/all"), 4);
}

#[test]
fn listings_move_on_by_what_the_server_sent() {
    // Every other item is a kind the catalog doesn't keep.
    let server = TestServer::start(|r| {
        if r.path() != "/library/metadata/9/children" {
            return reply(r);
        }
        let url = url::Url::parse(&format!("http://test{}", r.url)).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let start: usize = params["X-Plex-Container-Start"].parse().unwrap();
        let size: usize = params["X-Plex-Container-Size"].parse().unwrap();
        let items: Vec<_> = (start..500.min(start + size))
            .map(|i| {
                let kind = if i % 2 == 0 { "episode" } else { "photo" };
                json!({"ratingKey":i.to_string(),"type":kind,"title":"x","index":i})
            })
            .collect();
        json_reply(&json!({"MediaContainer":{"offset":start,"totalSize":500,"Metadata":items}}))
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    let episodes = source.episodes("series", Some("9")).unwrap();
    assert_eq!(episodes.len(), 250, "none repeated, none missed");
    assert_eq!(server.count(|r| r.path().ends_with("/children")), 3);
}

#[test]
fn the_home_page_survives_a_library_that_fails() {
    let server = TestServer::start(|r| {
        let value = match r.path() {
            "/library/sections" => json!({"MediaContainer":{"Directory":[
                {"key":"1","type":"movie","title":"Movies"},
                {"key":"2","type":"show","title":"Broken"},
                {"key":"3","type":"movie","title":"Empty"}]}}),
            "/library/sections/2/all" => return resp(500, Vec::new(), &[]),
            "/library/sections/3/all" => {
                json!({"MediaContainer":{"offset":0,"totalSize":0,"Metadata":[]}})
            }
            _ => return reply(r),
        };
        json_reply(&value)
    });
    let source = PlexSource::new(&config(&server)).unwrap();
    let home = source.home().unwrap();
    assert_eq!(home.libraries.len(), 3);
    let rows: Vec<_> = home.latest.iter().map(|(l, _)| l.name.as_str()).collect();
    assert_eq!(rows, ["Movies"], "as on Jellyfin: no failed or empty rows");
}

#[test]
fn opening_a_video_reads_its_details_once() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let listed = source.item("100").unwrap().to_entry();
    let video = source.details(&listed).unwrap();
    source.sidecars(&video).unwrap();
    let session = source.watch_session(&video).unwrap();
    source.playback_tracks(&video).unwrap();
    assert!(!source.transcode_targets(&video).is_empty());
    source.open_playback(&video.location, &session).unwrap();
    let reads = |s: &TestServer| s.count(|r| r.path() == "/library/metadata/100");
    assert_eq!(reads(&server), 1);

    // Where it was left has changed: the next read asks the server.
    source
        .report_watch(
            &session,
            WatchEvent {
                kind: WatchKind::Stop,
                position: 14.0,
                paused: false,
                speed: 1.0,
            },
        )
        .unwrap();
    source.details(&listed).unwrap();
    assert_eq!(reads(&server), 2);

    source.set_played("100", true).unwrap();
    source.details(&listed).unwrap();
    assert_eq!(reads(&server), 3, "marking it changes it too");
}

#[test]
fn pictures_are_asked_for_at_the_size_they_show() {
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let item = source.item("100").unwrap();
    let picture = source.open(item.poster.as_deref().unwrap()).unwrap();
    let mut bytes = [0; 7];
    assert_eq!(picture.read_at(0, &mut bytes).unwrap(), 7);
    let requests = server.requests();
    let asked = requests
        .iter()
        .find(|r| r.path() == "/photo/:/transcode")
        .unwrap();
    let url = url::Url::parse(&format!("http://test{}", asked.url)).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["url"], "/library/metadata/100/thumb/1");
    assert_eq!(
        (params["width"].as_str(), params["height"].as_str()),
        ("480", "720")
    );
    assert!(!item.poster.unwrap().contains("secret"));
}

#[test]
fn a_plex_id_is_no_format_hint() {
    use fp_core::format::{ContainerHints, Projection, resolve};
    let server = TestServer::start(reply);
    let source = PlexSource::new(&config(&server)).unwrap();
    let listed = source.item("100").unwrap().location;
    for id in ["180", "360"] {
        let location = listed.replace("/item/100", &format!("/item/{id}"));
        let name = fp_sources::urlutil::last_segment(&location);
        let got = resolve(None, ContainerHints::default(), &name, 1920, 1080);
        assert_eq!(got.format.projection, Projection::Flat, "ID {id}");
    }
}
