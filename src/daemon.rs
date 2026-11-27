use crate::browser;
use crate::cdp::CdpClient;
use crate::protocol::{Request, Response, ScrollDirection, SnapOptions, WaitTarget};
use crate::snapshot::{build_snapshot_expr, diff_snapshots, format_snapshot};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

const RESOLVE_LABEL_JS: &str = include_str!("js/resolve_label.js");

pub struct DaemonConfig {
    pub port: u16,
    pub headless: bool,
    pub home_dir: PathBuf,
}

impl DaemonConfig {
    pub fn socket_path(&self) -> PathBuf {
        self.home_dir.join("daemon.sock")
    }

    pub fn pid_path(&self) -> PathBuf {
        self.home_dir.join("daemon.pid")
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.home_dir.join("profile")
    }
}

struct SessionState {
    port: u16,
    headless: bool,
    profile_dir: PathBuf,
    active_target_id: Option<String>,
    cdp: Option<CdpClient>,
    last_snapshots: HashMap<String, String>,
    should_stop: bool,
}

impl SessionState {
    fn new(cfg: &DaemonConfig) -> Self {
        Self {
            port: cfg.port,
            headless: cfg.headless,
            profile_dir: cfg.profile_dir(),
            active_target_id: None,
            cdp: None,
            last_snapshots: HashMap::new(),
            should_stop: false,
        }
    }

    async fn ensure_cdp(&mut self) -> Result<&CdpClient> {
        browser::ensure_browser(self.port, self.headless, &self.profile_dir).await?;

        let mut targets = browser::list_page_targets(self.port).await?;
        if targets.is_empty() {
            let t = browser::open_new_tab(self.port, Some("about:blank")).await?;
            targets.push(t);
        }

        let target = if let Some(ref active_id) = self.active_target_id {
            targets
                .iter()
                .find(|t| &t.id == active_id)
                .cloned()
                .unwrap_or_else(|| targets[0].clone())
        } else {
            targets[0].clone()
        };

        let ws_url = target
            .web_socket_debugger_url
            .clone()
            .ok_or_else(|| anyhow!("target {} has no webSocketDebuggerUrl", target.id))?;

        let needs_reconnect = match &self.cdp {
            Some(client) => client.ws_url() != ws_url,
            None => true,
        };

        if needs_reconnect {
            let client = CdpClient::connect(&ws_url).await?;
            self.cdp = Some(client);
            self.active_target_id = Some(target.id);
        }

        Ok(self.cdp.as_ref().expect("cdp initialized"))
    }

    async fn take_snap(&mut self, opts: &SnapOptions) -> Result<String> {
        let expr = build_snapshot_expr(opts);
        let val = {
            let cdp = self.ensure_cdp().await?;
            cdp.eval_value(&expr).await?
        };
        let rendered = format_snapshot(val, opts)?;
        let key = self
            .active_target_id
            .clone()
            .unwrap_or_else(|| "default".into());

        let out = if opts.diff {
            match self.last_snapshots.get(&key) {
                Some(prev) => diff_snapshots(prev, &rendered),
                None => rendered.clone(),
            }
        } else {
            rendered.clone()
        };

        self.last_snapshots.insert(key, rendered);
        Ok(out)
    }

