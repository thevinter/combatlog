use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use crate::{parser, wcl};

const LOG_FILE_PATTERN: &str = r"^WoWCombatLog.*\.txt$";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_THRESHOLD: Duration = Duration::from_secs(120);
const MAX_FILE_AGE: Duration = Duration::from_secs(6 * 3600);
const MAX_CHUNK_LINES: usize = 5_000;
const MAX_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
const IN_PROGRESS_INTERVAL: Duration = Duration::from_secs(30);
const LIVE_RETRY_MAX: u32 = 120;
const LIVE_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LiveLogArgs {
    pub directory: String,
    pub email: String,
    pub password: String,
    pub region: i32,
    pub visibility: i32,
    pub guild_id: Option<i64>,
    pub include_entire_file_in_report: bool,
    pub enable_real_time_uploading: bool,
    #[serde(default)]
    pub game: String,
}

fn cancelled(rx: &watch::Receiver<bool>) -> bool {
    *rx.borrow() || rx.has_changed().is_err()
}

/// 1s sleep that returns early when the stop signal fires.
async fn idle_sleep(rx: &mut watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::time::sleep(POLL_INTERVAL) => {}
        _ = rx.changed() => {}
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct LiveProgress {
    app: AppHandle,
    file: Option<String>,
    segments: u64,
    in_progress: bool,
}

impl LiveProgress {
    fn emit(&self, state: &str, message: impl Into<String>) {
        let _ = self.app.emit(
            "live:progress",
            json!({
                "state": state,
                "message": message.into(),
                "file": self.file,
                "segments": self.segments,
                "inProgress": self.in_progress,
            }),
        );
    }
}

pub async fn run_live_log(
    app: AppHandle,
    args: LiveLogArgs,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let dir = PathBuf::from(&args.directory);
    if !dir.is_dir() {
        return Err(anyhow!("not a directory: {}", dir.display()));
    }
    let pattern = Regex::new(LOG_FILE_PATTERN)?;

    let mut progress = LiveProgress {
        app: app.clone(),
        file: None,
        segments: 0,
        in_progress: false,
    };

    progress.emit("waiting", "Initializing session...");
    let session = wcl::WclSession::new(if args.game.is_empty() {
        "warcraft"
    } else {
        &args.game
    })
    .await?;

    progress.emit("waiting", "Logging in...");
    let login = session.login(&args.email, &args.password).await?;
    let user_name = login
        .user
        .as_ref()
        .and_then(|u| u.user_name.as_deref())
        .unwrap_or("?")
        .to_string();
    progress.emit("waiting", format!("Logged in as {user_name}"));

    progress.emit("waiting", "Fetching latest parser...");
    let bundle = session.fetch_parser_code().await?;

    let harness = parser::harness_path(&app)?;
    progress.emit("waiting", "Starting parser...");
    let parser =
        parser::Parser::spawn(&app, &harness, &bundle.gamedata_code, &bundle.parser_code).await?;

    // from here on the sidecar must be closed and any created report terminated,
    // even when setup fails partway
    let mut report: Option<(String, String)> = None; // (code, url)
    let result = run_live_session(
        &app, args, &session, &parser, &bundle, &pattern, &dir, &mut cancel, &mut progress, &mut report,
    )
    .await;

    parser.close().await;
    if let Some((code, _)) = &report {
        if let Err(e) = session.terminate_report(code).await {
            eprintln!("[live] terminate_report failed: {e:#}");
            progress.emit(
                "error",
                format!("Report {code} could not be finalized on the server: {e:#}"),
            );
        }
    }

    result?;
    if let Some((code, url)) = &report {
        let _ = app.emit(
            "live:done",
            json!({"url": url, "code": code, "segments": progress.segments}),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_live_session(
    app: &AppHandle,
    args: LiveLogArgs,
    session: &wcl::WclSession,
    parser: &parser::Parser,
    bundle: &wcl::ParserBundle,
    pattern: &Regex,
    dir: &Path,
    cancel: &mut watch::Receiver<bool>,
    progress: &mut LiveProgress,
    report: &mut Option<(String, String)>,
) -> Result<()> {
    parser.clear_state().await?;

    let start_ms = now_ms();
    let code = session
        .create_report(
            "live.log",
            start_ms,
            start_ms,
            args.region,
            args.visibility,
            args.guild_id,
            bundle.parser_version,
        )
        .await?;
    let url = format!("https://www.warcraftlogs.com/reports/{code}");
    let _ = app.emit("live:started", json!({"code": code, "url": url}));
    progress.emit("waiting", format!("Live report created: {code}"));
    *report = Some((code.clone(), url));

    if !args.include_entire_file_in_report {
        // the existing file is still parsed for actor/ability state,
        // but fights before this moment are excluded from the report
        parser.set_live_logging_start_time(start_ms).await?;
    }

    tail_loop(args, session, parser, &code, pattern, dir, cancel, progress).await
}

#[allow(clippy::too_many_arguments)]
async fn tail_loop(
    args: LiveLogArgs,
    session: &wcl::WclSession,
    parser: &parser::Parser,
    code: &str,
    pattern: &Regex,
    dir: &Path,
    cancel: &mut watch::Receiver<bool>,
    progress: &mut LiveProgress,
) -> Result<()> {
    let mut uploader = Uploader {
        session,
        parser,
        code,
        args,
        segment_id: 1,
        last_master_ids: None,
        last_in_progress: None,
    };

    let mut current_path: Option<PathBuf> = None;
    let mut offset: u64 = 0;
    let mut last_data = Instant::now();
    let mut dirty = false; 
    let mut waiting_logged = false;

    while !cancelled(cancel) {
        let Some(path) = find_newest_log(dir, pattern).await else {
            if current_path.is_none() && !waiting_logged {
                progress.emit("waiting", "Waiting for a combat log to appear...");
                waiting_logged = true;
            }
            idle_sleep(cancel).await;
            continue;
        };

        if Some(&path) != current_path.as_ref() {
            // rotation: drain what's left of the old file first
            if let Some(old) = current_path.take() {
                while !cancelled(cancel) {
                    let Ok(chunk) = read_chunk(&old, offset).await else {
                        break; // old file gone
                    };
                    if chunk.lines.is_empty() {
                        break;
                    }
                    offset = chunk.new_offset;
                    if let Err(e) = uploader.upload_part(&chunk.lines, false, cancel, progress).await {
                        if cancelled(cancel) {
                            break; // stop requested mid-retry: fall through to final flush
                        }
                        return Err(e);
                    }
                }
            }
            offset = 0;
            let name = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("")
                .to_string();
            if let Some(date) = wcl::parse_start_date(&name) {
                parser.set_start_date(&date).await?;
            }
            progress.file = Some(name.clone());
            progress.emit("tailing", format!("Tailing {name}"));
            current_path = Some(path.clone());
            waiting_logged = false;
        }

        let chunk = match read_chunk(&path, offset).await {
            Ok(c) => c,
            Err(e) => {
                // transient (AV lock, file swapped out mid-read)
                eprintln!("[live] read error on {}: {e:#}", path.display());
                idle_sleep(cancel).await;
                continue;
            }
        };
        if chunk.file_size < offset {
            offset = 0;
            progress.emit("tailing", "Log truncated — reading from the beginning");
            continue;
        }

        let flush_result = if chunk.lines.is_empty() {
            if dirty && last_data.elapsed() > IDLE_THRESHOLD {
                progress.emit("idle", "Log idle — flushing current fight");
                dirty = false;
                uploader.upload_part(&[], true, cancel, progress).await
            } else {
                Ok(())
            }
        } else {
            offset = chunk.new_offset;
            last_data = Instant::now();
            dirty = true;
            uploader.upload_part(&chunk.lines, false, cancel, progress).await
        };
        if let Err(e) = flush_result {
            if cancelled(cancel) {
                break; // stop requested mid-retry: fall through to final flush
            }
            return Err(e);
        }
        if chunk.lines.is_empty() {
            idle_sleep(cancel).await;
        }
    }

    // uploads block the loop, so it can be well behind the game: drain what's left
    if let Some(path) = current_path.as_ref() {
        let size_at_stop = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
        if offset < size_at_stop {
            progress.emit("uploading", "Stopping — reading the rest of the log...");
            while offset < size_at_stop {
                let Ok(chunk) = read_chunk(path, offset).await else {
                    break;
                };
                if chunk.lines.is_empty() {
                    break;
                }
                offset = chunk.new_offset;
                if let Err(e) = uploader.upload_part(&chunk.lines, false, cancel, progress).await {
                    eprintln!("[live] drain after stop failed: {e:#}");
                    break;
                }
            }
        }
    }

    // final flush so an in-progress fight makes it into the report
    progress.emit("uploading", "Stopping — flushing final data...");
    if let Err(e) = uploader.upload_part(&[], true, cancel, progress).await {
        eprintln!("[live] final flush failed: {e:#}");
    }
    Ok(())
}

/// newest file in `dir` matching `pattern`, modified within MAX_FILE_AGE.
async fn find_newest_log(dir: &Path, pattern: &Regex) -> Option<PathBuf> {
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !pattern.is_match(name) {
            continue;
        }
        let Ok(meta) = entry.metadata().await else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if mtime.elapsed().map(|age| age > MAX_FILE_AGE).unwrap_or(false) {
            continue;
        }
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, entry.path()));
        }
    }
    newest.map(|(_, p)| p)
}

