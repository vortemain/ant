//! Single sign-on for team coordinators (decision D-063): the coordinator
//! accepts OpenID Connect ID tokens from one issuer (Google, Okta, Microsoft
//! Entra, Keycloak, ...) in place of the shared client token.
//!
//! At start the coordinator reads the issuer's discovery document and its
//! signing keys (JWKS). A presented bearer token that is not one of the
//! static tokens is checked as a JWT: RS256 signature by a published key
//! (keys are refetched, at most once a minute, when an unknown `kid`
//! appears), `iss` equal to the issuer, `aud` containing the configured
//! client id, not expired (60 s clock skew), a verified email, and (when
//! configured) an email domain on the allow list. A valid token makes its
//! holder a client; the email is recorded in the audit trail (D-059).
//!
//! Developers sign in with the device authorization grant (RFC 8628), which
//! needs no browser on the machine running `ant`: the coordinator publishes
//! what that needs at `GET /v1/sso` (unauthenticated: issuer, client id,
//! endpoints, and the installed-app client secret some issuers require,
//! which their own documentation says is not confidential).

use base64::Engine as _;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    /// The OAuth client id: the `aud` ID tokens must carry.
    pub audience: String,
    /// Allowed email domains (`example.com`); empty allows any verified email.
    pub allowed_domains: Vec<String>,
    /// Sent to clients for the device flow when the issuer requires one.
    pub client_secret: Option<String>,
}

/// RSA public keys by `kid` (modulus, exponent), and when they were fetched.
type KeyCache = (Option<Instant>, HashMap<String, (Vec<u8>, Vec<u8>)>);

pub struct OidcVerifier {
    config: OidcConfig,
    jwks_uri: String,
    device_endpoint: Option<String>,
    token_endpoint: String,
    agent: ureq::Agent,
    keys: Mutex<KeyCache>,
}

impl std::fmt::Debug for OidcVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("issuer", &self.config.issuer)
            .finish_non_exhaustive()
    }
}

fn decode(text: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .map_err(|_| "not base64url".to_owned())
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<serde_json::Value, String> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| format!("cannot fetch {url}: {error}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "{url} answered HTTP {}",
            response.status().as_u16()
        ));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("cannot read {url}: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("{url}: invalid JSON: {error}"))
}

