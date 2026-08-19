//! HTTP client + WarcraftLogs session


use std::io::{Cursor, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use rand::Rng;
use regex::Regex;
use wreq::Client;
use wreq_util::Emulation;
use serde::Deserialize;
use serde_json::{json, Value};

// This will be fetched dynamically
const FALLBACK_CLIENT_VERSION: &str = "9.0.1";
// These, well, we hope they dont chage/matter
const CHROME_VERSION: &str = "134.0.6998.205";
const ELECTRON_VERSION: &str = "37.7.0";
const MAX_RETRIES: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 1000;

/// A single RPGLogs site. The upload mechanism is identical across all of them;
/// only the base URL and the CDN parser bundle slug differ. `parser_slug` is the
/// `assets.rpglogs.com/js/parser-<slug>` bundle the site's parser page references.
#[derive(Debug, Clone, Copy)]
pub struct Game {
    pub id: &'static str,
    pub base_url: &'static str,
    pub parser_slug: &'static str,
}

/// Known RPGLogs sites. The first entry is the default fallback.
pub const GAMES: &[Game] = &[
    Game { id: "warcraft", base_url: "https://www.warcraftlogs.com", parser_slug: "warcraft" },
    Game { id: "ff",       base_url: "https://www.fflogs.com",       parser_slug: "ff" },
    Game { id: "eso",      base_url: "https://www.esologs.com",      parser_slug: "eso" },
    Game { id: "swtor",    base_url: "https://www.swtorlogs.com",    parser_slug: "swtor" },
    Game { id: "fellowship", base_url: "https://www.fellowshiplogs.com", parser_slug: "fellowship" },
];

/// Resolve a game by id, defaulting to Warcraft Logs for unknown/empty ids.
pub fn game_by_id(id: &str) -> Game {
    GAMES.iter().copied().find(|g| g.id == id).unwrap_or(GAMES[0])
}
#[derive(Debug)]
pub struct HttpStatus(pub u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginUser {
    #[serde(rename = "userName")]
    pub user_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub user: Option<LoginUser>,
    /// Guild picker entries, a sibling of `user` in the login response. Shape is
    /// left as raw JSON so a change on their side can't break login; `value` is
    /// the guild id (-1 = "Personal Logs"), plus `label` and `regionId`.
    #[serde(rename = "guildSelectItems", default)]
    pub guild_select_items: Option<Vec<Value>>,
}

pub struct ParserBundle {
    pub gamedata_code: String,
    pub parser_code: String,
    pub parser_version: i32,
}

pub struct WclSession {
    client: Client,
    client_version: String,
    game: Game,
}

impl WclSession {
    pub async fn new(game_id: &str) -> Result<Self> {
        let game = game_by_id(game_id);
        let client_version = match fetch_latest_client_version().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[wcl] failed to fetch latest client version, \
                     using fallback {FALLBACK_CLIENT_VERSION}: {e:#}"
                );
                FALLBACK_CLIENT_VERSION.to_string()
            }
        };
        let client = Client::builder()
            .emulation(Emulation::Chrome133)
            .cookie_store(true)
            .build()?;
        Ok(Self { client, client_version, game })
    }

    /// Public report URL for a finished report on this game's site.
    pub fn report_url(&self, code: &str) -> String {
        format!("{}/reports/{code}", self.game.base_url)
    }

    fn user_agent(&self) -> String {
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) ArchonApp/{} Chrome/{} Electron/{} Safari/537.36",
            self.client_version, CHROME_VERSION, ELECTRON_VERSION
        )
    }

    /// exponential backoff + jitter on 429/5xx.
    async fn send_with_retry(
        &self,
        mut builder: wreq::RequestBuilder,
    ) -> Result<wreq::Response> {
        builder = builder.header("User-Agent", self.user_agent());
        for attempt in 0..=MAX_RETRIES {
            let req = builder
                .try_clone()
                .ok_or_else(|| anyhow!("request body not cloneable for retry"))?;
            let resp = req.send().await;
            match resp {
                Ok(r) => {
                    let s = r.status().as_u16();
                    if s < 400 {
                        return Ok(r);
                    }
                    if (s == 429 || s >= 500) && attempt < MAX_RETRIES {
                        let base = RETRY_BASE_DELAY_MS * (1u64 << attempt);
                        let jitter: u64 = rand::thread_rng().gen_range(0..1000);
                        eprintln!(
                            "[wcl] HTTP {s} from {}, retrying in {}ms (attempt {}/{MAX_RETRIES})",
                            r.url(),
                            base + jitter,
                            attempt + 1
                        );
                        tokio::time::sleep(Duration::from_millis(base + jitter)).await;
                        continue;
                    }
                    let url = r.url().clone();
                    let body = r.text().await.unwrap_or_default();
                    let detail = serde_json::from_str::<Value>(&body)
                        .ok()
                        .as_ref()
                        .and_then(api_error_message)
                        .unwrap_or_else(|| {
                            if body.trim().is_empty() {
                                "(empty body)".to_string()
                            } else {
                                truncate(&body, 500)
                            }
                        });
                    return Err(anyhow::Error::new(HttpStatus(s))
                        .context(format!("{detail} ({url})")));
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        let base = RETRY_BASE_DELAY_MS * (1u64 << attempt);
                        eprintln!(
                            "[wcl] request failed ({e}), retrying in {base}ms \
                             (attempt {}/{MAX_RETRIES})",
                            attempt + 1
                        );
                        tokio::time::sleep(Duration::from_millis(base)).await;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
        unreachable!()
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<LoginResponse> {
        let body = json!({
            "email": email,
            "password": password,
            "version": self.client_version,
        });
        let resp = self
            .send_with_retry(
                self.client
                    .post(format!("{}/desktop-client/log-in", self.game.base_url))
                    .header("Content-Type", "application/json")
                    .json(&body),
            )
            .await
            .context("log-in request failed")?;
        let v = json_body(resp, "log-in").await?;
        let login: LoginResponse = serde_json::from_value(v.clone()).with_context(|| {
            format!("log-in: unexpected response shape: {}", truncate(&v.to_string(), 500))
        })?;
        if login.user.is_none() {
            let detail =
                api_error_message(&v).unwrap_or_else(|| truncate(&v.to_string(), 500));
            return Err(anyhow!("login to {} failed: {detail}", self.game.base_url));
        }
        Ok(login)
    }

    pub async fn fetch_parser_code(&self) -> Result<ParserBundle> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis();
        let url = format!(
            "{}/desktop-client/parser?id=1&ts={ts}\
             &gameContentDetectionEnabled=false&metersEnabled=false&liveFightDataEnabled=false",
            self.game.base_url
        );
        let resp = self
            .send_with_retry(self.client.get(&url))
            .await
            .context("fetching parser page failed")?;
        let html = resp.text().await?;

        let gamedata_re = Regex::new(r"(?s)<script[^>]*>(.*?window\.gameContentTypes.*?)</script>")?;
        let gamedata_code = gamedata_re
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();

        let specific = Regex::new(&format!(
            r#"src="(https://assets\.rpglogs\.com/js/(?:[\w-]+/)*parser-{}[^"]+)""#,
            regex::escape(self.game.parser_slug)
        ))?;
        let generic =
            Regex::new(r#"src="(https://assets\.rpglogs\.com/js/(?:[\w-]+/)*parser-[^"]+)""#)?;
        let parser_url = specific
            .captures(&html)
            .or_else(|| generic.captures(&html))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .with_context(|| {
                format!(
                    "parser script URL not found in parser page ({} bytes): {}",
                    html.len(),
                    truncate(&html, 300)
                )
            })?;

        let parser_resp = self.send_with_retry(self.client.get(&parser_url)).await?;
        let parser_code = parser_resp.text().await?;

        let pv_re = Regex::new(r"const parserVersion\s*=\s*(\d+)")?;
        let parser_version = pv_re
            .captures(&html)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(59);

        Ok(ParserBundle {
            gamedata_code,
            parser_code,
            parser_version,
        })
    }

    pub async fn create_report(
        &self,
        filename: &str,
        start_time: i64,
        end_time: i64,
        region: i32,
        visibility: i32,
        guild_id: Option<i64>,
        parser_version: i32,
    ) -> Result<String> {
        let body = json!({
            "clientVersion": self.client_version,
            "parserVersion": parser_version,
            "startTime": start_time,
            "endTime": end_time,
            "guildId": guild_id,
            "fileName": filename,
            "serverOrRegion": region,
            "visibility": visibility,
            "reportTagId": serde_json::Value::Null,
            "description": "",
        });
        let resp = self
            .send_with_retry(
                self.client
                    .post(format!("{}/desktop-client/create-report", self.game.base_url))
                    .header("Content-Type", "application/json")
                    .json(&body),
            )
            .await
            .context("create-report request failed")?;
        let v = json_body(resp, "create-report").await?;
        v.get("code")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let detail =
                    api_error_message(&v).unwrap_or_else(|| truncate(&v.to_string(), 500));
                anyhow!("create-report did not return a report code: {detail}")
            })
    }

    pub async fn set_master_table(
        &self,
        code: &str,
        segment_id: i64,
        is_real_time: bool,
        zip_bytes: Vec<u8>,
    ) -> Result<()> {
        let (boundary, body) = build_multipart(
            &[
                ("segmentId", &segment_id.to_string()),
                ("isRealTime", if is_real_time { "true" } else { "false" }),
            ],
            &[("logfile", "blob", "application/zip", zip_bytes)],
        );
        self.send_with_retry(
            self.client
                .post(format!(
                    "{}/desktop-client/set-report-master-table/{code}",
                    self.game.base_url
                ))
                .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
                .body(body),
        )
        .await?;
        Ok(())
    }

    pub async fn add_segment(
        &self,
        code: &str,
        segment_id: i64,
        start_time: i64,
        end_time: i64,
        mythic: i32,
        is_live_log: bool,
        is_real_time: bool,
        in_progress_event_count: i64,
        zip_bytes: Vec<u8>,
    ) -> Result<i64> {
        let parameters = json!({
            "startTime": start_time,
            "endTime": end_time,
            "mythic": mythic,
            "isLiveLog": is_live_log,
            "isRealTime": is_real_time,
            "inProgressEventCount": in_progress_event_count,
            "segmentId": segment_id,
        });
        let (boundary, body) = build_multipart(
            &[("parameters", &parameters.to_string())],
            &[("logfile", "blob", "application/zip", zip_bytes)],
        );
        let resp = self
            .send_with_retry(
                self.client
                    .post(format!(
                        "{}/desktop-client/add-report-segment/{code}",
                        self.game.base_url
                    ))
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(body),
            )
            .await
            .context("add-report-segment request failed")?;
        let v = json_body(resp, "add-report-segment").await?;
        match v.get("nextSegmentId").and_then(|n| n.as_i64()) {
            Some(next) => Ok(next),
            None => match api_error_message(&v) {
                Some(msg) => Err(anyhow!("add-report-segment failed: {msg}")),
                // 0 means "don't advance" 
                None => Ok(0),
            },
        }
    }

    pub async fn terminate_report(&self, code: &str) -> Result<()> {
        self.send_with_retry(
            self.client
                .post(format!("{}/desktop-client/terminate-report/{code}", self.game.base_url)),
        )
        .await?;
        Ok(())
    }
}

async fn fetch_latest_client_version() -> Result<String> {
    let client = wreq::Client::builder().build()?;
    let resp = client
        .get("https://api.github.com/repos/RPGLogs/Uploaders-archon/releases/latest")
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", "wcl-upload")
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let v: Value = resp.json().await?;
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .context("no release name")?;
    Ok(name)
}

fn build_multipart(
    fields: &[(&str, &str)],
    files: &[(&str, &str, &str, Vec<u8>)],
) -> (String, Vec<u8>) {
    let boundary = format!(
        "----WebKitFormBoundary{}",
        random_alnum(16)
    );
    let mut body: Vec<u8> = Vec::new();
    for (name, value) in fields {
        let part = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
            boundary = boundary,
            name = name,
            value = value
        );
        body.extend_from_slice(part.as_bytes());
    }
    for (name, fname, ctype, data) in files {
        let header = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; \
             filename=\"{fname}\"\r\nContent-Type: {ctype}\r\n\r\n",
            boundary = boundary,
            name = name,
            fname = fname,
            ctype = ctype
        );
        body.extend_from_slice(header.as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

fn random_alnum(n: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

pub fn make_zip(content: &str) -> Result<Vec<u8>> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(6));
        zw.start_file("log.txt", opts)?;
        zw.write_all(content.as_bytes())?;
        zw.finish()?;
    }
    Ok(buf)
}


