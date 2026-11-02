//! Root-relative network identities survive a configured address/root change.

mod common;

use common::{TestServer, pattern, resp, serve_bytes};
use fp_core::{Entry, EntryKind};
use fp_sources::{Credentials, HttpConfig, HttpDirSource, Source, WebDavSource, location};

fn config(id: &str, url: &str) -> HttpConfig {
    HttpConfig {
        id: id.into(),
        name: "Server".into(),
        url: url.into(),
        credentials: None,
        insecure_tls: false,
    }
}

#[test]
fn http_identity_reopens_at_a_new_address_and_root_and_keeps_sidecars() {
    let before = HttpDirSource::new(&config("folder/id", "http://old.invalid/old/")).unwrap();
    let stable = before
        .canonical_location("http://old.invalid/old/sub/Clip%20one.mp4?version=2")
        .unwrap();
    assert_eq!(
        location::decode(&stable).unwrap().key,
        "sub/Clip%20one.mp4?version=2"
    );
    assert!(!stable.contains("old.invalid"));
    let bytes = pattern(8192);
    let data = bytes.clone();
    let server = TestServer::start(move |r| {
        if r.header("Authorization") != Some("Basic Ym9iOnB3") {
            return resp(401, "missing configured credentials", &[]);
        }
        match r.path() {
            "/new/sub/" => resp(
                200,
                r#"<pre><a href="Clip%20one.mp4">Clip one.mp4</a>
<a href="Clip%20one.en.srt">Clip one.en.srt</a></pre>"#,
                &[],
            ),
            "/new/sub/Clip%20one.mp4" => serve_bytes(r, &data, true),
            _ => resp(404, "missing", &[]),
        }
    });
    let mut moved = config("folder/id", &server.url("/new/"));
    moved.credentials = Some(Credentials::new("bob", "pw"));
    let after = HttpDirSource::new(&moved).unwrap();
    let file = after.open(&stable).unwrap();
    let mut actual = [0; 16];
    assert_eq!(file.read_at(64, &mut actual).unwrap(), actual.len());
    assert_eq!(actual, bytes[64..80]);
    let parent = after.parent(&stable).unwrap();
    assert_eq!(location::decode(&parent).unwrap().key, "sub/");
    assert_eq!(after.list(Some(&parent)).unwrap().len(), 2);
    let video = Entry::new("Clip one.mp4", stable.clone(), EntryKind::Video);
    let mut raw_entry = Entry::new(
        "Clip one.mp4",
        server.url("/new/sub/Clip%20one.mp4?version=2"),
        EntryKind::Video,
    );
    raw_entry
        .scripts
        .push(server.url("/new/sub/Clip%20one.roll.funscript"));
    let full = after.details(&raw_entry).unwrap();
    assert_eq!(full.location, stable);
    assert_eq!(
        location::decode(&full.scripts[0]).unwrap().key,
        "sub/Clip%20one.roll.funscript"
    );
    let sidecars = after.sidecars(&video).unwrap();
    assert_eq!(sidecars.subtitles.len(), 1);
    assert_eq!(
        location::decode(&sidecars.subtitles[0].location)
            .unwrap()
            .key,
        "sub/Clip%20one.en.srt"
    );
    assert_eq!(after.canonical_location(&stable), Some(stable));
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.path().starts_with("/new/"))
    );
    let count = server.requests().len();
    for invalid in [
        location::encode("other", "http", "sub/Clip.mp4"),
        location::encode("folder/id", "webdav", "sub/Clip.mp4"),
        location::encode("folder/id", "http", "../outside.mp4"),
    ] {
        assert!(after.open(&invalid).is_err());
    }
    assert_eq!(server.requests().len(), count);
    assert!(
        after
            .canonical_location(&server.url("/new-other/file.mp4"))
            .is_none()
    );
    assert!(
        after
            .canonical_location("http://foreign.invalid/new/file.mp4")
            .is_none()
    );
}

#[test]
fn webdav_identity_rebases_escaped_names_and_navigation_without_the_old_host() {
    let before = WebDavSource::new(&config("dav", "http://old.invalid/dav/")).unwrap();
    let stable = before
        .canonical_location("http://old.invalid/dav/Clip%20%231.mkv")
        .unwrap();
    let server = TestServer::start(|r| match (r.method.as_str(), r.path()) {
        ("PROPFIND", "/moved/") => resp(
            207,
            r#"<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/moved/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/moved/Clip%20%231.mkv</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>8192</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
</D:multistatus>"#,
            &[],
        ),
        ("GET", "/moved/Clip%20%231.mkv") => serve_bytes(r, &pattern(8192), true),
        _ => resp(404, "missing", &[]),
    });
    let after = WebDavSource::new(&config("dav", &server.url("/moved/"))).unwrap();
    let list = after.list(None).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].location, stable);
    assert!(after.parent(&after.parent(&stable).unwrap()).is_none());
    let file = after.open(&stable).unwrap();
    let mut actual = [0; 16];
    assert_eq!(file.read_at(64, &mut actual).unwrap(), actual.len());
    assert_eq!(actual, pattern(8192)[64..80]);
    assert!(
        server
            .requests()
            .iter()
            .all(|r| r.path().starts_with("/moved/"))
    );
}
