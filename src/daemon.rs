use crate::browser::{self, LaunchPolicy};
use crate::cdp::CdpClient;
use crate::protocol::{
    Envelope, LaunchConfig, Phase, Request, Response, ScrollDirection, SnapOptions, WaitTarget,
};
use crate::snapshot::{
    build_snapshot_expr, diff_snapshots, parse_snapshot, render_interactive, render_snapshot,
};
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
const FOCUSED_VALUE_JS: &str = "(() => {
    let el = document.activeElement;
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    if (!el || el === document.body) return null;
    if (el.isContentEditable) return el.innerText;
    return typeof el.value === 'string' ? el.value : null;
})()";

// Receipt (2026-11-27, headless Chrome, React 18.3.1): click -> first DOM mutation 0.5-2.7ms sync,
// 51.7-53.7ms when the handler defers via setTimeout(0). 120ms covers the deferred path ~2x.
const POST_CLICK_SETTLE: Duration = Duration::from_millis(120);
// Receipt (same run): controlled-input re-render after Input.insertText 1.5ms.
const POST_FILL_SETTLE: Duration = Duration::from_millis(80);
// Tripwire for elements rendered after async work; anything slower belongs to `surf wait`.
const LABEL_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const LABEL_POLL_INTERVAL: Duration = Duration::from_millis(80);
const WAIT_TEXT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILL_ECHO_MAX_CHARS: usize = 200;

pub struct DaemonConfig {
    pub port: u16,
    pub headless: bool,
    pub attach_only: bool,
    pub home_dir: PathBuf,
}

impl DaemonConfig {
    pub fn socket_path(&self) -> PathBuf {
        self.home_dir.join("daemon.sock")
    }

    pub fn launch(&self) -> LaunchConfig {
        LaunchConfig {
            port: self.port,
            headless: self.headless,
            attach_only: self.attach_only,
        }
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
    attach_only: bool,
    profile_dir: PathBuf,
    active_target_id: Option<String>,
    cdp: Option<CdpClient>,
    last_snapshots: HashMap<String, String>,
    should_stop: bool,
    defer_snap: bool,
    step_label: Option<String>,
    phases: Vec<Phase>,
}

fn action_snap_opts(diff: bool) -> SnapOptions {
    if diff {
        SnapOptions {
            diff: true,
            max_text_chars: 0,
            ..Default::default()
        }
    } else {
        SnapOptions::default()
    }
}

impl SessionState {
    fn new(cfg: &DaemonConfig) -> Self {
        Self {
            port: cfg.port,
            headless: cfg.headless,
            attach_only: cfg.attach_only,
            profile_dir: cfg.profile_dir(),
            active_target_id: None,
            cdp: None,
            last_snapshots: HashMap::new(),
            should_stop: false,
            defer_snap: false,
            step_label: None,
            phases: Vec::new(),
        }
    }

    fn apply_launch(&mut self, launch: LaunchConfig) {
        if launch.port != self.port {
            self.port = launch.port;
            self.cdp = None;
            self.active_target_id = None;
            self.last_snapshots.clear();
        }
        self.headless = launch.headless;
        self.attach_only = launch.attach_only;
    }

    fn launch_policy(&self) -> LaunchPolicy<'_> {
        LaunchPolicy {
            headless: self.headless,
            attach_only: self.attach_only,
            profile_dir: &self.profile_dir,
        }
    }

    fn mark(&mut self, name: &str, since: Instant) {
        let name = match &self.step_label {
            Some(step) => format!("{step} {name}"),
            None => name.to_string(),
        };
        self.phases.push(Phase {
            name,
            us: since.elapsed().as_micros() as u64,
        });
    }

