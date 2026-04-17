#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod parser;
mod wcl;

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

const DEFAULT_HOTKEY: &str = "Shift+Tab";

struct OverlayState {
    current_hotkey: Mutex<Option<Shortcut>>,
}

const BATCH_SIZE: usize = 100_000;
const UPLOAD_UI_RESERVED_PCT: u32 = 10; // the first 10% are reserved for client-side read

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct UploadArgs {
    log_path: String,
    email: String,
    password: String,
    region: i32,
    visibility: i32,
    guild_id: Option<i64>,
}

#[derive(Serialize)]
struct VersionInfo {
    app: &'static str,
}

#[derive(Serialize)]
struct FileInfo {
    path: String,
    name: String,
    size: u64,
}

#[tauri::command]
fn app_version() -> VersionInfo {
    VersionInfo {
        app: env!("CARGO_PKG_VERSION"),
    }
}

/// native file picker.
#[tauri::command]
async fn pick_log_file(app: AppHandle) -> Option<FileInfo> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Combat log", &["txt"])
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let path = rx.await.ok().flatten()?;
    let pb = path.as_path()?.to_path_buf();
    Some(describe_file(&pb))
}

/// file info for a user-dropped path 
#[tauri::command]
fn file_info(path: String) -> Result<FileInfo, String> {
    let pb = std::path::PathBuf::from(&path);
    if !pb.is_file() {
        return Err(format!("not a file: {path}"));
    }
    Ok(describe_file(&pb))
}

/// external URL handler
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|e| format!("failed to open URL: {e}"))
}

fn describe_file(path: &std::path::Path) -> FileInfo {
    let name = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("")
        .to_string();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    FileInfo {
        path: path.to_string_lossy().to_string(),
        name,
        size,
    }
}

#[tauri::command]
fn set_hotkey(
    app: AppHandle,
    accelerator: String,
    state: tauri::State<OverlayState>,
) -> Result<String, String> {
    let new_shortcut: Shortcut = accelerator
        .parse()
        .map_err(|e| format!("invalid hotkey: {e}"))?;
    let gs = app.global_shortcut();
    let mut cur = state.current_hotkey.lock().unwrap();
    if cur.as_ref() == Some(&new_shortcut) {
        return Ok(accelerator);
    }
    gs.register(new_shortcut.clone()).map_err(|e| e.to_string())?;
    if let Some(prev) = cur.take() {
        let _ = gs.unregister(prev);
    }
    *cur = Some(new_shortcut);
    Ok(accelerator)
}

