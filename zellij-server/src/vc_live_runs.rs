//! Vibecrafted Server Live-runs feed — the semantic source behind the rail's
//! `● Live N` row and its read surface.
//!
//! VC Frame does not infer semantic liveness from local files, PIDs, or tabs.
//! It resolves the effective Vibecrafted Server origin from the operator's
//! settings and reads `active_runs` from `/api/control/state`. Zellij remains
//! the owner of physical sessions and panes; Vibecrafted Server owns run truth.

use isahc::prelude::*;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

/// CustomMessage name AND payload schema tag — one string, one contract.
pub const VC_LIVE_RUNS_MESSAGE: &str = "vc.live-runs.v1";
const DEFAULT_SERVER_PUBLIC_URL: &str = "http://127.0.0.1:3024";
const CONTROL_STATE_PATH: &str = "api/control/state";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(900);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRunCard {
    pub run_id: String,
    pub agent: String,
    pub skill: String,
    /// Basename of the run's `root` workspace — enough for a compact card.
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRunsSnapshot {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    pub runs: Vec<LiveRunCard>,
}

impl LiveRunsSnapshot {
    pub fn new(runs: Vec<LiveRunCard>) -> Self {
        Self {
            schema: VC_LIVE_RUNS_MESSAGE.to_owned(),
            server_url: None,
            runs,
        }
    }

    fn from_server(origin: &Url, runs: Vec<LiveRunCard>) -> Self {
        Self {
            schema: VC_LIVE_RUNS_MESSAGE.to_owned(),
            server_url: Some(origin.as_str().trim_end_matches('/').to_owned()),
            runs,
        }
    }

    pub fn payload(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

#[derive(Debug, Deserialize, Default)]
struct VibecraftedConfig {
    #[serde(default)]
    server: Option<ServerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    #[serde(default = "default_bind_host")]
    bind_host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    public_url: String,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_owned()
}

const fn default_port() -> u16 {
    3024
}

/// Resolve the endpoint exactly where Vibecrafted owns it: a one-process
/// `VC_SERVER_URL` override, then `[server]` in the XDG config. Missing config
/// has the same localhost default as the installed Vibecrafted runtime.
pub fn configured_server_public_url() -> Result<Url, String> {
    let override_url = std::env::var("VC_SERVER_URL").ok();
    resolve_server_public_url(override_url.as_deref(), &vibecrafted_config_path())
}

fn vibecrafted_config_path() -> PathBuf {
    if let Some(config_home) = nonempty_env("XDG_CONFIG_HOME") {
        return expand_leading_tilde(config_home).join("vibecrafted/config.toml");
    }
    let home = nonempty_env("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config/vibecrafted/config.toml")
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn expand_leading_tilde(value: String) -> PathBuf {
    if value == "~" {
        return nonempty_env("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = nonempty_env("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

fn resolve_server_public_url(
    override_url: Option<&str>,
    config_path: &Path,
) -> Result<Url, String> {
    if let Some(override_url) = override_url.map(str::trim).filter(|url| !url.is_empty()) {
        return validate_server_origin(override_url, "VC_SERVER_URL");
    }

    let raw = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return validate_server_origin(DEFAULT_SERVER_PUBLIC_URL, "Vibecrafted default");
        },
        Err(error) => {
            return Err(format!(
                "cannot read Vibecrafted settings at {}: {error}",
                config_path.display()
            ));
        },
    };
    let config: VibecraftedConfig = toml::from_str(&raw).map_err(|error| {
        format!(
            "invalid Vibecrafted settings at {}: {error}",
            config_path.display()
        )
    })?;
    let origin = match config.server {
        Some(server) if !server.public_url.trim().is_empty() => server.public_url,
        Some(server) => origin_for(&server.bind_host, server.port),
        None => DEFAULT_SERVER_PUBLIC_URL.to_owned(),
    };
    validate_server_origin(&origin, "server.public_url")
}

fn origin_for(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

fn validate_server_origin(value: &str, source: &str) -> Result<Url, String> {
    let mut url = Url::parse(value).map_err(|error| format!("invalid {source}: {error}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(format!(
            "invalid {source}: expected an HTTP(S) origin without credentials, path, query, or fragment"
        ));
    }
    url.set_path("");
    Ok(url)
}

fn control_state_url(origin: &Url) -> Result<Url, String> {
    origin
        .join(CONTROL_STATE_PATH)
        .map_err(|error| format!("cannot build Vibecrafted control-state URL: {error}"))
}

/// Fetch the one canonical list. Failure is returned to the caller so the UI
/// can retain the previous value as degraded instead of lying with local data.
pub async fn fetch_live_runs(http_client: &HttpClient) -> Result<LiveRunsSnapshot, String> {
    let origin = configured_server_public_url()?;
    fetch_live_runs_from(http_client, &origin).await
}

async fn fetch_live_runs_from(
    http_client: &HttpClient,
    origin: &Url,
) -> Result<LiveRunsSnapshot, String> {
    let endpoint = control_state_url(origin)?;
    let request = Request::get(endpoint.as_str())
        .timeout(REQUEST_TIMEOUT)
        .body(())
        .map_err(|error| format!("cannot build Vibecrafted Server request: {error}"))?;
    let mut response = http_client
        .send_async(request)
        .await
        .map_err(|error| format!("Vibecrafted Server request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Vibecrafted Server returned HTTP {} for {endpoint}",
            response.status()
        ));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("cannot read Vibecrafted Server response: {error}"))?;
    let runs = parse_control_state(&body)?.runs;
    Ok(LiveRunsSnapshot::from_server(origin, runs))
}

fn parse_control_state(body: &str) -> Result<LiveRunsSnapshot, String> {
    #[derive(Deserialize)]
    struct ControlState {
        active_runs: Vec<ServerRun>,
    }
    #[derive(Deserialize)]
    struct ServerRun {
        run_id: String,
        #[serde(default)]
        agent: String,
        #[serde(default)]
        skill: String,
        #[serde(default)]
        root: String,
        #[serde(default)]
        worker_pid: Option<i64>,
    }

    let state: ControlState = serde_json::from_str(body)
        .map_err(|error| format!("invalid Vibecrafted control-state response: {error}"))?;
    let mut runs = state
        .active_runs
        .into_iter()
        .map(|run| LiveRunCard {
            run_id: run.run_id,
            agent: run.agent,
            skill: run.skill,
            repo: Path::new(&run.root)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_owned(),
            worker_pid: run.worker_pid,
        })
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
    Ok(LiveRunsSnapshot::new(runs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(contents: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), contents).unwrap();
        tmp
    }

    #[test]
    fn settings_accept_arbitrary_ipv4_hostname_and_ipv6_origins() {
        for origin in [
            "http://100.82.232.70:3025",
            "https://observer.tailnet.example:8443",
            "http://[fd7a:115c:a1e0::1]:3025",
        ] {
            let tmp = write_config(&format!("[server]\npublic_url = \"{origin}\"\n"));
            let resolved = resolve_server_public_url(None, &tmp.path().join("config.toml"))
                .expect("configured origin should resolve");
            assert_eq!(resolved.as_str().trim_end_matches('/'), origin);
        }
    }

    #[test]
    fn process_override_wins_over_persistent_settings() {
        let tmp = write_config("[server]\npublic_url = \"http://settings.example:3025\"\n");
        let resolved = resolve_server_public_url(
            Some("https://override.example:9443"),
            &tmp.path().join("config.toml"),
        )
        .unwrap();
        assert_eq!(resolved.as_str(), "https://override.example:9443/");
    }

    #[test]
    fn bind_host_and_port_form_the_effective_origin_when_public_url_is_absent() {
        let tmp = write_config("[server]\nbind_host = \"fd7a:115c:a1e0::1\"\nport = 4040\n");
        let resolved = resolve_server_public_url(None, &tmp.path().join("config.toml")).unwrap();
        assert_eq!(resolved.as_str(), "http://[fd7a:115c:a1e0::1]:4040/");
    }

    #[test]
    fn credentials_and_non_origin_paths_are_rejected() {
        for origin in [
            "http://user:secret@example.com:3025",
            "http://example.com:3025/somewhere",
        ] {
            let tmp = write_config(&format!("[server]\npublic_url = \"{origin}\"\n"));
            assert!(resolve_server_public_url(None, &tmp.path().join("config.toml")).is_err());
        }
    }

    #[test]
    fn only_server_active_runs_become_live_cards() {
        let snapshot = parse_control_state(
            r#"{
                "active_runs": [
                    {"run_id":"work-260813-020000-2","agent":"codex","skill":"workflow","root":"/tmp/ws/vc-frame","worker_pid":22},
                    {"run_id":"work-260813-010000-1","agent":"claude","skill":"implement","root":"/tmp/ws/vibecrafted","worker_pid":null}
                ],
                "stalled_runs": [
                    {"run_id":"impl-stale","agent":"grok","root":"/tmp/ws/old","worker_pid":33}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            snapshot
                .runs
                .iter()
                .map(|run| run.run_id.as_str())
                .collect::<Vec<_>>(),
            vec!["work-260813-010000-1", "work-260813-020000-2"]
        );
        assert_eq!(snapshot.runs[0].repo, "vibecrafted");
        assert_eq!(snapshot.runs[0].worker_pid, None);
    }

    #[tokio::test]
    async fn fetches_active_runs_from_the_configured_server_endpoint() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let bytes_read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..bytes_read]);
            assert!(request.starts_with("GET /api/control/state HTTP/1.1"));
            let body = r#"{"active_runs":[{"run_id":"work-live","root":"/tmp/ws/vc-frame"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let client = HttpClient::builder().build().unwrap();
        let origin = Url::parse(&format!("http://{address}")).unwrap();
        let snapshot = fetch_live_runs_from(&client, &origin).await.unwrap();

        server.join().unwrap();
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(snapshot.runs[0].run_id, "work-live");
        assert_eq!(
            snapshot.server_url.as_deref(),
            Some(origin.as_str().trim_end_matches('/'))
        );
    }

    #[test]
    fn missing_active_runs_is_degraded_not_an_empty_census() {
        assert!(parse_control_state(r#"{"stalled_runs": []}"#).is_err());
    }

    #[test]
    fn snapshot_payload_carries_the_schema_tag() {
        let snapshot = LiveRunsSnapshot::new(vec![]);
        let payload = snapshot.payload().unwrap();
        assert!(payload.contains(r#""schema":"vc.live-runs.v1""#));
    }
}