    async fn handle(&mut self, req: Request) -> Result<String> {
        match req {
            Request::Status => {
                let ver = browser::get_version(self.port).await.ok();
                let tabs = browser::list_page_targets(self.port)
                    .await
                    .unwrap_or_default();
                let browser_info = ver
                    .map(|v| v.browser)
                    .unwrap_or_else(|| "not running (will auto-launch)".into());
                Ok(format!(
                    "daemon: running (pid {})\ncdp_port: {}\nbrowser: {}\ntabs: {}\nactive_tab: {}\n",
                    std::process::id(),
                    self.port,
                    browser_info,
                    tabs.len(),
                    self.active_target_id.as_deref().unwrap_or("none")
                ))
            }
            Request::Stop => {
                self.should_stop = true;
                Ok("daemon stopping\n".into())
            }
            Request::Open { url, snap } => {
                let normalized = normalize_url(&url);
                {
                    let cdp = self.ensure_cdp().await?;
                    cdp.navigate(&normalized).await?;
                }
                self.take_snap(&snap).await
            }
            Request::Snap { opts } => self.take_snap(&opts).await,
            Request::Click { ref_id, diff } => {
                let (x, y) = {
                    let cdp = self.ensure_cdp().await?;
                    resolve_ref_point(cdp, ref_id).await?
                };
                {
                    let cdp = self.ensure_cdp().await?;
                    cdp.click_point(x, y).await?;
                }
                tokio::time::sleep(Duration::from_millis(120)).await;
                let opts = SnapOptions {
                    diff,
                    ..Default::default()
                };
                self.take_snap(&opts).await
            }
            Request::ClickLabel { label, diff } => {
                let (x, y) = {
                    let cdp = self.ensure_cdp().await?;
                    resolve_label_point(cdp, &label).await?
                };
                {
                    let cdp = self.ensure_cdp().await?;
                    cdp.click_point(x, y).await?;
                }
                tokio::time::sleep(Duration::from_millis(120)).await;
                let opts = SnapOptions {
                    diff,
                    ..Default::default()
                };
                self.take_snap(&opts).await
            }
            Request::Fill { ref_id, text, diff } => {
                {
                    let cdp = self.ensure_cdp().await?;
                    focus_and_clear_ref(cdp, ref_id).await?;
                    cdp.insert_text(&text).await?;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
                let opts = SnapOptions {
                    diff,
                    ..Default::default()
                };
                self.take_snap(&opts).await
            }
            Request::FillLabel { label, text, diff } => {
                {
                    let cdp = self.ensure_cdp().await?;
                    focus_and_clear_label(cdp, &label).await?;
                    cdp.insert_text(&text).await?;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
                let opts = SnapOptions {
                    diff,
                    ..Default::default()
                };
                self.take_snap(&opts).await
            }
            Request::Type { text } => {
                let cdp = self.ensure_cdp().await?;
                cdp.insert_text(&text).await?;
                Ok(format!("typed {} chars\n", text.chars().count()))
            }
            Request::Key { key } => {
                let cdp = self.ensure_cdp().await?;
                cdp.dispatch_key(&key).await?;
                Ok(format!("key {key}\n"))
            }
            Request::Scroll { direction, amount } => {
                let expr = match direction {
                    ScrollDirection::Down => format!("window.scrollBy(0, {amount}); window.scrollY"),
                    ScrollDirection::Up => format!("window.scrollBy(0, -{amount}); window.scrollY"),
                    ScrollDirection::Top => "window.scrollTo(0, 0); window.scrollY".into(),
                    ScrollDirection::Bottom => {
                        "window.scrollTo(0, document.body.scrollHeight); window.scrollY".into()
                    }
                };
                let cdp = self.ensure_cdp().await?;
                let y = cdp.eval_value(&expr).await?;
                Ok(format!("scrollY: {y}\n"))
            }
            Request::Wait { target } => match target {
                WaitTarget::Ms { ms } => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(format!("waited {ms}ms\n"))
                }
                WaitTarget::Idle { timeout_ms } => {
                    let cdp = self.ensure_cdp().await?;
                    let expr = format!(
                        "new Promise((resolve) => {{
                            let timer = null;
                            const done = (r) => {{ obs.disconnect(); clearTimeout(maxT); clearTimeout(timer); resolve(r); }};
                            const bump = () => {{ clearTimeout(timer); timer = setTimeout(() => done('idle'), 250); }};
                            const obs = new MutationObserver(bump);
                            if (document.body) obs.observe(document.body, {{ childList: true, subtree: true, attributes: true }});
                            const maxT = setTimeout(() => done('timeout'), {timeout_ms});
                            bump();
                        }})"
                    );
                    let res = cdp.eval_value(&expr).await?;
                    Ok(format!("wait: {}\n", res.as_str().unwrap_or("done")))
                }
                WaitTarget::Text { text, timeout_ms } => {
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
                    let needle = json!(text);
                    let expr = format!(
                        "(document.body && document.body.innerText || '').includes({needle})"
                    );
                    while tokio::time::Instant::now() < deadline {
                        let cdp = self.ensure_cdp().await?;
                        if cdp.eval_value(&expr).await? == Value::Bool(true) {
                            return Ok(format!("found text: {text}\n"));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    bail!("wait_text_timeout={timeout_ms}ms exceeded waiting for \"{text}\"")
                }
            },
            Request::Eval { expr } => {
                let cdp = self.ensure_cdp().await?;
                let val = cdp.eval_value(&expr).await?;
                match val {
                    Value::String(s) => Ok(format!("{s}\n")),
                    other => Ok(format!("{}\n", serde_json::to_string_pretty(&other)?)),
                }
            }
            Request::Screenshot { path } => {
                let cdp = self.ensure_cdp().await?;
                let b64 = cdp.screenshot_png_base64().await?;
                let bytes = decode_base64(&b64)?;
                let out_path = path.unwrap_or_else(|| "/tmp/surf-screenshot.png".into());
                std::fs::write(&out_path, bytes)
                    .with_context(|| format!("failed to write screenshot to {out_path}"))?;
                Ok(format!("saved {out_path}\n"))
            }
            Request::Tabs => {
                browser::ensure_browser(self.port, self.headless, &self.profile_dir).await?;
                let tabs = browser::list_page_targets(self.port).await?;
                let mut out = String::new();
                for (i, t) in tabs.iter().enumerate() {
                    let marker = if self.active_target_id.as_deref() == Some(&t.id) {
                        "*"
                    } else {
                        " "
                    };
                    out.push_str(&format!("{marker}[{i}] {} — {}\n", t.title, t.url));
                }
                Ok(out)
            }
            Request::TabSwitch { index } => {
                let tabs = browser::list_page_targets(self.port).await?;
                let target = tabs.get(index).ok_or_else(|| {
                    anyhow!("tab_index={index} out of range (open tabs: {})", tabs.len())
                })?;
                browser::activate_tab(self.port, &target.id).await?;
                self.active_target_id = Some(target.id.clone());
                self.cdp = None;
                self.take_snap(&SnapOptions::default()).await
            }
            Request::TabNew { url } => {
                browser::ensure_browser(self.port, self.headless, &self.profile_dir).await?;
                let normalized = url.as_deref().map(normalize_url);
                let target = browser::open_new_tab(self.port, normalized.as_deref()).await?;
                self.active_target_id = Some(target.id);
                self.cdp = None;
                if normalized.is_some() {
                    let cdp = self.ensure_cdp().await?;
                    cdp.wait_ready(10_000).await?;
                }
                self.take_snap(&SnapOptions::default()).await
            }
            Request::TabClose { index } => {
                let tabs = browser::list_page_targets(self.port).await?;
                let target = match index {
                    Some(i) => tabs.get(i).ok_or_else(|| {
                        anyhow!("tab_index={i} out of range (open tabs: {})", tabs.len())
                    })?,
                    None => tabs
                        .iter()
                        .find(|t| self.active_target_id.as_deref() == Some(&t.id))
                        .or_else(|| tabs.first())
                        .ok_or_else(|| anyhow!("no open tabs"))?,
                };
                browser::close_tab(self.port, &target.id).await?;
                if self.active_target_id.as_deref() == Some(&target.id) {
                    self.active_target_id = None;
                    self.cdp = None;
                }
                Ok(format!("closed tab {}\n", target.title))
            }
            Request::Batch { steps } => {
                let total = steps.len();
                let mut last_out = String::new();
                for (i, step) in steps.into_iter().enumerate() {
                    match Box::pin(self.handle(step)).await {
                        Ok(out) => last_out = out,
                        Err(e) => bail!("batch step {}/{total} failed: {e}", i + 1),
                    }
                }
                Ok(last_out)
            }
        }
    }
}