    async fn ensure_cdp(&mut self) -> Result<&CdpClient> {
        if self.cdp.as_ref().is_some_and(CdpClient::is_alive) {
            return Ok(self.cdp.as_ref().expect("checked alive"));
        }

        let start = Instant::now();
        self.cdp = None;
        browser::ensure_browser(self.port, &self.launch_policy()).await?;

        let mut targets = browser::list_page_targets(self.port).await?;
        if targets.is_empty() {
            let t = browser::open_new_tab(self.port, Some("about:blank")).await?;
            targets.push(t);
        }

        let target = self
            .active_target_id
            .as_ref()
            .and_then(|active_id| targets.iter().find(|t| &t.id == active_id))
            .unwrap_or(&targets[0])
            .clone();

        let ws_url = target
            .web_socket_debugger_url
            .clone()
            .ok_or_else(|| anyhow!("target {} has no webSocketDebuggerUrl", target.id))?;

        self.cdp = Some(CdpClient::connect(&ws_url).await?);
        self.active_target_id = Some(target.id);
        self.mark("cdp_discover", start);
        Ok(self.cdp.as_ref().expect("cdp initialized"))
    }

    async fn take_snap(&mut self, opts: &SnapOptions) -> Result<String> {
        if self.defer_snap {
            return Ok(String::new());
        }
        let expr = build_snapshot_expr(opts);
        let cdp = self.ensure_cdp().await?;
        let start = Instant::now();
        let val = cdp.eval_value(&expr).await?;
        let snap = parse_snapshot(val)?;
        let interactive = render_interactive(&snap);
        let key = self
            .active_target_id
            .clone()
            .unwrap_or_else(|| "default".into());

        let out = match self.last_snapshots.get(&key) {
            Some(prev) if opts.diff => diff_snapshots(prev, &interactive),
            _ => render_snapshot(&snap, opts),
        };

        self.last_snapshots.insert(key, interactive);
        self.mark("snapshot", start);
        Ok(out)
    }

    async fn settle(&mut self, dur: Duration) {
        let start = Instant::now();
        tokio::time::sleep(dur).await;
        self.mark("settle", start);
    }

    async fn click_at(&mut self, x: f64, y: f64, diff: bool) -> Result<String> {
        let start = Instant::now();
        self.ensure_cdp().await?.click_point(x, y).await?;
        self.mark("click", start);
        self.settle(POST_CLICK_SETTLE).await;
        self.take_snap(&action_snap_opts(diff)).await
    }

    async fn type_into_focused(&mut self, target: &str, text: &str, diff: bool) -> Result<String> {
        let start = Instant::now();
        self.ensure_cdp().await?.insert_text(text).await?;
        self.mark("input", start);
        self.settle(POST_FILL_SETTLE).await;
        let value = self
            .ensure_cdp()
            .await?
            .eval_value(FOCUSED_VALUE_JS)
            .await?;
        let mut out = fill_confirmation(target, text, &value);
        out.push_str(&self.take_snap(&action_snap_opts(diff)).await?);
        Ok(out)
    }

