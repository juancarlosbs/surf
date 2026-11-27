use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

// Receipts (2026-11-27, M-series mac, headless Chrome): loopback discovery round trip <6ms,
// cold boot to first /json/version 867ms. Limits sit >100x / ~11x past those as tripwires.
const CDP_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CDP_HTTP_READ_TIMEOUT: Duration = Duration::from_secs(5);
const BROWSER_BOOT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(rename = "type")]
    pub target_type: String,
    pub web_socket_debugger_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserVersion {
    #[serde(rename = "Browser")]
    pub browser: String,
}

pub fn surf_home_dir() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("SURF_HOME") {
        return Ok(PathBuf::from(custom));
    }
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".surf"))
}

pub fn find_browser_binary() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("SURF_BROWSER") {
        let p = PathBuf::from(&custom);
        if p.exists() {
            return Ok(p);
        }
        bail!("SURF_BROWSER={custom} does not exist");
    }

    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Arc.app/Contents/MacOS/Arc",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Dia.app/Contents/MacOS/Dia",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/brave-browser",
        ]
    };

    for path in candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }

    bail!(
        "No Chromium browser found. Checked {:?}. Set SURF_BROWSER=/path/to/chrome.",
        candidates
    )
}

pub async fn http_request(port: u16, method: &str, path: &str) -> Result<(u16, String)> {
    let addr = format!("127.0.0.1:{port}");
    let mut stream = tokio::time::timeout(CDP_HTTP_CONNECT_TIMEOUT, TcpStream::connect(&addr))
        .await
        .map_err(|_| {
            anyhow!(
                "cdp_http_connect_timeout={}ms exceeded on {addr}",
                CDP_HTTP_CONNECT_TIMEOUT.as_millis()
            )
        })?
        .with_context(|| format!("failed to connect to CDP at {addr}"))?;

    let req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;

    let raw = tokio::time::timeout(CDP_HTTP_READ_TIMEOUT, read_http_response(&mut stream))
        .await
        .map_err(|_| {
            anyhow!(
                "cdp_http_read_timeout={}ms exceeded for {method} {path}",
                CDP_HTTP_READ_TIMEOUT.as_millis()
            )
        })??;

    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response from CDP"))?;

    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let is_chunked = head.lines().any(|l| {
        l.to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
    });

    let decoded = if is_chunked {
        decode_chunked(body)
    } else {
        body.to_string()
    };

    Ok((status, decoded))
}

async fn read_http_response(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let mut header_end: Option<usize> = None;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);

        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = pos + 4;
                header_end = Some(end);
                let head = String::from_utf8_lossy(&buf[..pos]);
                for line in head.lines() {
                    let lower = line.to_ascii_lowercase();
                    if let Some(val) = lower.strip_prefix("content-length:") {
                        content_length = val.trim().parse().ok();
                    } else if lower.contains("transfer-encoding: chunked") {
                        is_chunked = true;
                    }
                }
            }
        }

        if let Some(end) = header_end {
            if let Some(len) = content_length {
                if buf.len() >= end + len {
                    buf.truncate(end + len);
                    break;
                }
            } else if is_chunked && buf[end..].windows(5).any(|w| w == b"0\r\n\r\n") {
                break;
            }
        }
    }

    Ok(buf)
}

fn decode_chunked(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some((size_hex, after)) = rest.split_once("\r\n") {
        let size_str = size_hex.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_str, 16) else {
            break;
        };
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].strip_prefix("\r\n").unwrap_or(&after[size..]);
    }
    out
}

pub async fn get_version(port: u16) -> Result<BrowserVersion> {
    let (status, body) = http_request(port, "GET", "/json/version").await?;
    if status != 200 {
        bail!("CDP /json/version returned HTTP {status}: {body}");
    }
    serde_json::from_str(&body).context("failed to parse /json/version JSON")
}

pub async fn list_page_targets(port: u16) -> Result<Vec<Target>> {
    let (status, body) = http_request(port, "GET", "/json/list").await?;
    if status != 200 {
        bail!("CDP /json/list returned HTTP {status}: {body}");
    }
    let targets: Vec<Target> = serde_json::from_str(&body).context("failed to parse /json/list")?;
    Ok(targets
        .into_iter()
        .filter(|t| t.target_type == "page" && t.web_socket_debugger_url.is_some())
        .collect())
}