/// the master-table fingerprint: upload a new table only when these change.
pub fn master_ids(m: &Value) -> (i64, i64, i64, i64) {
    (
        m.get("lastAssignedActorID").and_then(|v| v.as_i64()).unwrap_or(0),
        m.get("lastAssignedAbilityID").and_then(|v| v.as_i64()).unwrap_or(0),
        m.get("lastAssignedTupleID").and_then(|v| v.as_i64()).unwrap_or(0),
        m.get("lastAssignedPetID").and_then(|v| v.as_i64()).unwrap_or(0),
    )
}

pub fn build_master_string(m: &Value, log_version: i64, game_version: i64) -> String {
    let mut parts = vec![format!("{log_version}|{game_version}|")];
    for (key, skey) in &[
        ("lastAssignedActorID", "actorsString"),
        ("lastAssignedAbilityID", "abilitiesString"),
        ("lastAssignedTupleID", "tuplesString"),
        ("lastAssignedPetID", "petsString"),
    ] {
        let last = m.get(*key).and_then(|v| v.as_i64()).unwrap_or(0);
        parts.push(last.to_string());
        let s = m.get(*skey).and_then(|v| v.as_str()).unwrap_or("");
        if !s.is_empty() {
            parts.push(s.trim_end_matches('\n').to_string());
        }
    }
    parts.join("\n") + "\n"
}


