//! HTTP client + WarcraftLogs session


use std::io::{Cursor, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use rand::Rng;
use regex::Regex;
use rquest::{Client, Impersonate};
use serde::Deserialize;
use serde_json::{json, Value};

const BASE_URL: &str = "https://www.warcraftlogs.com";
// This will be fetched dynamically
const FALLBACK_CLIENT_VERSION: &str = "9.0.1";
// These, well, we hope they dont chage/matter
const CHROME_VERSION: &str = "134.0.6998.205";
const ELECTRON_VERSION: &str = "37.7.0";
const MAX_RETRIES: u32 = 3;
const RETRY_BASE_DELAY_MS: u64 = 1000;

#[derive(Debug, Clone, Deserialize)]
pub struct LoginUser {
    #[serde(rename = "userName")]
    pub user_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub user: Option<LoginUser>,
}

pub struct ParserBundle {
    pub gamedata_code: String,
    pub parser_code: String,
    pub parser_version: i32,
}

pub struct WclSession {
    client: Client,
    client_version: String,
}

impl WclSession {
    pub async fn new() -> Result<Self> {
        let client_version = fetch_latest_client_version()
            .await
            .unwrap_or_else(|_| FALLBACK_CLIENT_VERSION.to_string());
        let client = Client::builder()
            .impersonate(Impersonate::Chrome133)
            .cookie_store(true)
            .build()?;
        Ok(Self { client, client_version })
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
        mut builder: rquest::RequestBuilder,
    ) -> Result<rquest::Response> {
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
                        tokio::time::sleep(Duration::from_millis(base + jitter)).await;
                        continue;
                    }
                    let body = r.text().await.unwrap_or_default();
                    bail!("HTTP {s}: {}", truncate(&body, 500));
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        let base = RETRY_BASE_DELAY_MS * (1u64 << attempt);
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
                    .post(format!("{BASE_URL}/desktop-client/log-in"))
                    .header("Content-Type", "application/json")
                    .json(&body),
            )
            .await?;
        Ok(resp.json::<LoginResponse>().await?)
    }

    pub async fn fetch_parser_code(&self) -> Result<ParserBundle> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis();
        let url = format!(
            "{BASE_URL}/desktop-client/parser?id=1&ts={ts}\
             &gameContentDetectionEnabled=false&metersEnabled=false&liveFightDataEnabled=false"
        );
        let resp = self.send_with_retry(self.client.get(&url)).await?;
        let html = resp.text().await?;

        let gamedata_re = Regex::new(r"(?s)<script[^>]*>(.*?window\.gameContentTypes.*?)</script>")?;
        let gamedata_code = gamedata_re
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();

        let parser_url_re =
            Regex::new(r#"src="(https://assets\.rpglogs\.com/js/parser-warcraft[^"]+)""#)?;
        let parser_url = parser_url_re
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .context("parser-warcraft script URL not found in parser page")?;

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
                    .post(format!("{BASE_URL}/desktop-client/create-report"))
                    .header("Content-Type", "application/json")
                    .json(&body),
            )
            .await?;
        let v: Value = resp.json().await?;
        v.get("code")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .context("create-report response missing `code`")
    }

    pub async fn set_master_table(
        &self,
        code: &str,
        segment_id: i64,
        zip_bytes: Vec<u8>,
    ) -> Result<()> {
        let (boundary, body) = build_multipart(
            &[("segmentId", &segment_id.to_string()), ("isRealTime", "false")],
            &[("logfile", "blob", "application/zip", zip_bytes)],
        );
        self.send_with_retry(
            self.client
                .post(format!(
                    "{BASE_URL}/desktop-client/set-report-master-table/{code}"
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
        zip_bytes: Vec<u8>,
    ) -> Result<i64> {
        let parameters = json!({
            "startTime": start_time,
            "endTime": end_time,
            "mythic": mythic,
            "isLiveLog": false,
            "isRealTime": false,
            "inProgressEventCount": 0,
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
                        "{BASE_URL}/desktop-client/add-report-segment/{code}"
                    ))
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(body),
            )
            .await?;
        let v: Value = resp.json().await?;
        Ok(v.get("nextSegmentId")
            .and_then(|n| n.as_i64())
            .unwrap_or(segment_id + 1))
    }

    pub async fn terminate_report(&self, code: &str) -> Result<()> {
        self.send_with_retry(
            self.client
                .post(format!("{BASE_URL}/desktop-client/terminate-report/{code}")),
        )
        .await?;
        Ok(())
    }
}

async fn fetch_latest_client_version() -> Result<String> {
    let client = rquest::Client::builder().build()?;
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

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
