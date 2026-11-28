use crate::browser::{self, LaunchPolicy};
use crate::cdp::CdpClient;
use crate::protocol::{Phase, Request, ScrollDirection, SnapOptions, WaitTarget};
use crate::snapshot::{
    build_snapshot_expr, diff_snapshots, parse_snapshot, render_interactive, render_snapshot,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
// Receipt (2026-11-28, headless Chrome, 18 loads of reddit/github/youtube/stackoverflow/vercel/airbnb
// right after `open`): gaps between mutation bursts while loading reached 298ms; the next cluster
// (lazy loads, tickers) started at 776ms. Static pages mutate 0 times and pay the full window.
const IDLE_QUIET_WINDOW: Duration = Duration::from_millis(350);
const WAIT_TEXT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const FILL_ECHO_MAX_CHARS: usize = 200;

pub const DEFAULT_SESSION: &str = "default";
const MAX_SESSION_NAME_CHARS: usize = 64;

pub struct Config {
    pub port: u16,
    pub headless: bool,
    pub attach_only: bool,
    pub session: String,
    pub home_dir: PathBuf,
}

impl Config {
    fn sessions_dir(&self) -> PathBuf {
        self.home_dir.join("sessions")
    }

    fn state_path(&self) -> PathBuf {
        self.sessions_dir().join(format!("{}.json", self.session))
    }

    fn lock_path(&self) -> PathBuf {
        self.sessions_dir().join(format!("{}.lock", self.session))
    }

    fn profile_dir(&self) -> PathBuf {
        self.home_dir.join("profile")
    }
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    port: u16,
    active_target_id: Option<String>,
    last_snapshots: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct LaunchedBrowser {
    port: u16,
    pid: u32,
}

fn validate_session_name(name: &str) -> Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if name.is_empty() || name.len() > MAX_SESSION_NAME_CHARS || !valid_chars {
        bail!(
            "invalid session name {name:?}: use 1-{MAX_SESSION_NAME_CHARS} chars of [A-Za-z0-9_-]"
        );
    }
    Ok(())
}

fn lock_file(path: &std::path::Path) -> Result<std::fs::File> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(file)
}

pub struct Outcome {
    pub result: Result<String>,
    pub elapsed_ms: u64,
    pub phases: Vec<Phase>,
}

/// Runs one request in this process. A per-session file lock serializes commands of one session.
pub async fn run(cfg: &Config, req: Request) -> Result<Outcome> {
    let start = Instant::now();
    validate_session_name(&cfg.session)?;
    std::fs::create_dir_all(cfg.sessions_dir())
        .with_context(|| format!("failed to create {}", cfg.sessions_dir().display()))?;
    let _session_lock = lock_file(&cfg.lock_path())?;

    let mut state = SessionState::new(cfg);
    let saved = std::fs::read(cfg.state_path())
        .ok()
        .and_then(|raw| serde_json::from_slice::<PersistedState>(&raw).ok())
        .filter(|saved| saved.port == cfg.port);
    if let Some(saved) = saved {
        state.active_target_id = saved.active_target_id;
        state.last_snapshots = saved.last_snapshots;
    }

    let result = state.handle(req).await;

    let saved = PersistedState {
        port: cfg.port,
        active_target_id: state.active_target_id.take(),
        last_snapshots: std::mem::take(&mut state.last_snapshots),
    };
    std::fs::write(cfg.state_path(), serde_json::to_vec(&saved)?)
        .with_context(|| format!("failed to write {}", cfg.state_path().display()))?;

    Ok(Outcome {
        result,
        elapsed_ms: start.elapsed().as_millis() as u64,
        phases: std::mem::take(&mut state.phases),
    })
}

struct SessionState {
    port: u16,
    headless: bool,
    attach_only: bool,
    profile_dir: PathBuf,
    browser_log: PathBuf,
    active_target_id: Option<String>,
    cdp: Option<CdpClient>,
    last_snapshots: HashMap<String, String>,
    session: String,
    sessions_dir: PathBuf,
    launched_path: PathBuf,
    launch_lock_path: PathBuf,
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
    fn new(cfg: &Config) -> Self {
        Self {
            port: cfg.port,
            headless: cfg.headless,
            attach_only: cfg.attach_only,
            profile_dir: cfg.profile_dir(),
            browser_log: cfg.home_dir.join("browser.log"),
            active_target_id: None,
            cdp: None,
            last_snapshots: HashMap::new(),
            session: cfg.session.clone(),
            sessions_dir: cfg.sessions_dir(),
            launched_path: cfg.home_dir.join("browser.json"),
            launch_lock_path: cfg.home_dir.join("browser.lock"),
            defer_snap: false,
            step_label: None,
            phases: Vec::new(),
        }
    }

    async fn ensure_browser(&mut self, headless: bool) -> Result<()> {
        let policy = LaunchPolicy {
            headless,
            ..self.launch_policy()
        };
        let _launch_lock = lock_file(&self.launch_lock_path)?;
        if let Some(pid) = browser::ensure_browser(self.port, &policy).await? {
            let launched = LaunchedBrowser {
                port: self.port,
                pid,
            };
            std::fs::write(&self.launched_path, serde_json::to_vec(&launched)?)
                .with_context(|| format!("failed to write {}", self.launched_path.display()))?;
            self.active_target_id = None;
            self.last_snapshots.clear();
        }
        Ok(())
    }

    fn launched_browser(&self) -> Option<LaunchedBrowser> {
        std::fs::read(&self.launched_path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<LaunchedBrowser>(&raw).ok())
            .filter(|b| b.port == self.port)
    }