impl OidcVerifier {
    /// Read the issuer's discovery document and keys; fails closed.
    pub fn discover(config: OidcConfig) -> Result<Self, String> {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(15)))
                .build(),
        );
        let issuer = config.issuer.trim_end_matches('/');
        let discovery = get_json(
            &agent,
            &format!("{issuer}/.well-known/openid-configuration"),
        )?;
        if discovery["issuer"]
            .as_str()
            .map(|value| value.trim_end_matches('/'))
            != Some(issuer)
        {
            return Err(format!(
                "the discovery document does not name {issuer} as its issuer"
            ));
        }
        let field = |name: &str| discovery[name].as_str().map(str::to_owned);
        let verifier = Self {
            jwks_uri: field("jwks_uri").ok_or("the issuer publishes no jwks_uri")?,
            device_endpoint: field("device_authorization_endpoint"),
            token_endpoint: field("token_endpoint")
                .ok_or("the issuer publishes no token_endpoint")?,
            config,
            agent,
            keys: Mutex::new((None, HashMap::new())),
        };
        verifier.refresh_keys()?;
        Ok(verifier)
    }

    fn refresh_keys(&self) -> Result<(), String> {
        let jwks = get_json(&self.agent, &self.jwks_uri)?;
        let mut keys = HashMap::new();
        for key in jwks["keys"].as_array().into_iter().flatten() {
            if key["kty"] != "RSA" || key.get("use").is_some_and(|value| value != "sig") {
                continue;
            }
            let (Some(kid), Some(n), Some(e)) =
                (key["kid"].as_str(), key["n"].as_str(), key["e"].as_str())
            else {
                continue;
            };
            keys.insert(kid.to_owned(), (decode(n)?, decode(e)?));
        }
        if keys.is_empty() {
            return Err("the issuer publishes no RSA signing keys".to_owned());
        }
        let mut stored = self.keys.lock().map_err(|_| "key cache poisoned")?;
        *stored = (Some(Instant::now()), keys);
        Ok(())
    }

    /// What `ant login --sso` needs for the device flow.
    pub fn public_info(&self) -> serde_json::Value {
        serde_json::json!({
            "issuer": self.config.issuer,
            "client_id": self.config.audience,
            "client_secret": self.config.client_secret,
            "device_authorization_endpoint": self.device_endpoint,
            "token_endpoint": self.token_endpoint,
        })
    }

    /// The verified email of a valid ID token.
    pub fn verify(&self, jwt: &str, now_s: u64) -> Result<String, String> {
        let parts: Vec<&str> = jwt.split('.').collect();
        let [header, claims, signature] = parts.as_slice() else {
            return Err("not a JWT".to_owned());
        };
        let header: serde_json::Value =
            serde_json::from_slice(&decode(header)?).map_err(|_| "bad JWT header")?;
        if header["alg"] != "RS256" {
            return Err("only RS256 ID tokens are accepted".to_owned());
        }
        let kid = header["kid"].as_str().unwrap_or_default().to_owned();
        let key = {
            let keys = self.keys.lock().map_err(|_| "key cache poisoned")?;
            keys.1.get(&kid).cloned().map(Ok).unwrap_or(Err(keys.0))
        };
        let (n, e) = match key {
            Ok(key) => key,
            Err(fetched) => {
                // Issuers rotate keys: refetch, but not more than once a minute.
                if fetched.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
                    return Err("signed by an unknown key".to_owned());
                }
                self.refresh_keys()?;
                let keys = self.keys.lock().map_err(|_| "key cache poisoned")?;
                keys.1
                    .get(&kid)
                    .cloned()
                    .ok_or("signed by an unknown key")?
            }
        };
        ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
            .verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &decode(signature)?,
            )
            .map_err(|_| "bad signature")?;
        let claims: serde_json::Value =
            serde_json::from_slice(&decode(claims)?).map_err(|_| "bad JWT claims")?;
        if claims["iss"]
            .as_str()
            .map(|value| value.trim_end_matches('/'))
            != Some(self.config.issuer.trim_end_matches('/'))
        {
            return Err("issued by someone else".to_owned());
        }
        let audience = &self.config.audience;
        let for_us = match &claims["aud"] {
            serde_json::Value::String(aud) => aud == audience,
            serde_json::Value::Array(auds) => auds.iter().any(|aud| aud == audience),
            _ => false,
        };
        if !for_us {
            return Err("issued for another application".to_owned());
        }
        let exp = claims["exp"].as_u64().ok_or("no expiry")?;
        if exp + 60 < now_s {
            return Err("expired".to_owned());
        }
        if claims["iat"].as_u64().is_some_and(|iat| iat > now_s + 60) {
            return Err("issued in the future".to_owned());
        }
        let email = claims["email"]
            .as_str()
            .ok_or("no email in the token")?
            .to_owned();
        if claims["email_verified"] == serde_json::Value::Bool(false) {
            return Err("the email is not verified".to_owned());
        }
        if !self.config.allowed_domains.is_empty() {
            let domain = email
                .rsplit_once('@')
                .map(|(_, domain)| domain.to_ascii_lowercase());
            if !domain.is_some_and(|domain| {
                self.config
                    .allowed_domains
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&domain))
            }) {
                return Err("this email domain is not allowed".to_owned());
            }
        }
        Ok(email)
    }
}

/// The `exp` of a JWT, without verifying it (clients use it to refresh
/// before expiry; the coordinator verifies).
pub fn unverified_expiry(jwt: &str) -> Option<u64> {
    let claims = jwt.split('.').nth(1)?;
    let claims: serde_json::Value = serde_json::from_slice(&decode(claims).ok()?).ok()?;
    claims["exp"].as_u64()
}
