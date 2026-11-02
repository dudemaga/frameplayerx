//! Source-owned identities, independent of the server's current address.

use crate::urlutil::{encode_segment, redact};
use url::Url;

/// An opaque key understood only by the configured source and provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub source_id: String,
    pub kind: String,
    pub key: String,
}

/// Encodes components individually, including arbitrary source identifiers.
pub fn encode(source_id: &str, kind: &str, key: &str) -> String {
    let component = |value: &str| {
        if matches!(value, "." | "..") {
            value.replace('.', "%2E")
        } else {
            encode_segment(value)
        }
    };
    format!(
        "source://source/{}/{}/{}",
        component(source_id),
        component(kind),
        component(key)
    )
}

/// Decodes only the exact source-owned URI grammar; no network authority or
/// query parameters can be smuggled outside the opaque key.
pub fn decode(location: &str) -> Option<Location> {
    let url = Url::parse(location).ok()?;
    if url.scheme() != "source"
        || url.host_str() != Some("source")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    // Inspect the original path: URL parsing would otherwise normalize dot
    // segments before we can enforce exactly three opaque components.
    let authority = location.split_once("://")?.1;
    let path = authority.split_once('/')?.1;
    let parts: Vec<_> = path
        .split('/')
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(std::borrow::Cow::into_owned)
        })
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.len() != 3 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    Some(Location {
        source_id: parts[0].clone(),
        kind: parts[1].clone(),
        key: parts[2].clone(),
    })
}

/// The exact configured owner. Callers must not try other sources on error.
pub fn owner(location: &str) -> Option<String> {
    decode(location).map(|l| l.source_id)
}

fn root_url(root: &str) -> Option<Url> {
    let mut root = Url::parse(root).ok()?;
    if !matches!(root.scheme(), "http" | "https")
        || !root.username().is_empty()
        || root.password().is_some()
        || root.fragment().is_some()
    {
        return None;
    }
    if !root.path().ends_with('/') {
        root.set_path(&format!("{}/", root.path()));
    }
    Some(root)
}

fn safe_path(path: &str) -> bool {
    !path.starts_with('/')
        && !path.contains(['\\', '#'])
        && path.split('/').all(|part| {
            let decoded: Vec<_> = percent_encoding::percent_decode_str(part).collect();
            decoded != b"."
                && decoded != b".."
                && !decoded.iter().any(|b| matches!(b, b'/' | b'\\' | 0))
        })
}

/// A same-origin descendant expressed relative to the configured root.
/// Escaped path bytes and non-secret query parameters remain intact.
pub fn relative_url(root: &str, location: &str) -> Option<String> {
    let root = root_url(root)?;
    let url = Url::parse(location).ok()?;
    if url.origin() != root.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || redact(url.as_str()) != url.as_str()
    {
        return None;
    }
    let path = if url.path() == root.path().trim_end_matches('/') {
        ""
    } else {
        url.path().strip_prefix(root.path())?
    };
    if !safe_path(path) {
        return None;
    }
    let mut key = path.to_string();
    if let Some(query) = url.query() {
        key.push('?');
        key.push_str(query);
    }
    Some(key)
}

/// Resolves a validated relative key against the source's current address.
pub fn resolve_url(root: &str, key: &str) -> Option<String> {
    let root = root_url(root)?;
    let path = key.split_once('?').map_or(key, |(path, _)| path);
    if !safe_path(path) || key.contains('#') {
        return None;
    }
    let url = root.join(&format!("./{key}")).ok()?;
    relative_url(root.as_str(), url.as_str())?;
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_components_roundtrip_without_becoming_url_structure() {
        let uri = encode("source/id?#ü", "http", "sub/film%20one.mkv?version=2");
        assert_eq!(
            decode(&uri),
            Some(Location {
                source_id: "source/id?#ü".into(),
                kind: "http".into(),
                key: "sub/film%20one.mkv?version=2".into()
            })
        );
        assert_eq!(decode(&encode("id", "http", "")).unwrap().key, "");
        assert_eq!(decode(&encode("..", "http", ".")).unwrap().key, ".");
        for bad in [
            "source://other/id/http/key",
            "source://u:p@source/id/http/key",
            "source://source:123/id/http/key",
            "source://source/id/http/key?x=1",
            "source://source/id/http/key#x",
            "source://source/id/http/key/extra",
            "source://source//http/key",
            "source://source/id/http/%FF",
            "source://source/foreign/../id/http/key",
        ] {
            assert!(decode(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn relative_urls_rebase_only_within_the_source_root() {
        let old = "http://old:8080/videos/";
        let key = relative_url(old, "http://old:8080/videos/sub/film%20one.mkv?version=2").unwrap();
        assert_eq!(key, "sub/film%20one.mkv?version=2");
        assert_eq!(
            resolve_url("https://new/moved/", &key).unwrap(),
            "https://new/moved/sub/film%20one.mkv?version=2"
        );
        for other in [
            "http://old:8080/videos-other/file",
            "http://old:8081/videos/file",
            "http://other/videos/file",
            "http://u:p@old:8080/videos/file",
            "http://old:8080/videos/file?api_key=secret",
            "http://old:8080/videos/../file",
        ] {
            assert!(relative_url(old, other).is_none(), "{other}");
        }
        for key in [
            "../secret",
            "/secret",
            "//other/file",
            "%2e%2e/secret",
            "%2e%2e%2fsecret",
            "a\\..\\secret",
            "file?token=secret",
        ] {
            assert!(resolve_url(old, key).is_none(), "{key}");
        }
    }
}