    fn session_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.sessions_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension()? == "json")
                    .then(|| path.file_stem()?.to_str().map(String::from))
                    .flatten()
            })
            .collect();
        if !names.contains(&self.session) {
            names.push(self.session.clone());
        }
        names.sort();
        names
    }

    fn launch_policy(&self) -> LaunchPolicy<'_> {
        LaunchPolicy {
            headless: self.headless,
            attach_only: self.attach_only,
            profile_dir: &self.profile_dir,
            log_path: &self.browser_log,
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
        self.ensure_browser(self.headless).await?;

        let targets = browser::list_page_targets(self.port).await?;
        let active = self
            .active_target_id
            .as_ref()
            .and_then(|active_id| targets.iter().find(|t| &t.id == active_id))
            .cloned();
        let target = match active {
            Some(target) => target,
            None if self.session == DEFAULT_SESSION && !targets.is_empty() => targets[0].clone(),
            None => browser::open_new_tab(self.port, Some("about:blank")).await?,
        };

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
        let key = self.baseline_key();

        let out = match self.last_snapshots.get(&key) {
            Some(prev) if opts.diff => diff_snapshots(prev, &interactive),
            _ => render_snapshot(&snap, opts),
        };

        self.last_snapshots.insert(key, interactive);
        self.mark("snapshot", start);
        Ok(out)
    }

    fn baseline_key(&self) -> String {
        self.active_target_id
            .clone()
            .unwrap_or_else(|| "default".into())
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
                let sessions = self
                    .session_names()
                    .into_iter()
                    .map(|name| {
                        if name == self.session {
                            format!("*{name}")
                        } else {
                            name
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                Ok(format!(
                    "cdp_port: {}\nattach_only: {}\nbrowser: {}\nlaunched_by_surf: {}\ntabs: {}\nsession: {}\nactive_tab: {}\nsessions: {sessions}\n",
                    self.port,
                    self.attach_only,
                    browser_info,
                    self.launched_browser()
                        .map_or_else(|| "no".to_string(), |b| format!("pid {}", b.pid)),
                    tabs.len(),
                    self.session,
                    self.active_target_id.as_deref().unwrap_or("none")
                ))
            }
            Request::Stop => {
                let Some(LaunchedBrowser { pid, .. }) = self.launched_browser() else {
                    return Ok("no surf-launched browser recorded; nothing to stop\n".into());
                };
                let _ = std::fs::remove_file(&self.launched_path);
                self.cdp = None;
                self.active_target_id = None;
                self.last_snapshots.clear();
                if browser::close_browser_if_launched(self.port, pid).await? {
                    Ok(format!(
                        "closed browser (pid {pid}) on 127.0.0.1:{}\n",
                        self.port
                    ))
                } else {
                    Ok(format!(
                        "browser on 127.0.0.1:{} is not the one surf launched (pid {pid}); left it running\n",
                        self.port
                    ))
                }
            }
            Request::Open { url, snap } => {
                let normalized = normalize_url(&url);
                let cdp = self.ensure_cdp().await?;
                let start = Instant::now();
                cdp.navigate(&normalized).await?;
                self.mark("navigate", start);
                let key = self.baseline_key();
                self.last_snapshots.remove(&key);
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
                    let quiet_ms = IDLE_QUIET_WINDOW.as_millis();
                    let expr = format!(
                        "new Promise((resolve) => {{
                            let timer = null;
                            const done = (r) => {{ obs.disconnect(); clearTimeout(maxT); clearTimeout(timer); resolve(r); }};
                            const bump = () => {{ clearTimeout(timer); timer = setTimeout(() => done('idle'), {quiet_ms}); }};
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
                WaitTarget::Label { label, timeout_ms } => {
                    let args = json!({ "query": label, "mode": "probe" });
                    let expr = format!("({RESOLVE_LABEL_JS})({args})");
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
                    loop {
                        let cdp = self.ensure_cdp().await?;
                        let val = cdp.eval_value(&expr).await?;
                        if val.get("found").and_then(Value::as_bool) == Some(true) {
                            return Ok(format!("found label: {}\n", json!(label)));
                        }
                        if tokio::time::Instant::now() >= deadline {
                            bail!(
                                "wait_label_timeout={timeout_ms}ms: no visible element matching \"{label}\" (run `surf snap` to inspect)"
                            );
                        }
                        tokio::time::sleep(LABEL_POLL_INTERVAL).await;
                    }
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
                self.ensure_browser(self.headless).await?;
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
            Request::Login { url } => {
                self.ensure_browser(false).await?;
                let snap = Box::pin(self.handle(Request::TabNew { url })).await?;
                Ok(format!(
                    "sign in using the opened window. Cookies persist in the browser profile ({profile} when surf launches the browser); later runs, headless included, reuse them.\nno window? the browser already on 127.0.0.1:{port} is headless: run `surf stop` and rerun `surf login`.\n{snap}",
                    profile = self.profile_dir.display(),
                    port = self.port
                ))
            }
            Request::TabNew { url } => {
                self.ensure_browser(self.headless).await?;
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
                self.last_snapshots.remove(&target.id);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_are_validated() {
        assert!(validate_session_name("agent-1_b").is_ok());
        let err = validate_session_name("../x").unwrap_err().to_string();
        assert!(err.contains("[A-Za-z0-9_-]"), "{err}");
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name(&"a".repeat(MAX_SESSION_NAME_CHARS + 1)).is_err());
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