async fn resolve_ref_point(cdp: &CdpClient, ref_id: u32) -> Result<(f64, f64)> {
    let expr = format!(
        "(() => {{
            const el = window.__surfRefs && window.__surfRefs.get({ref_id});
            if (!el || !el.isConnected) return {{ found: false }};
            el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }});
            const r = el.getBoundingClientRect();
            return {{ found: true, x: r.left + r.width / 2, y: r.top + r.height / 2 }};
        }})()"
    );
    let val = cdp.eval_value(&expr).await?;
    if val.get("found").and_then(|v| v.as_bool()) != Some(true) {
        bail!("ref @{ref_id} not found or detached; run `surf snap` to refresh refs");
    }
    let x = val.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = val.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    Ok((x, y))
}

async fn focus_and_clear_ref(cdp: &CdpClient, ref_id: u32) -> Result<()> {
    let expr = format!(
        "(() => {{
            const el = window.__surfRefs && window.__surfRefs.get({ref_id});
            if (!el || !el.isConnected) return {{ found: false }};
            el.scrollIntoView({{ block: 'center', inline: 'center', behavior: 'instant' }});
            el.focus();
            if ('select' in el && typeof el.select === 'function') {{
                el.select();
            }} else if (el.isContentEditable) {{
                const range = document.createRange();
                range.selectNodeContents(el);
                const sel = window.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
            }}
            const hasContent = Boolean((el.value && el.value.length) || (el.isContentEditable && el.innerText.length));
            return {{ found: true, hasContent }};
        }})()"
    );
    let val = cdp.eval_value(&expr).await?;
    if val.get("found").and_then(|v| v.as_bool()) != Some(true) {
        bail!("ref @{ref_id} not found or detached; run `surf snap` to refresh refs");
    }
    if val.get("hasContent").and_then(|v| v.as_bool()) == Some(true) {
        cdp.dispatch_key("Backspace").await?;
    }
    Ok(())
}