pub async fn open_new_tab(port: u16, url: Option<&str>) -> Result<Target> {
    let path = match url {
        Some(u) if !u.is_empty() => format!("/json/new?{}", percent_encode_url(u)),
        _ => "/json/new".to_string(),
    };
    let (status, body) = http_request(port, "PUT", &path).await?;
    let (status, body) = if status == 405 {
        http_request(port, "GET", &path).await?
    } else {
        (status, body)
    };
    if status != 200 {
        bail!("CDP {path} returned HTTP {status}: {body}");
    }
    serde_json::from_str(&body).context("failed to parse new tab JSON")
}

pub async fn close_tab(port: u16, target_id: &str) -> Result<()> {
    let (status, body) = http_request(port, "GET", &format!("/json/close/{target_id}")).await?;
    if status != 200 {
        bail!("CDP /json/close/{target_id} returned HTTP {status}: {body}");
    }
    Ok(())
}

pub async fn activate_tab(port: u16, target_id: &str) -> Result<()> {
    let (status, body) = http_request(port, "GET", &format!("/json/activate/{target_id}")).await?;
    if status != 200 {
        bail!("CDP /json/activate/{target_id} returned HTTP {status}: {body}");
    }
    Ok(())
}

fn percent_encode_url(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b':' | b'/' | b'?' | b'&' | b'=' | b'#' | b'%'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn is_connection_refused(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
    })
}

pub struct LaunchPolicy<'a> {
    pub headless: bool,
    pub attach_only: bool,
    pub profile_dir: &'a Path,
}

pub async fn ensure_browser(port: u16, policy: &LaunchPolicy<'_>) -> Result<BrowserVersion> {
    match get_version(port).await {
        Ok(ver) => return Ok(ver),
        Err(e) if !is_connection_refused(&e) => {
            return Err(e.context(format!(
                "127.0.0.1:{port} accepts connections but is not a usable CDP endpoint; refusing to launch another browser on an occupied port"
            )));
        }
        Err(_) => {}
    }

    if policy.attach_only {
        bail!(
            "no CDP browser listening on 127.0.0.1:{port} (attach-only mode via --attach/SURF_ATTACH_ONLY; start your browser with --remote-debugging-port={port})"
        );
    }

    let bin = find_browser_binary()?;
    let profile_dir = policy.profile_dir;
    std::fs::create_dir_all(profile_dir)
        .with_context(|| format!("failed to create profile dir {}", profile_dir.display()))?;

    let mut cmd = Command::new(&bin);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-timer-throttling")
        .arg("--disable-renderer-backgrounding")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if policy.headless {
        cmd.arg("--headless=new");
    }

    cmd.spawn()
        .with_context(|| format!("failed to launch browser {}", bin.display()))?;

    let start = Instant::now();
    while start.elapsed() < BROWSER_BOOT_TIMEOUT {
        if let Ok(ver) = get_version(port).await {
            return Ok(ver);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    bail!(
        "browser_boot_timeout={}ms exceeded waiting for CDP on 127.0.0.1:{port} (binary: {}). Set SURF_BROWSER=/path/to/browser to launch another binary, or start your browser with --remote-debugging-port={port} and pass --attach",
        BROWSER_BOOT_TIMEOUT.as_millis(),
        bin.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attach_only_fails_fast_without_launching() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = std::env::temp_dir();
        let policy = LaunchPolicy {
            headless: true,
            attach_only: true,
            profile_dir: &dir,
        };
        let err = ensure_browser(port, &policy).await.unwrap_err().to_string();
        assert!(
            err.contains(&format!("no CDP browser listening on 127.0.0.1:{port}")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn occupied_non_cdp_port_is_not_treated_as_free() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let dir = std::env::temp_dir();
        let policy = LaunchPolicy {
            headless: true,
            attach_only: false,
            profile_dir: &dir,
        };
        let err = format!("{:#}", ensure_browser(port, &policy).await.unwrap_err());
        assert!(err.contains("refusing to launch another browser"), "{err}");
        assert!(err.contains("HTTP 404"), "{err}");
    }
}
