//! `ant publish android` (roadmap P9, decision D-062): build the signed
//! release App Bundle and upload it to a Google Play track.
//!
//! Authentication is a Google Cloud service account with access to the app
//! in Play Console (its JSON key file). Ant signs a short-lived JWT with the
//! key (RS256), exchanges it at the key's `token_uri` for an access token
//! scoped to the Android Publisher API, then: creates an edit, uploads the
//! bundle, assigns it to the track, and commits the edit. The key file is
//! stored sealed (D-052) after its first use, like signing passwords; it
//! never appears in logs or arguments.
//!
//! Google requires the very first bundle of a new app to be uploaded in Play
//! Console by hand; every later version can come from here.

use base64::Engine as _;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "usage: ant publish android [--track internal|alpha|beta|production] [--draft] [--key SERVICE_ACCOUNT.json] [--project PATH]
       (advanced: --aab FILE --package APPLICATION_ID publish an existing bundle without building)";

const SCOPE: &str = "https://www.googleapis.com/auth/androidpublisher";
const DEFAULT_API: &str = "https://androidpublisher.googleapis.com";

/// The fields of a Google service-account key file that Ant uses.
#[derive(serde::Deserialize)]
struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key: String,
    token_uri: String,
}

pub fn publish_command(mut args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    match args.next().map(|arg| arg.to_string_lossy().into_owned()).as_deref() {
        Some("android") => publish_android(args.map(|arg| arg.to_string_lossy().into_owned())),
        Some("ios") => Err(
            "publishing to the App Store needs an Apple Developer account and App Store Connect API key; \
             Ant does not do it yet"
                .to_owned(),
        ),
        _ => Err(USAGE.to_owned()),
    }
}

fn publish_android(mut args: impl Iterator<Item = String>) -> Result<u8, String> {
    let mut track = "internal".to_owned();
    let mut draft = false;
    let mut key = None;
    let mut project = None;
    let mut aab = None;
    let mut package = None;
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--track" => track = value("--track")?,
            "--draft" => draft = true,
            "--key" => key = Some(PathBuf::from(value("--key")?)),
            "--project" => project = Some(value("--project")?),
            "--aab" => aab = Some(PathBuf::from(value("--aab")?)),
            "--package" => package = Some(value("--package")?),
            other => return Err(format!("unexpected argument '{other}'\n{USAGE}")),
        }
    }
    if !track
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')
        || track.is_empty()
    {
        return Err(format!("'{track}' is not a track name"));
    }
    let account = service_account(key.as_deref())?;

    let (bundle, app_id) = match (aab, package) {
        (Some(aab), Some(package)) => (aab, package),
        (None, None) => build_bundle(project.as_deref())?,
        _ => return Err("--aab and --package go together".to_owned()),
    };
    let bytes = std::fs::read(&bundle)
        .map_err(|error| format!("cannot read {}: {error}", bundle.display()))?;
    println!(
        "Publishing {} ({app_id}, {:.1} MB) to the {track} track…",
        bundle.display(),
        bytes.len() as f64 / 1_048_576.0
    );
    let play = Play::new(&account)?;
    let edit = play.create_edit(&app_id)?;
    let version = play.upload_bundle(&app_id, &edit, bytes)?;
    println!("  uploaded: versionCode {version}");
    play.assign_track(&app_id, &edit, &track, version, draft)?;
    play.commit(&app_id, &edit)?;
    println!(
        "Published versionCode {version} of {app_id} to {track}{}.",
        if draft { " as a draft" } else { "" }
    );
    println!("  Play Console: https://play.google.com/console/developers/app-list");
    Ok(0)
}

/// The service-account key: `--key FILE` (then remembered, sealed) or the
/// one remembered before.
fn service_account(key: Option<&Path>) -> Result<ServiceAccount, String> {
    let dir = crate::signing::data_dir()
        .ok_or("cannot find a per-user data directory")?
        .join("play");
    let sealed = dir.join(crate::signing::sealed_name("service-account"));
    let text = match key {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let account: ServiceAccount = serde_json::from_str(&text).map_err(|_| {
                format!(
                    "{} is not a Google service-account key file",
                    path.display()
                )
            })?;
            if account.kind != "service_account" {
                return Err(format!("{} is not a service-account key", path.display()));
            }
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            if let Ok(old) = std::fs::read(&sealed) {
                crate::signing::forget_sealed(&old);
            }
            let _ = std::fs::remove_file(&sealed);
            crate::signing::write_private(&sealed, &crate::signing::seal(text.as_bytes())?)?;
            println!(
                "Remembered the service account {} ({}). You can delete {} now.",
                account.client_email,
                crate::signing::protection_of(&std::fs::read(&sealed).unwrap_or_default()),
                path.display()
            );
            text
        }
        None => {
            let blob = std::fs::read(&sealed).map_err(|_| {
                "no Google Play service account yet: pass --key path/to/service-account.json once \
                 (Play Console → Users and permissions → invite the service account with release rights)"
                    .to_owned()
            })?;
            String::from_utf8(crate::signing::unseal(&blob)?)
                .map_err(|_| "the stored service account is damaged".to_owned())?
        }
    };
    let account: ServiceAccount = serde_json::from_str(&text)
        .map_err(|_| "the stored service account is damaged".to_owned())?;
    Ok(account)
}

