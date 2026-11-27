mod browser;
mod cdp;
mod cli;
mod daemon;
mod protocol;
mod snapshot;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{Cli, Command, command_to_request};
use daemon::DaemonConfig;
use protocol::{Request, Response};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let home_dir = browser::surf_home_dir()?;
    let cfg = DaemonConfig {
        port: cli.port,
        headless: cli.headless,
        attach_only: cli.attach_only,
        home_dir,
    };

    if let Command::Daemon { foreground } = cli.command {
        if foreground {
            return daemon::run_daemon(cfg).await;
        }
        spawn_daemon(&cfg)?;
        wait_for_socket(&cfg).await?;
        println!("surf daemon started on {}", cfg.socket_path().display());
        return Ok(());
    }

    let req = command_to_request(cli.command)?;
    let resp = send_request(&cfg, &req).await?;

    if cli.timing {
        print_timing(&resp);
    }

    if resp.ok {
        if let Some(out) = resp.output {
            print!("{out}");
        }
        Ok(())
    } else {
        bail!("{}", resp.error.unwrap_or_else(|| "unknown error".into()))
    }
}

fn print_timing(resp: &Response) {
    if let Some(ms) = resp.elapsed_ms {
        eprintln!("[surf: {ms}ms]");
    }
    for phase in &resp.phases {
        eprintln!("  {} {:.1}ms", phase.name, phase.us as f64 / 1000.0);
    }
}

async fn send_request(cfg: &DaemonConfig, req: &Request) -> Result<Response> {
    let sock_path = cfg.socket_path();

    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(_) => {
            if matches!(req, Request::Stop) {
                return Ok(Response::ok("daemon not running\n", 0));
            }
            spawn_daemon(cfg)?;
            wait_for_socket(cfg).await?
        }
    };

    let (reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut line = String::new();
    let mut buf = BufReader::new(reader);
    buf.read_line(&mut line).await?;
    if line.trim().is_empty() {
        bail!("daemon closed connection without response");
    }

    serde_json::from_str(&line).context("failed to parse daemon response")
}

fn spawn_daemon(cfg: &DaemonConfig) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate current surf binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--port")
        .arg(cfg.port.to_string())
        .arg("daemon")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if cfg.headless {
        cmd.arg("--headless");
    }
    if cfg.attach_only {
        cmd.arg("--attach");
    }

    cmd.spawn().context("failed to spawn surf daemon")?;
    Ok(())
}

async fn wait_for_socket(cfg: &DaemonConfig) -> Result<UnixStream> {
    let sock_path = cfg.socket_path();
    let start = Instant::now();
    let timeout = Duration::from_secs(3);
    while start.elapsed() < timeout {
        if let Ok(stream) = UnixStream::connect(&sock_path).await {
            return Ok(stream);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!(
        "daemon_boot_timeout=3000ms exceeded waiting for {}",
        sock_path.display()
    )
}
