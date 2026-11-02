//! Plex code linking, saved device-token refresh and account server discovery.
//!
//! The account endpoints are fixed in production. These calls block and
//! belong on job threads; a pending link is kept only until it completes.

use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions};
use crate::timeutil::parse_iso8601;
use crate::urlutil::redact;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::Ed25519KeyPair;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ACCOUNT_API: &str = "https://clients.plex.tv/api/v2";
const LINK_API: &str = "https://plex.tv/api/v2";
/// Where a user enters the four-character code on another device.
pub const LINK_URL: &str = "https://plex.tv/link";
const JSON_LIMIT: u64 = 8 << 20;
const TOKEN_LIFETIME: u64 = 7 * 24 * 60 * 60;

/// A registered signing key. PKCS#8 contains the private key and must only
/// be saved with the source's other private credentials.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexDeviceKey {
    pub id: String,
    pkcs8: String,
}

impl fmt::Debug for PlexDeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexDeviceKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PlexDeviceKey {
    fn pair(&self) -> Result<Ed25519KeyPair> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.pkcs8)
            .map_err(|_| Error::Config("invalid saved Plex signing key".into()))?;
        Ed25519KeyPair::from_pkcs8(&bytes)
            .map_err(|_| Error::Config("invalid saved Plex signing key".into()))
    }

    fn assertion(&self, client: &str, nonce: Option<&str>) -> Result<String> {
        let header = json!({"typ":"JWT", "alg":"EdDSA", "kid":self.id});
        let at = now();
        let mut claims = json!({
            "aud":"plex.tv", "iss":client, "iat":at, "exp":at + 300,
            "scope":"username,friendly_name",
        });
        if let Some(nonce) = nonce {
            claims["nonce"] = nonce.into();
        }
        let data = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?),
        );
        let signature = self.pair()?.sign(data.as_bytes());
        Ok(format!(
            "{data}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

/// Account credential used for discovery and refresh. A resource may
/// supply a different token for the chosen owned/shared server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexCredentials {
    pub access_token: String,
    pub client_identifier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_key: Option<PlexDeviceKey>,
    /// Unix seconds; only a refresh scheduling hint, not authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl fmt::Debug for PlexCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexCredentials")
            .field("client_identifier", &self.client_identifier)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl PlexCredentials {
    pub fn manual(access_token: String, client_identifier: String) -> Self {
        Self {
            access_token,
            client_identifier,
            device_key: None,
            expires_at: None,
        }
    }

    /// A refresh can begin before expiry, avoiding one in a media read.
    pub fn needs_refresh(&self) -> bool {
        self.device_key.is_some()
            && self
                .expires_at
                .is_some_and(|expiry| expiry <= now() + 60 * 60)
    }
}

/// A pending device authorization. Its code is a secret, so Debug
/// deliberately omits it.
#[derive(Clone)]
pub struct PlexPin {
    pub id: u64,
    pub code: String,
    pub expires_at: u64,
    client_identifier: String,
}

impl PlexPin {
    pub fn expired(&self) -> bool {
        now() >= self.expires_at
    }
}

impl fmt::Debug for PlexPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexPin")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// One address advertised for a Plex server; retain HTTPS hostnames for
/// certificate checks, including plex.direct and relay addresses.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlexConnection {
    pub uri: String,
    /// An advertised literal IP, without replacing the URL's TLS hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<std::net::IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub local: bool,
    #[serde(default)]
    pub relay: bool,
    #[serde(default)]
    pub protocol: String,
}

impl fmt::Debug for PlexConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexConnection")
            .field("uri", &redact(&self.uri))
            .field("local", &self.local)
            .field("relay", &self.relay)
            .finish()
    }
}

/// A server visible to the linked account, including shared servers.
#[derive(Clone, PartialEq, Eq)]
pub struct PlexResource {
    pub name: String,
    pub machine_identifier: String,
    pub access_token: Option<String>,
    pub connections: Vec<PlexConnection>,
    pub owned: bool,
    pub provides: String,
}

impl fmt::Debug for PlexResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlexResource")
            .field("name", &self.name)
            .field("machine_identifier", &self.machine_identifier)
            .field("connections", &self.connections)
            .field("owned", &self.owned)
            .finish_non_exhaustive()
    }
}