/// `ant build android --release`, then the bundle it produced and the app id.
fn build_bundle(project: Option<&str>) -> Result<(PathBuf, String), String> {
    let mut args: Vec<OsString> = vec!["android".into(), "--release".into()];
    if let Some(project) = project {
        args.extend(["--project".into(), project.into()]);
    }
    let (code, deliverables) = crate::front::build_and_collect(args.into_iter())?;
    if code != 0 {
        return Err("the release build failed; nothing was published".to_owned());
    }
    let bundle = deliverables
        .iter()
        .find(|path| path.extension().is_some_and(|extension| extension == "aab"))
        .cloned()
        .ok_or("the release build produced no .aab")?;
    let root = match project {
        Some(project) => PathBuf::from(project),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    let plan: build_graph::PlanFile = serde_json::from_slice(
        &std::fs::read(root.join(".ant").join("plan.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let outputs = build_adapter::declared_file_outputs(&plan);
    let app_id = crate::signing::android_app_id(&root, &outputs)?;
    Ok((bundle, app_id))
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// An RS256-signed JWT for the OAuth 2.0 JWT bearer grant (RFC 7523).
fn signed_assertion(account: &ServiceAccount, now_s: u64) -> Result<String, String> {
    let header = base64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": account.client_email,
        "scope": SCOPE,
        "aud": account.token_uri,
        "iat": now_s,
        "exp": now_s + 3600,
    });
    let claims = base64url(claims.to_string().as_bytes());
    let message = format!("{header}.{claims}");
    let pem = account.private_key.trim();
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|_| "the service account's private key is not valid PEM".to_owned())?;
    let key = ring::signature::RsaKeyPair::from_pkcs8(&der)
        .map_err(|_| "the service account's private key is not an RSA PKCS#8 key".to_owned())?;
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "cannot sign the token request".to_owned())?;
    Ok(format!("{message}.{}", base64url(&signature)))
}

struct Play {
    agent: ureq::Agent,
    api: String,
    token: String,
}

impl Play {
    fn new(account: &ServiceAccount) -> Result<Self, String> {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .user_agent(concat!("ant/", env!("CARGO_PKG_VERSION")))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs();
        let assertion = signed_assertion(account, now)?;
        let form = format!(
            "grant_type={}&assertion={assertion}",
            "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"
        );
        let answer = Self::json(
            agent
                .post(&account.token_uri)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .send(form.as_bytes()),
            "Google sign-in for the service account",
        )?;
        let token = answer["access_token"]
            .as_str()
            .ok_or("Google returned no access token")?
            .to_owned();
        // Tests point this at a local fake; nothing else should.
        let api = std::env::var("ANT_PLAY_API").unwrap_or_else(|_| DEFAULT_API.to_owned());
        Ok(Self { agent, api, token })
    }

    fn json(
        response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        what: &str,
    ) -> Result<serde_json::Value, String> {
        let mut response = response.map_err(|error| format!("{what} failed: {error}"))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| format!("{what}: {error}"))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value["error"]["message"]
                        .as_str()
                        .or_else(|| value["error_description"].as_str())
                        .map(str::to_owned)
                })
                .unwrap_or(body);
            return Err(format!("{what} failed (HTTP {status}): {message}"));
        }
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&body).map_err(|error| format!("{what}: invalid answer: {error}"))
    }

    fn url(&self, app_id: &str, rest: &str) -> String {
        format!(
            "{}/androidpublisher/v3/applications/{app_id}/{rest}",
            self.api
        )
    }

    fn create_edit(&self, app_id: &str) -> Result<String, String> {
        let answer = Self::json(
            self.agent
                .post(&self.url(app_id, "edits"))
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("Content-Type", "application/json")
                .send("{}"),
            "starting a Play edit",
        )?;
        answer["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Play returned no edit id".to_owned())
    }

    fn upload_bundle(&self, app_id: &str, edit: &str, bytes: Vec<u8>) -> Result<u64, String> {
        let url = format!(
            "{}/upload/androidpublisher/v3/applications/{app_id}/edits/{edit}/bundles?uploadType=media",
            self.api
        );
        let answer = Self::json(
            self.agent
                .post(&url)
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("Content-Type", "application/octet-stream")
                .send(&bytes[..]),
            "uploading the bundle",
        )?;
        answer["versionCode"]
            .as_u64()
            .ok_or_else(|| "Play returned no versionCode".to_owned())
    }

    fn assign_track(
        &self,
        app_id: &str,
        edit: &str,
        track: &str,
        version: u64,
        draft: bool,
    ) -> Result<(), String> {
        let body = serde_json::json!({
            "track": track,
            "releases": [{
                "versionCodes": [version.to_string()],
                "status": if draft { "draft" } else { "completed" },
            }],
        });
        Self::json(
            self.agent
                .put(&self.url(app_id, &format!("edits/{edit}/tracks/{track}")))
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("Content-Type", "application/json")
                .send(body.to_string().as_bytes()),
            "assigning the release to the track",
        )
        .map(|_| ())
    }

    fn commit(&self, app_id: &str, edit: &str) -> Result<(), String> {
        Self::json(
            self.agent
                .post(&self.url(app_id, &format!("edits/{edit}:commit")))
                .header("Authorization", &format!("Bearer {}", self.token))
                .header("Content-Type", "application/json")
                .send("{}"),
            "committing the Play edit",
        )
        .map(|_| ())
    }
}
