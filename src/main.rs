mod browser;
mod cdp;
mod cli;
mod protocol;
mod session;
mod snapshot;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, command_to_request};
use session::{Config, Outcome};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config {
        port: cli.port,
        headless: cli.headless,
        attach_only: cli.attach_only,
        session: cli.session,
        home_dir: browser::surf_home_dir()?,
    };
    let req = command_to_request(cli.command)?;
    let outcome = session::run(&cfg, req).await?;
    if cli.timing {
        print_timing(&outcome);
    }
    print!("{}", outcome.result?);
    Ok(())
}

fn print_timing(outcome: &Outcome) {
    eprintln!("[surf: {}ms]", outcome.elapsed_ms);
    for phase in &outcome.phases {
        eprintln!("  {} {:.1}ms", phase.name, phase.us as f64 / 1000.0);
    }
}
