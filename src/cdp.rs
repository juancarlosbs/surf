use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct CdpClient {
    tx: mpsc::UnboundedSender<String>,
    pending: PendingMap,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
}

impl CdpClient {
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let timeout_ms: u64 = std::env::var("SURF_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15_000);

        let (ws_stream, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(ws_url),
        )
        .await
        .map_err(|_| anyhow!("cdp_ws_connect_timeout=5000ms for {ws_url}"))?
        .with_context(|| format!("failed to connect CDP WebSocket at {ws_url}"))?;

        let (mut sink, mut stream) = ws_stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        let alive_writer = Arc::clone(&alive);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            alive_writer.store(false, Ordering::Relaxed);
        });

        let pending_reader = Arc::clone(&pending);
        let alive_reader = Arc::clone(&alive);
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                let Ok(msg) = item else {
                    break;
                };
                if let Message::Text(text) = msg
                    && let Ok(val) = serde_json::from_str::<Value>(&text)
                    && let Some(id) = val.get("id").and_then(|v| v.as_u64())
                {
                    let waiter = pending_reader.lock().await.remove(&id);
                    if let Some(tx) = waiter {
                        if let Some(err) = val.get("error") {
                            let _ = tx.send(Err(anyhow!("CDP error: {err}")));
                        } else {
                            let _ = tx.send(Ok(val.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                }
            }
            alive_reader.store(false, Ordering::Relaxed);
            let mut map = pending_reader.lock().await;
            for (_, waiter) in map.drain() {
                let _ = waiter.send(Err(anyhow!("CDP WebSocket closed")));
            }
        });

        Ok(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_millis(timeout_ms),
            alive,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (resp_tx, resp_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, resp_tx);

        let payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });

        if self.tx.send(payload.to_string()).is_err() {
            self.pending.lock().await.remove(&id);
            bail!("CDP connection closed before sending {method}");
        }

        match tokio::time::timeout(self.timeout, resp_rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => bail!("CDP channel dropped while awaiting {method}"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!(
                    "cdp_call_timeout={}ms exceeded for {method}",
                    self.timeout.as_millis()
                )
            }
        }
    }

    pub async fn eval_value(&self, expression: &str) -> Result<Value> {
        let res = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;

        if let Some(exc) = res.get("exceptionDetails") {
            let desc = exc
                .pointer("/exception/description")
                .or_else(|| exc.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("JS exception");
            bail!("JS error: {desc}");
        }

        Ok(res.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    pub async fn navigate(&self, url: &str) -> Result<()> {
        let res = self.call("Page.navigate", json!({ "url": url })).await?;
        if let Some(err) = res.get("errorText").and_then(|v| v.as_str()) {
            bail!("Navigation failed for {url}: {err}");
        }
        self.wait_ready(10_000).await?;
        Ok(())
    }

    pub async fn wait_ready(&self, timeout_ms: u64) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Value::String(state)) = self.eval_value("document.readyState").await
                && (state == "complete" || state == "interactive")
            {
                tokio::time::sleep(Duration::from_millis(80)).await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    pub async fn click_point(&self, x: f64, y: f64) -> Result<()> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y }),
        )
        .await?;
        self.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mousePressed",
                "x": x,
                "y": y,
                "button": "left",
                "clickCount": 1
            }),
        )
        .await?;
        self.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseReleased",
                "x": x,
                "y": y,
                "button": "left",
                "clickCount": 1
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    pub async fn dispatch_key(&self, key: &str) -> Result<()> {
        let (key_name, code, vk) = map_key(key);
        self.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "rawKeyDown",
                "key": key_name,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
            }),
        )
        .await?;
        if key_name == "Enter" {
            self.call(
                "Input.dispatchKeyEvent",
                json!({ "type": "char", "text": "\r" }),
            )
            .await?;
        }
        self.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key_name,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn screenshot_png_base64(&self) -> Result<String> {
        let res = self
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .await?;
        res.get("data")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("CDP captureScreenshot returned no data"))
    }
}

fn map_key(input: &str) -> (&str, &str, u32) {
    match input.to_ascii_lowercase().as_str() {
        "enter" | "return" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "escape" | "esc" => ("Escape", "Escape", 27),
        "backspace" => ("Backspace", "Backspace", 8),
        "delete" => ("Delete", "Delete", 46),
        "space" => (" ", "Space", 32),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", 38),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", 40),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", 37),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", 39),
        "home" => ("Home", "Home", 36),
        "end" => ("End", "End", 35),
        "pageup" => ("PageUp", "PageUp", 33),
        "pagedown" => ("PageDown", "PageDown", 34),
        _ => (input, input, 0),
    }
}