async fn resolve_label_point(cdp: &CdpClient, label: &str) -> Result<(f64, f64)> {
    let args = json!({ "query": label, "mode": "click" });
    let expr = format!("({RESOLVE_LABEL_JS})({args})");
    let val = cdp.eval_value(&expr).await?;
    if val.get("found").and_then(|v| v.as_bool()) != Some(true) {
        bail!("click-label: no visible element matching \"{label}\"");
    }
    let x = val.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = val.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    Ok((x, y))
}

async fn focus_and_clear_label(cdp: &CdpClient, label: &str) -> Result<()> {
    let args = json!({ "query": label, "mode": "fill" });
    let expr = format!("({RESOLVE_LABEL_JS})({args})");
    let val = cdp.eval_value(&expr).await?;
    if val.get("found").and_then(|v| v.as_bool()) != Some(true) {
        bail!("fill-label: no visible editable element matching \"{label}\"");
    }

    let is_content_editable = val
        .get("isContentEditable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_content = val
        .get("hasContent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_content_editable {
        let x = val.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = val.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        cdp.click_point(x, y).await?;
        if has_content {
            let select_expr = "(() => {
                const el = window.__surfActiveEditable;
                if (!el || !el.isConnected) return false;
                const range = document.createRange();
                range.selectNodeContents(el);
                const sel = window.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
                return true;
            })()";
            let _ = cdp.eval_value(select_expr).await?;
            cdp.dispatch_key("Backspace").await?;
        }
    } else if has_content {
        cdp.dispatch_key("Backspace").await?;
    }
    Ok(())
}

fn normalize_url(url: &str) -> String {
    if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => bail!("invalid base64 byte {b}"),
        };
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

pub async fn run_daemon(cfg: DaemonConfig) -> Result<()> {
    std::fs::create_dir_all(&cfg.home_dir)?;
    let sock_path = cfg.socket_path();
    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind Unix socket {}", sock_path.display()))?;
    std::fs::write(cfg.pid_path(), std::process::id().to_string())?;

    let state = Arc::new(Mutex::new(SessionState::new(&cfg)));

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                let (stream, _) = accept_res?;
                let state = Arc::clone(&state);
                let stop = handle_conn(stream, state).await;
                if stop {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(cfg.pid_path());
    Ok(())
}

async fn handle_conn(stream: UnixStream, state: Arc<Mutex<SessionState>>) -> bool {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let start = Instant::now();
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let mut guard = state.lock().await;
                match guard.handle(req).await {
                    Ok(out) => Response::ok(out, start.elapsed().as_millis() as u64),
                    Err(e) => Response::err(e.to_string(), start.elapsed().as_millis() as u64),
                }
            }
            Err(e) => Response::err(
                format!("invalid request JSON: {e}"),
                start.elapsed().as_millis() as u64,
            ),
        };

        let mut payload = serde_json::to_string(&resp).unwrap_or_else(|_| {
            r#"{"ok":false,"error":"failed to serialize response"}"#.to_string()
        });
        payload.push('\n');
        if writer.write_all(payload.as_bytes()).await.is_err() {
            break;
        }

        if state.lock().await.should_stop {
            return true;
        }
    }
    false
}
