//! Node sidecar driver. Spawns the bundled Node binary with `parser-harness.js`
//! and talks to it over stdin/stdout protocol defined in `parser-harness.js`. 
use std::path::Path;

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::sync::{mpsc, Mutex};

const READY_TIMEOUT_MS: u64 = 15_000;
const RESPONSE_TIMEOUT_MS: u64 = 60_000;

struct Inner {
    child: CommandChild,
    rx: mpsc::Receiver<String>,
}

pub struct Parser {
    inner: Mutex<Inner>,
}

impl Parser {
    /// `harness_path` is an absolute path to `parser-harness.js`.
    pub async fn spawn(
        app: &AppHandle,
        harness_path: &Path,
        gamedata_code: &str,
        parser_code: &str,
    ) -> Result<Self> {
        let harness = harness_path
            .to_str()
            .ok_or_else(|| anyhow!("non-utf8 harness path"))?;

        let (mut raw_rx, mut child) = app
            .shell()
            .sidecar("node")?
            .args([harness])
            .spawn()
            .context("failed to spawn Node sidecar")?;

        let (line_tx, line_rx) = mpsc::channel::<String>(64);
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(4096);
            while let Some(event) = raw_rx.recv().await {
                match event {
                    CommandEvent::Stdout(bytes) => {
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let mut line: Vec<u8> = buf.drain(..=pos).collect();
                            line.pop(); // trailing \n
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            let s = String::from_utf8_lossy(&line).to_string();
                            if line_tx.send(s).await.is_err() {
                                return;
                            }
                        }
                    }
                    CommandEvent::Stderr(bytes) => {
                        eprintln!("[node] {}", String::from_utf8_lossy(&bytes).trim_end());
                    }
                    CommandEvent::Error(e) => {
                        eprintln!("[node] error: {e}");
                    }
                    CommandEvent::Terminated(t) => {
                        eprintln!("[node] terminated: code={:?}", t.code);
                        return;
                    }
                    _ => {}
                }
            }
        });

        let bootstrap = serde_json::to_string(&json!({
            "gamedataCode": gamedata_code,
            "parserCode": parser_code,
        }))?;
        child
            .write(format!("{bootstrap}\n").as_bytes())
            .context("failed to write bootstrap to sidecar stdin")?;

        let inner = Inner {
            child,
            rx: line_rx,
        };
        let parser = Self {
            inner: Mutex::new(inner),
        };

        let ready = parser
            .recv_with_timeout(READY_TIMEOUT_MS)
            .await
            .context("parser did not emit ready response")?;
        if !ready.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
            bail!("parser bootstrap failed: {ready}");
        }
        Ok(parser)
    }

    async fn recv_with_timeout(&self, timeout_ms: u64) -> Result<Value> {
        let mut inner = self.inner.lock().await;
        let line = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            inner.rx.recv(),
        )
        .await
        .context("parser response timeout")?
        .context("parser stdout closed")?;
        Ok(serde_json::from_str(&line).with_context(|| format!("parser emitted non-JSON: {line}"))?)
    }

    async fn exchange(&self, payload: Value) -> Result<Value> {
        let line = serde_json::to_string(&payload)?;
        {
            let mut inner = self.inner.lock().await;
            inner
                .child
                .write(format!("{line}\n").as_bytes())
                .context("failed to write command to sidecar stdin")?;
        }
        self.recv_with_timeout(RESPONSE_TIMEOUT_MS).await
    }

    pub async fn clear_state(&self) -> Result<()> {
        let r = self.exchange(json!({"action": "clear-state"})).await?;
        check_ok(&r)
    }

    pub async fn set_start_date(&self, date: &str) -> Result<()> {
        let r = self
            .exchange(json!({"action": "set-start-date", "startDate": date}))
            .await?;
        check_ok(&r)
    }

    pub async fn parse_lines(&self, lines: &[String], region: i32) -> Result<()> {
        let r = self
            .exchange(json!({
                "action": "parse-lines",
                "lines": lines,
                "selectedRegion": region,
            }))
            .await?;
        check_ok(&r)
    }

    pub async fn collect_fights(&self) -> Result<Value> {
        let r = self
            .exchange(json!({
                "action": "collect-fights",
                "pushFightIfNeeded": true,
                "scanningOnly": false,
            }))
            .await?;
        check_ok(&r)?;
        Ok(r)
    }

    pub async fn collect_master_info(&self) -> Result<Value> {
        let r = self.exchange(json!({"action": "collect-master-info"})).await?;
        check_ok(&r)?;
        Ok(r)
    }

    pub async fn clear_fights(&self) -> Result<()> {
        let r = self.exchange(json!({"action": "clear-fights"})).await?;
        check_ok(&r)
    }

    pub async fn close(self) {
        let inner = self.inner.into_inner();
        let _ = inner.child.kill();
    }
}

fn check_ok(v: &Value) -> Result<()> {
    if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        Err(anyhow!(
            "parser error: {}",
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error")
        ))
    }
}

/// resolved from `tauri.conf.json` bundle.resources
pub fn harness_path(app: &AppHandle) -> Result<std::path::PathBuf> {
    let resource_dir = app
        .path()
        .resource_dir()
        .context("resource dir unavailable")?;
    Ok(resource_dir.join("resources").join("parser-harness.js"))
}