/// The Plex account API, independent of the media-server connection.
#[derive(Clone, Debug)]
pub struct PlexAuthClient {
    base: String,
    link_base: String,
}

impl Default for PlexAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PlexAuthClient {
    pub fn new() -> Self {
        Self {
            base: ACCOUNT_API.into(),
            link_base: LINK_API.into(),
        }
    }

    /// A loopback account API for hermetic protocol tests. Production
    /// callers use [`Self::new`], not a configurable account endpoint.
    #[doc(hidden)]
    pub fn with_test_endpoint(base: &str) -> Result<Self> {
        let parsed = url::Url::parse(base)
            .map_err(|_| Error::Config("invalid Plex test endpoint".into()))?;
        if parsed.scheme() != "http"
            || !matches!(
                parsed.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            )
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(Error::Config(
                "Plex test endpoint must be HTTP loopback".into(),
            ));
        }
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            link_base: base.trim_end_matches('/').into(),
        })
    }

    fn client(base: &str, client: &str, token: Option<&str>) -> HttpClient {
        let mut headers = vec![
            ("Accept".into(), "application/json".into()),
            ("X-Plex-Product".into(), "FramePlayerX".into()),
            ("X-Plex-Version".into(), env!("CARGO_PKG_VERSION").into()),
            ("X-Plex-Device".into(), "Steam Frame".into()),
            ("X-Plex-Client-Identifier".into(), client.into()),
        ];
        if let Some(token) = token {
            headers.push(("X-Plex-Token".into(), token.into()));
        }
        HttpClient::new_with_headers(
            HttpOptions {
                max_retries: 0,
                read_timeout: Duration::from_secs(15),
                ..Default::default()
            },
            headers,
            base,
        )
    }

    fn json(
        base: &str,
        method: &str,
        path: &str,
        client: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        let address = format!("{base}{path}");
        let bytes = body.as_ref().map(serde_json::to_vec).transpose()?;
        let reply = Self::client(base, client, token)
            .request(
                method,
                &address,
                &[("Content-Type", "application/json")],
                bytes.as_deref(),
                JSON_LIMIT,
            )?
            .error_for_status(&address)?;
        serde_json::from_slice(&reply.body).map_err(Error::from)
    }

    /// Requests a four-character code to enter at [`LINK_URL`], using a
    /// source-qualified identity derived from this installation. Traditional
    /// tokens also work with servers that reject the newer JWT tokens.
    pub fn initiate(&self, installation: &str) -> Result<PlexPin> {
        if installation.is_empty()
            || !installation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(Error::Config("invalid Plex client identifier".into()));
        }
        let mut random = [0_u8; 16];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| Error::Config("cannot create a Plex client identifier".into()))?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let client_identifier = format!("{installation}-{suffix}");
        let response = Self::json(
            &self.link_base,
            "POST",
            "/pins",
            &client_identifier,
            None,
            Some(json!({"strong": false})),
        )?;
        let id = response
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::parse("Plex link", "missing PIN id"))?;
        let code = string(&response, "code")
            .ok_or_else(|| Error::parse("Plex link", "missing PIN code"))?;
        if code.len() != 4 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(Error::parse("Plex link", "expected a four-character code"));
        }
        let expires_at =
            expiry(&response).ok_or_else(|| Error::parse("Plex link", "missing PIN expiry"))?;
        Ok(PlexPin {
            id,
            code,
            expires_at,
            client_identifier,
        })
    }

    /// Exchanges the claimed code for a traditional account token.
    pub fn poll(&self, pin: &PlexPin) -> Result<Option<PlexCredentials>> {
        if pin.expired() {
            return Err(Error::Auth("Plex link expired".into()));
        }
        let response = Self::json(
            &self.link_base,
            "GET",
            &format!("/pins/{}", pin.id),
            &pin.client_identifier,
            None,
            None,
        )?;
        Ok(
            string(&response, "authToken").map(|access_token| PlexCredentials {
                expires_at: None,
                access_token,
                client_identifier: pin.client_identifier.clone(),
                device_key: None,
            }),
        )
    }

    pub fn refresh(&self, credentials: &PlexCredentials) -> Result<PlexCredentials> {
        let key = credentials
            .device_key
            .as_ref()
            .ok_or_else(|| Error::Auth("Plex requires reconnection".into()))?;
        let response = Self::json(
            &self.base,
            "GET",
            "/auth/nonce",
            &credentials.client_identifier,
            None,
            None,
        )?;
        let nonce = string(&response, "nonce")
            .ok_or_else(|| Error::parse("Plex refresh", "missing nonce"))?;
        let assertion = key.assertion(&credentials.client_identifier, Some(&nonce))?;
        let response = Self::json(
            &self.base,
            "POST",
            "/auth/token",
            &credentials.client_identifier,
            None,
            Some(json!({"jwt":assertion})),
        )?;
        let access_token = string(&response, "auth_token")
            .ok_or_else(|| Error::parse("Plex refresh", "missing token"))?;
        Ok(PlexCredentials {
            expires_at: Some(token_expiry(&access_token).unwrap_or_else(|| now() + TOKEN_LIFETIME)),
            access_token,
            ..credentials.clone()
        })
    }

    /// Lists server resources only. Player devices commonly have null
    /// accessToken and must not make the entire response fail to parse.
    pub fn resources(&self, credentials: &PlexCredentials) -> Result<Vec<PlexResource>> {
        let response = Self::json(
            &self.base,
            "GET",
            "/resources?includeHttps=1&includeRelay=1&includeIPv6=1",
            &credentials.client_identifier,
            Some(&credentials.access_token),
            None,
        )?;
        let rows = response
            .as_array()
            .ok_or_else(|| Error::parse("Plex resources", "expected resource array"))?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let provides = string(row, "provides").unwrap_or_default();
                if !provides.split(',').any(|kind| kind.trim() == "server") {
                    return None;
                }
                let machine_identifier =
                    string(row, "clientIdentifier").or_else(|| string(row, "machineIdentifier"))?;
                let connections = row
                    .get("connections")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|connection| {
                        let uri = string(connection, "uri")?;
                        let parsed = url::Url::parse(&uri).ok()?;
                        if !matches!(parsed.scheme(), "http" | "https")
                            || !parsed.username().is_empty()
                            || parsed.password().is_some()
                        {
                            return None;
                        }
                        Some(PlexConnection {
                            protocol: parsed.scheme().into(),
                            uri,
                            address: string(connection, "address").and_then(|address| {
                                address
                                    .strip_prefix('[')
                                    .and_then(|a| a.strip_suffix(']'))
                                    .unwrap_or(&address)
                                    .parse()
                                    .ok()
                            }),
                            port: connection.get("port").and_then(|port| {
                                port.as_u64()
                                    .or_else(|| port.as_str().and_then(|p| p.parse().ok()))
                                    .and_then(|p| u16::try_from(p).ok())
                                    .filter(|p| *p > 0)
                            }),
                            local: boolean(connection, "local"),
                            relay: boolean(connection, "relay"),
                        })
                    })
                    .collect();
                Some(PlexResource {
                    name: string(row, "name").unwrap_or_else(|| "Plex".into()),
                    machine_identifier,
                    access_token: string(row, "accessToken"),
                    connections,
                    owned: boolean(row, "owned"),
                    provides,
                })
            })
            .collect())
    }
}

fn string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn boolean(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .is_some_and(|v| v == true || v == 1 || v == "1" || v == "true")
}

fn expiry(value: &Value) -> Option<u64> {
    value
        .get("expiresAt")
        .and_then(|v| {
            v.as_u64().or_else(|| {
                v.as_str()
                    .and_then(parse_iso8601)
                    .and_then(|n| n.try_into().ok())
            })
        })
        .or_else(|| {
            value
                .get("expiresIn")
                .and_then(Value::as_u64)
                .map(|seconds| now().saturating_add(seconds))
        })
}

fn token_expiry(token: &str) -> Option<u64> {
    // This claim schedules refresh only. Server authorization still uses
    // the opaque token; decoded claims never establish an identity.
    let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_u64()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