#[tauri::command]
fn get_hotkey(state: tauri::State<OverlayState>) -> Option<String> {
    state
        .current_hotkey
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.to_string())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// start upload
#[tauri::command]
async fn start_upload(app: AppHandle, args: UploadArgs) -> Result<(), String> {
    tokio::spawn(async move {
        if let Err(e) = run_upload(&app, args).await {
            let _ = app.emit(
                "upload:error",
                json!({"message": format!("{e:#}")}),
            );
        }
    });
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .manage(OverlayState {
            current_hotkey: Mutex::new(None),
        })
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let state = app.state::<OverlayState>();
                    let is_current = state
                        .current_hotkey
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| s == shortcut)
                        .unwrap_or(false);
                    if !is_current {
                        return;
                    }
                    if let Some(win) = app.get_webview_window("main") {
                        let visible = win.is_visible().unwrap_or(false);
                        if visible {
                            let _ = win.hide();
                        } else {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();
            match DEFAULT_HOTKEY.parse::<Shortcut>() {
                Ok(shortcut) => match handle.global_shortcut().register(shortcut.clone()) {
                    Ok(_) => {
                        handle
                            .state::<OverlayState>()
                            .current_hotkey
                            .lock()
                            .unwrap()
                            .replace(shortcut);
                    }
                    Err(e) => eprintln!("failed to register default hotkey {DEFAULT_HOTKEY}: {e}"),
                },
                Err(e) => eprintln!("invalid default hotkey {DEFAULT_HOTKEY}: {e}"),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_version,
            pick_log_file,
            file_info,
            open_url,
            start_upload,
            set_hotkey,
            get_hotkey,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn emit_progress(app: &AppHandle, step: &str, message: impl Into<String>, pct: u32) {
    let _ = app.emit(
        "upload:progress",
        json!({
            "step": step,
            "message": message.into(),
            "pct": pct,
        }),
    );
}

/// this is (hopefully) a mirror of `upload_worker` in `web/webapp.py`.
/// (TODO: I should really deduplicate this)
async fn run_upload(app: &AppHandle, args: UploadArgs) -> Result<()> {
    let log_path = PathBuf::from(&args.log_path);
    let filename = log_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("log.txt")
        .to_string();

    emit_progress(app, "read", "Reading log file...", 1);
    let raw = tokio::fs::read_to_string(&log_path)
        .await
        .with_context(|| format!("reading {}", log_path.display()))?;
    let all_lines: Vec<String> = raw
        .lines()
        .map(|s| s.to_string())
        .collect();
    let total = all_lines.len();
    emit_progress(
        app,
        "read",
        format!("Read {} lines", format_with_commas(total)),
        2,
    );

    emit_progress(app, "session", "Initializing session...", 3);
    let session = wcl::WclSession::new().await?;

    emit_progress(app, "login", "Logging in...", 4);
    let login = session.login(&args.email, &args.password).await?;
    let user_name = login
        .user
        .as_ref()
        .and_then(|u| u.user_name.as_deref())
        .unwrap_or("?")
        .to_string();
    emit_progress(app, "login", format!("Logged in as {user_name}"), 5);

    emit_progress(app, "fetch-parser", "Fetching latest parser...", 6);
    let bundle = session.fetch_parser_code().await?;
    let parser_version = bundle.parser_version;
    emit_progress(
        app,
        "fetch-parser",
        format!("Parser v{parser_version} loaded"),
        7,
    );

    let harness = parser::harness_path(app)?;
    emit_progress(app, "parser", "Starting parser...", 8);
    let parser = parser::Parser::spawn(app, &harness, &bundle.gamedata_code, &bundle.parser_code)
        .await?;
    parser.clear_state().await?;
    if let Some(date) = wcl::parse_start_date(&filename) {
        parser.set_start_date(&date).await?;
    }
    emit_progress(app, "parser", "Parser ready", 9);

    let mut segment_id: i64 = 1;
    let mut report_code: Option<String> = None;
    let mut last_master_ids: Option<(i64, i64, i64, i64)> = None;
    let total_batches = (total + BATCH_SIZE - 1) / BATCH_SIZE;

    for (batch_idx, chunk) in all_lines.chunks(BATCH_SIZE).enumerate() {
        let batch_num = batch_idx + 1;
        let pct = UPLOAD_UI_RESERVED_PCT
            + (80 * batch_num as u32 / total_batches.max(1) as u32);

        parser.parse_lines(&chunk.to_vec(), args.region).await?;
        let fd = parser.collect_fights().await?;
        let fights = fd.get("fights").and_then(|v| v.as_array());
        if fights.map(|a| a.is_empty()).unwrap_or(true) {
            emit_progress(
                app,
                "parse",
                format!("Batch {batch_num}/{total_batches} — no fights yet"),
                pct,
            );
            continue;
        }

        if report_code.is_none() {
            let start_time = fd.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0);
            let end_time = fd.get("endTime").and_then(|v| v.as_i64()).unwrap_or(0);
            let code = session
                .create_report(
                    &filename,
                    start_time,
                    end_time,
                    args.region,
                    args.visibility,
                    args.guild_id,
                    parser_version,
                )
                .await?;
            emit_progress(
                app,
                "report",
                format!("Report created: {code}"),
                pct,
            );
            report_code = Some(code);
        }
        let code = report_code.as_deref().unwrap();

        let mi = parser.collect_master_info().await?;
        let master_ids = (
            mi.get("lastAssignedActorID").and_then(|v| v.as_i64()).unwrap_or(0),
            mi.get("lastAssignedAbilityID").and_then(|v| v.as_i64()).unwrap_or(0),
            mi.get("lastAssignedTupleID").and_then(|v| v.as_i64()).unwrap_or(0),
            mi.get("lastAssignedPetID").and_then(|v| v.as_i64()).unwrap_or(0),
        );
        if Some(master_ids) != last_master_ids {
            let log_version = fd.get("logVersion").and_then(|v| v.as_i64()).unwrap_or(0);
            let game_version = fd.get("gameVersion").and_then(|v| v.as_i64()).unwrap_or(0);
            let master = wcl::build_master_string(&mi, log_version, game_version);
            let zipped = wcl::make_zip(&master)?;
            session.set_master_table(code, segment_id, zipped).await?;
            last_master_ids = Some(master_ids);
        }

        let evts: i64 = fights
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.get("eventCount").and_then(|n| n.as_i64()))
                    .sum()
            })
            .unwrap_or(0);
        let start_time = fd.get("startTime").and_then(|v| v.as_i64()).unwrap_or(0);
        let end_time = fd.get("endTime").and_then(|v| v.as_i64()).unwrap_or(0);
        let mythic = fd.get("mythic").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        let fights_str = wcl::build_fights_string(&fd);
        let zipped = wcl::make_zip(&fights_str)?;
        segment_id = session
            .add_segment(code, segment_id, start_time, end_time, mythic, zipped)
            .await?;
        parser.clear_fights().await?;
        emit_progress(
            app,
            "upload",
            format!(
                "Segment {batch_num}/{total_batches} — {} events",
                format_with_commas(evts as usize)
            ),
            pct,
        );
    }

    parser.close().await;

    match report_code {
        Some(code) => {
            session.terminate_report(&code).await?;
            let url = format!("https://www.warcraftlogs.com/reports/{code}");
            let _ = app.emit("upload:done", json!({"url": url, "code": code}));
            Ok(())
        }
        None => Err(anyhow!("No fights found in log file.")),
    }
}

fn format_with_commas(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}