    async fn handle(&mut self, req: Request) -> Result<String> {
        match req {
            Request::Status => {
                let ver = browser::get_version(self.port).await.ok();
                let tabs = browser::list_page_targets(self.port)
                    .await
                    .unwrap_or_default();
                let not_running = if self.attach_only {
                    "not running (attach-only: will not launch)"
                } else {
                    "not running (will auto-launch)"
                };
                let browser_info = ver.map(|v| v.browser).unwrap_or_else(|| not_running.into());
                Ok(format!(
                    "daemon: running (pid {})\ncdp_port: {}\nattach_only: {}\nbrowser: {}\ntabs: {}\nactive_tab: {}\n",
                    std::process::id(),
                    self.port,
                    self.attach_only,
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
                let cdp = self.ensure_cdp().await?;
                let start = Instant::now();
                cdp.navigate(&normalized).await?;
                self.mark("navigate", start);
                self.take_snap(&snap).await
            }
            Request::Snap { opts } => self.take_snap(&opts).await,
            Request::Click { ref_id, diff } => {
                let cdp = self.ensure_cdp().await?;
                let start = Instant::now();
                let (x, y) = resolve_ref_point(cdp, ref_id).await?;
                self.mark("resolve", start);
                self.click_at(x, y, diff).await
            }
            Request::ClickLabel { label, diff } => {
                let cdp = self.ensure_cdp().await?;
                let start = Instant::now();
                let (x, y) = resolve_label_point(cdp, &label).await?;
                self.mark("resolve", start);
                self.click_at(x, y, diff).await
            }
            Request::Fill { ref_id, text, diff } => {
                let cdp = self.ensure_cdp().await?;
                let start = Instant::now();
                focus_and_clear_ref(cdp, ref_id).await?;
                self.mark("resolve", start);
                self.type_into_focused(&format!("@{ref_id}"), &text, diff)
                    .await
            }
            Request::FillLabel { label, text, diff } => {
                let cdp = self.ensure_cdp().await?;
                let start = Instant::now();
                focus_and_clear_label(cdp, &label).await?;
                self.mark("resolve", start);
                self.type_into_focused(&json!(label).to_string(), &text, diff)
                    .await
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
                    ScrollDirection::Down => {
                        format!("window.scrollBy(0, {amount}); window.scrollY")
                    }
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
                        tokio::time::sleep(WAIT_TEXT_POLL_INTERVAL).await;
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
                browser::ensure_browser(self.port, &self.launch_policy()).await?;
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
                browser::ensure_browser(self.port, &self.launch_policy()).await?;
                let normalized = url.as_deref().map(normalize_url);
                let target = browser::open_new_tab(self.port, normalized.as_deref()).await?;
                let url_ignored = target.url.is_empty() || target.url == "about:blank";
                self.active_target_id = Some(target.id);
                self.cdp = None;
                if let Some(url) = normalized.filter(|u| u != "about:blank") {
                    let cdp = self.ensure_cdp().await?;
                    let start = Instant::now();
                    if url_ignored {
                        cdp.navigate(&url).await?;
                    } else {
                        cdp.wait_ready(10_000).await?;
                    }
                    self.mark("navigate", start);
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
                    let is_last = i + 1 == total;
                    let cmd = request_name(&step);
                    self.defer_snap = !is_last && !matches!(step, Request::Snap { .. });
                    self.step_label = Some(format!("step{}/{total}", i + 1));
                    let start = Instant::now();
                    let res = Box::pin(self.handle(step)).await;
                    self.mark(&cmd, start);
                    self.defer_snap = false;
                    self.step_label = None;
                    match res {
                        Ok(out) => last_out = out,
                        Err(e) => bail!("batch step {}/{total} ({cmd}) failed: {e}", i + 1),
                    }
                }
                Ok(last_out)
            }
        }
    }
}

fn request_name(req: &Request) -> String {
    serde_json::to_value(req)
        .ok()
        .and_then(|v| v.get("cmd").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "step".into())
}

fn quoted_prefix(s: &str) -> String {
    let shown: String = s.chars().take(FILL_ECHO_MAX_CHARS).collect();
    json!(shown).to_string()
}

fn fill_confirmation(target: &str, requested: &str, value: &Value) -> String {
    let Some(actual) = value.as_str() else {
        return format!("filled {target} (focused element value not readable)\n");
    };
    let total = actual.chars().count();
    let mut out = format!("filled {target} val={}", quoted_prefix(actual));
    if total > FILL_ECHO_MAX_CHARS {
        out.push_str(&format!(
            " [truncated: showing {FILL_ECHO_MAX_CHARS}/{total} chars; use `surf eval` for the full value]"
        ));
    }
    out.push('\n');
    if actual.trim_end() != requested.trim_end() {
        out.push_str(&format!(
            "warning: field value differs from requested text (requested={})\n",
            quoted_prefix(requested)
        ));
    }
    out
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
    let deadline = tokio::time::Instant::now() + LABEL_WAIT_TIMEOUT;

    loop {
        let val = cdp.eval_value(&expr).await?;
        if val.get("found").and_then(|v| v.as_bool()) == Some(true) {
            let x = val.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = val.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            return Ok((x, y));
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "click-label_timeout={}ms: no visible element matching \"{label}\" (run `surf snap` to inspect)",
                LABEL_WAIT_TIMEOUT.as_millis()
            );
        }
        tokio::time::sleep(LABEL_POLL_INTERVAL).await;
    }
}

async fn focus_and_clear_label(cdp: &CdpClient, label: &str) -> Result<()> {
    let args = json!({ "query": label, "mode": "fill" });
    let expr = format!("({RESOLVE_LABEL_JS})({args})");
    let deadline = tokio::time::Instant::now() + LABEL_WAIT_TIMEOUT;

    let val = loop {
        let val = cdp.eval_value(&expr).await?;
        if val.get("found").and_then(|v| v.as_bool()) == Some(true) {
            break val;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "fill-label_timeout={}ms: no visible editable element matching \"{label}\" (run `surf snap` to inspect)",
                LABEL_WAIT_TIMEOUT.as_millis()
            );
        }
        tokio::time::sleep(LABEL_POLL_INTERVAL).await;
    };

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
        let resp = match serde_json::from_str::<Envelope>(&line) {
            Ok(Envelope { req, launch }) => {
                let mut guard = state.lock().await;
                if let Some(launch) = launch {
                    guard.apply_launch(launch);
                }
                let res = guard.handle(req).await;
                let elapsed_ms = start.elapsed().as_millis() as u64;
                let mut resp = match res {
                    Ok(out) => Response::ok(out, elapsed_ms),
                    Err(e) => Response::err(e.to_string(), elapsed_ms),
                };
                resp.phases = std::mem::take(&mut guard.phases);
                resp
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_accepts_requests_with_and_without_launch() {
        let env: Envelope = serde_json::from_str(
            r#"{"cmd":"status","launch":{"port":9333,"headless":true,"attach_only":true}}"#,
        )
        .unwrap();
        assert!(matches!(env.req, Request::Status));
        assert_eq!(env.launch.unwrap().port, 9333);

        let env: Envelope = serde_json::from_str(r#"{"cmd":"click","ref_id":3}"#).unwrap();
        assert!(matches!(env.req, Request::Click { ref_id: 3, .. }));
        assert!(env.launch.is_none());
    }

    #[test]
    fn launch_config_from_request_overrides_daemon_start_flags() {
        let cfg = DaemonConfig {
            port: 9222,
            headless: false,
            attach_only: false,
            home_dir: PathBuf::from("/tmp/surf"),
        };
        let mut state = SessionState::new(&cfg);
        state.active_target_id = Some("t1".into());
        state.last_snapshots.insert("t1".into(), "snap".into());

        state.apply_launch(LaunchConfig {
            port: 9222,
            headless: true,
            attach_only: true,
        });
        assert!(state.attach_only && state.headless);
        assert_eq!(state.active_target_id.as_deref(), Some("t1"));

        state.apply_launch(LaunchConfig {
            port: 9333,
            headless: true,
            attach_only: true,
        });
        assert_eq!(state.port, 9333);
        assert!(state.active_target_id.is_none() && state.last_snapshots.is_empty());
    }

    #[test]
    fn fill_confirmation_echoes_dom_value() {
        let out = fill_confirmation("\"Post text\"", "hi", &json!("hi"));
        assert_eq!(out, "filled \"Post text\" val=\"hi\"\n");
    }

    #[test]
    fn fill_confirmation_warns_on_mismatch_and_truncates() {
        let long = "x".repeat(FILL_ECHO_MAX_CHARS + 5);
        let out = fill_confirmation("@3", "y", &json!(long));
        assert!(out.contains(&format!(
            "showing {FILL_ECHO_MAX_CHARS}/{} chars",
            FILL_ECHO_MAX_CHARS + 5
        )));
        assert!(out.contains("warning: field value differs from requested text (requested=\"y\")"));
    }

    #[test]
    fn fill_confirmation_handles_unreadable_value() {
        let out = fill_confirmation("@1", "a", &Value::Null);
        assert!(out.contains("not readable"));
    }

    #[test]
    fn action_diffs_skip_page_text() {
        assert_eq!(action_snap_opts(true).max_text_chars, 0);
        assert!(action_snap_opts(false).max_text_chars > 0);
    }
}