struct Chunk {
    lines: Vec<String>,
    new_offset: u64,
    file_size: u64, // size at read time; < offset means the file was truncated
}

/// complete lines starting at `offset`; a partial trailing line is left for the
/// next poll (new_offset always lands just past a '\n').
async fn read_chunk(path: &Path, offset: u64) -> Result<Chunk> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata().await?.len();
    if size <= offset {
        return Ok(Chunk { lines: Vec::new(), new_offset: offset, file_size: size });
    }
    file.seek(SeekFrom::Start(offset)).await?;
    let want = (size - offset).min(MAX_CHUNK_BYTES) as usize;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break; // file shrank mid-read; use what we have
        }
        filled += n;
    }
    buf.truncate(filled);

    let mut lines = Vec::new();
    let mut consumed = 0usize;
    let mut start = 0usize;
    while lines.len() < MAX_CHUNK_LINES {
        let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let mut line = &buf[start..start + nl];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        lines.push(String::from_utf8_lossy(line).into_owned());
        consumed = start + nl + 1;
        start = consumed;
    }
    Ok(Chunk {
        lines,
        new_offset: offset + consumed as u64,
        file_size: size,
    })
}

fn fights_empty(v: &Value) -> bool {
    v.get("fights")
        .and_then(|f| f.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

struct Uploader<'a> {
    session: &'a wcl::WclSession,
    parser: &'a parser::Parser,
    code: &'a str,
    args: LiveLogArgs,
    segment_id: i64,
    last_master_ids: Option<(i64, i64, i64, i64)>,
    last_in_progress: Option<Instant>,
}

impl Uploader<'_> {
    /// mirror of Archon's uploadFilePart.
    async fn upload_part(
        &mut self,
        lines: &[String],
        push_fight: bool,
        cancel: &mut watch::Receiver<bool>,
        progress: &mut LiveProgress,
    ) -> Result<()> {
        if !lines.is_empty() {
            self.parser.parse_lines(lines, self.args.region).await?;
        }
        let mut fd = self.parser.collect_fights(push_fight).await?;
        let mut in_progress_count: i64 = 0;

        if fights_empty(&fd) {
            let ip = self.parser.collect_in_progress_fight().await?;
            let has_in_progress = !fights_empty(&ip);
            if progress.in_progress != has_in_progress {
                progress.in_progress = has_in_progress;
                if has_in_progress {
                    progress.emit("tailing", "Fight in progress");
                }
            }
            if !self.args.enable_real_time_uploading || !has_in_progress {
                return Ok(());
            }
            // each preview re-sends the whole fight so far, not a delta
            if self.last_in_progress.map(|t| t.elapsed() < IN_PROGRESS_INTERVAL).unwrap_or(false) {
                return Ok(());
            }
            self.last_in_progress = Some(Instant::now());
            in_progress_count = ip
                .get("fights")
                .and_then(|f| f.as_array())
                .and_then(|a| a.first())
                .and_then(|f| f.get("eventCount"))
                .and_then(|n| n.as_i64())
                .unwrap_or(0);
            fd = ip;
        } else {
            progress.in_progress = false;
        }

        let is_real_time = self.args.enable_real_time_uploading;
        let (session, code, segment_id) = (self.session, self.code, self.segment_id);
        let (email, password) = (self.args.email.clone(), self.args.password.clone());

        let mi = self.parser.collect_master_info().await?;
        let master_ids = wcl::master_ids(&mi);
        if Some(master_ids) != self.last_master_ids {
            let log_version = fd.get("logVersion").and_then(|v| v.as_i64()).unwrap_or(0);
            let game_version = fd.get("gameVersion").and_then(|v| v.as_i64()).unwrap_or(0);
            let master = wcl::build_master_string(&mi, log_version, game_version);
            let zipped = wcl::make_zip(&master)?;
            live_retry(session, &email, &password, cancel, progress, || {
                let z = zipped.clone();
                async move {
                    session
                        .set_master_table(code, segment_id, is_real_time, z)
                        .await
                        .map(|_| 0i64)
                }
            })
            .await?;
            self.last_master_ids = Some(master_ids);
        }

        let start_time = fd.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0);
        let end_time = fd.get("endTime").and_then(|v| v.as_i64()).unwrap_or(0);
        let mythic = fd.get("mythic").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let fights_str = wcl::build_fights_string(&fd);
        let zipped = wcl::make_zip(&fights_str)?;
        let next = live_retry(session, &email, &password, cancel, progress, || {
            let z = zipped.clone();
            async move {
                session
                    .add_segment(
                        code,
                        segment_id,
                        start_time,
                        end_time,
                        mythic,
                        true,
                        is_real_time,
                        in_progress_count,
                        z,
                    )
                    .await
            }
        })
        .await?;
        // previews overwrite in place, so only a commit advances - and it must always
        // advance, since the server refuses a second write to a committed id
        if in_progress_count == 0 {
            self.segment_id = next.max(segment_id + 1);
            progress.segments += 1;
            progress.emit("uploading", format!("Uploaded segment {}", progress.segments));
        }
        self.parser.clear_fights().await?;
        Ok(())
    }
}

