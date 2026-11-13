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

    /// `Some(entry)` on a cache hit, `None` on a miss.
    pub fn cache_lookup(&self, key: &str) -> Result<Option<crate::protocol::CacheEntry>, String> {
        let response = self
            .agent
            .get(&self.url(&format!("/v1/cache/{key}")))
            .header("Authorization", &self.token.header_value())
            .call()
            .map_err(|error| format!("cache lookup failed: {error}"))?;
        let (status, body) = self.read(response)?;
        if status == 404 {
            return Ok(None);
        }
        Self::check(status, &body, "cache lookup")?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| format!("cache lookup: invalid response: {error}"))
    }

    pub fn cache_store(
        &self,
        key: &str,
        worker_id: &str,
        outputs: &[crate::protocol::Entry],
    ) -> Result<(), String> {
        let request = crate::protocol::CacheStoreRequest {
            worker_id: worker_id.to_owned(),
            outputs: outputs.to_vec(),
        };
        let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        let response = self
            .agent
            .put(&self.url(&format!("/v1/cache/{key}")))
            .header("Authorization", &self.token.header_value())
            .header("Content-Type", "application/json")
            .send(&payload[..])
            .map_err(|error| format!("cache store failed: {error}"))?;
        let (status, body) = self.read(response)?;
        Self::check(status, &body, "cache store")
    }

    /// Upload many blobs in batched requests (D-023), split by count and by
    /// [`crate::protocol::BATCH_TARGET_BYTES`].
    pub fn put_blobs(&self, blobs: &[Vec<u8>]) -> Result<(), String> {
        let mut batch: Vec<(String, &[u8])> = Vec::new();
        let mut size = 0usize;
        for bytes in blobs {
            if !batch.is_empty()
                && (batch.len() >= crate::protocol::MAX_BATCH_BLOBS
                    || size + bytes.len() > crate::protocol::BATCH_TARGET_BYTES)
            {
                self.send_batch(&batch)?;
                batch.clear();
                size = 0;
            }
            batch.push((digest_bytes(bytes).to_string(), bytes.as_slice()));
            size += bytes.len();
        }
        if !batch.is_empty() {
            self.send_batch(&batch)?;
        }
        Ok(())
    }

    fn send_batch(&self, batch: &[(String, &[u8])]) -> Result<(), String> {
        let frames: Vec<(&str, &[u8])> = batch.iter().map(|(d, b)| (d.as_str(), *b)).collect();
        let payload = crate::protocol::encode_frames(&frames);
        let response = self
            .agent
            .post(&self.url("/v1/blobs/batch-put"))
            .header("Authorization", &self.token.header_value())
            .header("Content-Type", "application/octet-stream")
            .send(&payload[..])
            .map_err(|error| format!("batch upload failed: {error}"))?;
        let (status, body) = self.read(response)?;
        Self::check(status, &body, "batch upload")
    }

    /// Download many blobs in batched requests (D-023); every blob is
    /// verified against its digest before it is returned. Results are in
    /// the order of `digests`.
    pub fn get_blobs(&self, digests: &[String]) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::with_capacity(digests.len());
        for chunk in digests.chunks(crate::protocol::MAX_BATCH_BLOBS / 4) {
            let request = crate::protocol::BatchGetRequest {
                digests: chunk.to_vec(),
            };
            let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
            let response = self
                .agent
                .post(&self.url("/v1/blobs/batch-get"))
                .header("Authorization", &self.token.header_value())
                .header("Content-Type", "application/json")
                .send(&payload[..])
                .map_err(|error| format!("batch download failed: {error}"))?;
            let (status, body) = self.read(response)?;
            Self::check(status, &body, "batch download")?;
            let frames = crate::protocol::decode_frames(&body)?;
            if frames.len() != chunk.len() {
                return Err("batch download returned the wrong number of blobs".to_owned());
            }
            for ((digest, bytes), wanted) in frames.into_iter().zip(chunk) {
                let expected: ContentDigest = wanted.parse()?;
                if &digest != wanted || digest_bytes(&bytes) != expected {
                    return Err(format!("batch download: blob {wanted} failed verification"));
                }
                out.push(bytes);
            }
        }
        Ok(out)
    }

    /// Fetch a tool task-cache entry (D-025); `None` on a miss.
    pub fn task_cache_get(&self, tool: &str, key: &str) -> Result<Option<Vec<u8>>, String> {
        let response = self
            .agent
            .get(&self.url(&format!("/v1/task-cache/{tool}/{key}")))
            .header("Authorization", &self.token.header_value())
            .call()
            .map_err(|error| format!("task cache lookup failed: {error}"))?;
        let (status, body) = self.read(response)?;
        if status == 404 {
            return Ok(None);
        }
        Self::check(status, &body, "task cache lookup")?;
        Ok(Some(body))
    }

    /// Store a tool task-cache entry (worker tokens only, D-025).
    pub fn task_cache_put(&self, tool: &str, key: &str, bytes: &[u8]) -> Result<(), String> {
        let response = self
            .agent
            .put(&self.url(&format!("/v1/task-cache/{tool}/{key}")))
            .header("Authorization", &self.token.header_value())
            .send(bytes)
            .map_err(|error| format!("task cache store failed: {error}"))?;
        let (status, body) = self.read(response)?;
        Self::check(status, &body, "task cache store")
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
