//! Authenticated HTTP client shared by `ant worker` and `ant remote run`.

use crate::auth::Token;
use crate::blobs::digest_bytes;
use crate::protocol::{MissingRequest, MissingResponse};
use build_digest::ContentDigest;
use std::time::Duration;

pub struct Coordinator {
    base: String,
    token: Token,
    agent: ureq::Agent,
    max_body_bytes: u64,
}

/// Refuse to send a bearer token over plain HTTP to anything but loopback,
/// unless the caller explicitly accepts that risk (for example on a private
/// WireGuard network).
pub fn check_transport(url: &str, allow_insecure: bool) -> Result<(), String> {
    if let Some(rest) = url.strip_prefix("http://") {
        let host_port = rest.split('/').next().unwrap_or("");
        let host = if let Some(stripped) = host_port.strip_prefix('[') {
            stripped.split(']').next().unwrap_or("")
        } else {
            host_port
                .rsplit_once(':')
                .map_or(host_port, |(host, _)| host)
        };
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback && !allow_insecure {
            return Err(format!(
                "refusing to send a token over plain HTTP to {host}; use https:// (TLS reverse proxy) \
                 or pass --allow-insecure-http for a trusted private network"
            ));
        }
        Ok(())
    } else if url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!(
            "coordinator URL must start with http:// or https:// (got {url})"
        ))
    }
}

impl Coordinator {
    pub fn new(base: &str, token: Token, allow_insecure: bool) -> Result<Self, String> {
        check_transport(base, allow_insecure)?;
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build();
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            agent: ureq::Agent::new_with_config(config),
            max_body_bytes: 1 << 30,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn check(status: u16, body: &[u8], what: &str) -> Result<(), String> {
        if (200..300).contains(&status) {
            return Ok(());
        }
        let detail = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
        Err(format!("{what} failed: HTTP {status}: {detail}"))
    }

    fn read(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
    ) -> Result<(u16, Vec<u8>), String> {
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(self.max_body_bytes)
            .read_to_vec()
            .map_err(|error| format!("cannot read response: {error}"))?;
        Ok((status, body))
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<T, String> {
        let response = self
            .agent
            .get(&self.url(path))
            .header("Authorization", &self.token.header_value())
            .call()
            .map_err(|error| format!("{what} failed: {error}"))?;
        let (status, body) = self.read(response)?;
        Self::check(status, &body, what)?;
        serde_json::from_slice(&body).map_err(|error| format!("{what}: invalid response: {error}"))
    }

    /// POST JSON; returns `None` for 204 No Content.
    pub fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        what: &str,
    ) -> Result<Option<T>, String> {
        let payload = serde_json::to_vec(body).map_err(|error| error.to_string())?;
        let response = self
            .agent
            .post(&self.url(path))
            .header("Authorization", &self.token.header_value())
            .header("Content-Type", "application/json")
            .send(&payload[..])
            .map_err(|error| format!("{what} failed: {error}"))?;
        let (status, body) = self.read(response)?;
        if status == 204 {
            return Ok(None);
        }
        Self::check(status, &body, what)?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| format!("{what}: invalid response: {error}"))
    }

    pub fn missing(&self, digests: Vec<String>) -> Result<Vec<String>, String> {
        let response: Option<MissingResponse> = self.post_json(
            "/v1/blobs/missing",
            &MissingRequest { digests },
            "blob query",
        )?;
        Ok(response.map(|r| r.missing).unwrap_or_default())
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<ContentDigest, String> {
        let digest = digest_bytes(bytes);
        let response = self
            .agent
            .put(&self.url(&format!("/v1/blobs/{digest}")))
            .header("Authorization", &self.token.header_value())
            .send(bytes)
            .map_err(|error| format!("blob upload failed: {error}"))?;
        let (status, body) = self.read(response)?;
        Self::check(status, &body, "blob upload")?;
        Ok(digest)
    }

    /// Download a blob and verify it hashes to its name before returning it.
    pub fn get_blob(&self, digest: &str) -> Result<Vec<u8>, String> {
        let expected: ContentDigest = digest.parse()?;
        let response = self
            .agent
            .get(&self.url(&format!("/v1/blobs/{digest}")))
            .header("Authorization", &self.token.header_value())
            .call()
            .map_err(|error| format!("blob download failed: {error}"))?;
        let (status, body) = self.read(response)?;
        Self::check(status, &body, "blob download")?;
        let actual = digest_bytes(&body);
        if actual != expected {
            return Err(format!(
                "downloaded blob hashes to {actual}, expected {expected}"
            ));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_tokens_only_go_to_loopback() {
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost:1/x",
            "http://[::1]:9",
            "https://build.example.com",
        ] {
            assert!(check_transport(ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "http://10.0.0.5:8080",
            "http://build.example.com",
            "ftp://x",
            "127.0.0.1:80",
        ] {
            assert!(check_transport(bad, false).is_err(), "{bad}");
        }
        assert!(
            check_transport("http://10.0.0.5:8080", true).is_ok(),
            "explicit opt-in"
        );
    }
}