pub fn build_fights_string(fd: &Value) -> String {
    let log_version = fd.get("logVersion").and_then(|v| v.as_i64()).unwrap_or(0);
    let game_version = fd.get("gameVersion").and_then(|v| v.as_i64()).unwrap_or(0);
    let fights = fd.get("fights").and_then(|v| v.as_array());
    let total: i64 = fights
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("eventCount").and_then(|n| n.as_i64()))
                .sum()
        })
        .unwrap_or(0);
    let evts: String = fights
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("eventsString").and_then(|s| s.as_str()))
                .collect()
        })
        .unwrap_or_default();
    format!("{log_version}|{game_version}\n{total}\n{evts}")
}

pub fn parse_start_date(filename: &str) -> Option<String> {
    let re = Regex::new(r"WoWCombatLog-(\d{2})(\d{2})(\d{2})_").ok()?;
    let c = re.captures(filename)?;
    let mm: i32 = c.get(1)?.as_str().parse().ok()?;
    let dd: i32 = c.get(2)?.as_str().parse().ok()?;
    let yy: i32 = c.get(3)?.as_str().parse().ok()?;
    Some(format!("{mm}/{dd}/{}", 2000 + yy))
}

async fn json_body(resp: wreq::Response, what: &str) -> Result<Value> {
    let status = resp.status().as_u16();
    let url = resp.url().clone();
    let text = resp
        .text()
        .await
        .with_context(|| format!("{what}: failed reading response body from {url}"))?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{what}: HTTP {status} from {url} returned non-JSON body: {}",
            truncate(&text, 500)
        )
    })
}

/// This is a best-effort extraction of an error message from the WCL JSON payload.
/// Sadly can't guarantee for it to work every time
fn api_error_message(v: &Value) -> Option<String> {
    for key in ["error", "message", "reason"] {
        if let Some(s) = v.get(key).and_then(|e| e.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if v.get("success").and_then(|b| b.as_bool()) == Some(false) {
        return Some(format!("server reported failure: {}", truncate(&v.to_string(), 300)));
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run with: WCL_EMAIL=... WCL_PASSWORD=... cargo test login_error -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "hits the live WCL API; needs WCL_EMAIL/WCL_PASSWORD"]
    async fn login_error_is_descriptive() {
        let email = std::env::var("WCL_EMAIL").expect("WCL_EMAIL not set");
        let password = std::env::var("WCL_PASSWORD").expect("WCL_PASSWORD not set");
        let session = WclSession::new("warcraft").await.expect("session");
        match session.login(&email, &password).await {
            Ok(l) => println!("login OK as {:?}", l.user.and_then(|u| u.user_name)),
            Err(e) => println!("login error: {e:#}"),
        }
    }
}
