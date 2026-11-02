//! Renewal across the account and server boundaries, without live credentials.

use super::*;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::Ed25519KeyPair;
use serde_json::json;

#[path = "../../tests/common/mod.rs"]
mod common;
use common::{TestServer, resp};

#[test]
fn rejected_saved_credentials_renew_at_handshake_or_on_a_later_request() {
    for reject_at_handshake in [true, false] {
        let server = TestServer::start(move |r| {
            let token = r.header("X-Plex-Token");
            let body = match r.path() {
                "/identity" => json!({"MediaContainer":{"machineIdentifier":"machine"}}),
                "/" | "/library/sections" if token == Some("server-old") && reject_at_handshake => {
                    return resp(401, Vec::new(), &[]);
                }
                "/" => json!({"MediaContainer":{}}),
                "/library/sections" => json!({"MediaContainer":{"Directory":[]}}),
                "/library/metadata/100" if token == Some("server-new") => {
                    json!({"MediaContainer":{"Metadata":[{"ratingKey":"100","type":"movie","title":"Film"}]}})
                }
                _ => return resp(401, Vec::new(), &[]),
            };
            resp(
                200,
                serde_json::to_vec(&body).unwrap(),
                &[("Content-Type", "application/json")],
            )
        });
        let base = server.base.clone();
        let account = TestServer::start(move |r| {
            let body = match r.path() {
                "/auth/nonce" => json!({"nonce":"nonce"}),
                "/auth/token" => json!({"auth_token":"account-new"}),
                "/resources" => {
                    assert_eq!(r.header("X-Plex-Token"), Some("account-new"));
                    json!([{"name":"Plex","provides":"server","clientIdentifier":"machine","accessToken":"server-new",
                        "connections":[{"uri":base,"local":true,"relay":false}]}])
                }
                _ => return resp(404, Vec::new(), &[]),
            };
            resp(
                200,
                serde_json::to_vec(&body).unwrap(),
                &[("Content-Type", "application/json")],
            )
        });
        let api = PlexAuthClient::with_test_endpoint(&account.base).unwrap();
        let private = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let auth = serde_json::from_value::<auth::PlexCredentials>(json!({
            "access_token":"account-old", "client_identifier":"saved-client",
            "device_key":{"id":"saved-key", "pkcs8":URL_SAFE_NO_PAD.encode(private.as_ref())},
            "expires_at":4_000_000_000_u64,
        }))
        .unwrap();
        let config = PlexConfig {
            id: "source".into(),
            name: "Plex".into(),
            url: server.base.clone(),
            machine_identifier: "machine".into(),
            access_token: "server-old".into(),
            connections: Vec::new(),
            auth: Some(auth),
            insecure_tls: false,
            max_height: None,
            local_only: false,
            allow_relay: true,
        };
        let mut source = PlexSource::new(&config).unwrap();
        source.auth_api = api;
        source.probe().unwrap();
        assert_eq!(source.item("100").unwrap().name, "Film");
        assert_eq!(account.count(|r| r.path() == "/auth/token"), 1);
        let Some(SourceConfig::Plex(updated)) = source.updated_config() else {
            panic!("renewed credentials must be persisted");
        };
        assert_eq!(updated.id, config.id);
        assert_eq!(updated.access_token, "server-new");
        assert_eq!(updated.auth.unwrap().access_token, "account-new");
        assert!(source.updated_config().is_none());
        assert!(
            server
                .requests()
                .iter()
                .all(|r| r.header("X-Plex-Token") != Some("account-new"))
        );
    }
}

#[test]
fn a_listed_remote_address_has_to_be_encrypted() {
    let listed = |uri: &str| PlexConnection {
        uri: uri.into(),
        address: None,
        port: None,
        local: false,
        relay: false,
        protocol: uri.split(':').next().unwrap().into(),
    };
    let config = |url: &str, connections: Vec<PlexConnection>| PlexConfig {
        id: "source".into(),
        name: "Plex".into(),
        url: url.into(),
        machine_identifier: "machine".into(),
        access_token: "secret".into(),
        connections,
        auth: None,
        insecure_tls: false,
        max_height: None,
        local_only: false,
        allow_relay: false,
    };
    let tried =
        |c: &PlexConfig| -> Vec<String> { connections(c).into_iter().map(|c| c.uri).collect() };
    let https = "https://1-2-3-4.abc.plex.direct:32400";
    for (plain, kept) in [
        ("http://203.0.113.5:32400", false),
        ("http://[2001:db8::5]:32400", false),
        ("http://plex.example.com:32400", false),
        ("http://192.168.1.5:32400", true),
        ("http://[fd00::5]:32400", true),
        ("http://nas.local:32400", true),
        ("http://nas:32400", true),
    ] {
        let c = config(https, vec![listed(https), listed(plain)]);
        assert_eq!(tried(&c).contains(&plain.to_string()), kept, "{plain}");
        assert!(tried(&c).contains(&https.to_string()));
    }
    // A saved address is held to the same rule on a linked source.
    let plain = "http://203.0.113.5:32400";
    let c = config(plain, vec![listed(https), listed(plain)]);
    assert_eq!(tried(&c), [https]);
    let c = config(plain, vec![listed(https)]);
    assert_eq!(tried(&c), [https]);
    // An address typed for an access token is the one they chose.
    let c = config(plain, Vec::new());
    assert_eq!(tried(&c), [plain]);
}