/// Archon retries live uploads for up to an hour 
/// 401 means the session expired -> re-login and retry.
async fn live_retry<T, F, Fut>(
    session: &wcl::WclSession,
    email: &str,
    password: &str,
    cancel: &mut watch::Receiver<bool>,
    progress: &LiveProgress,
    f: F,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = None;
    for attempt in 1..=LIVE_RETRY_MAX {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if cancelled(cancel) {
                    return Err(e);
                }
                let unauthorized = e
                    .root_cause()
                    .downcast_ref::<wcl::HttpStatus>()
                    .map(|s| s.0 == 401)
                    .unwrap_or(false);
                if unauthorized {
                    match session.login(email, password).await {
                        Ok(_) => progress.emit(
                            "retrying",
                            format!("Session expired — logged in again (attempt {attempt})"),
                        ),
                        Err(login_err) => {
                            eprintln!("[live] re-login failed: {login_err:#}");
                            progress.emit(
                                "retrying",
                                format!("Session expired, re-login failed (attempt {attempt}): {login_err:#}"),
                            );
                        }
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                        _ = cancel.changed() => {}
                    }
                } else {
                    eprintln!("[live] upload failed (attempt {attempt}/{LIVE_RETRY_MAX}): {e:#}");
                    progress.emit("retrying", format!("Upload failed (attempt {attempt}): {e:#}"));
                    tokio::select! {
                        _ = tokio::time::sleep(LIVE_RETRY_DELAY) => {}
                        _ = cancel.changed() => {}
                    }
                }
                last_err = Some(e);
            }
        }
    }
    Err(match last_err {
        Some(e) => e.context(format!("upload failed, giving up after {LIVE_RETRY_MAX} attempts")),
        None => anyhow!("upload failed"),
    })
}
