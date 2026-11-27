use crate::protocol::{
    Request, ScrollDirection, SnapOptions, WaitTarget, default_max_elements, default_max_text_chars,
};
use anyhow::{Result, anyhow, bail};
use clap::builder::FalseyValueParser;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "surf",
    version,
    about = "Fast, ultra-low-token browser automation for AI agents via CDP"
)]
pub struct Cli {
    /// CDP remote debugging port (default: 9222, env: SURF_PORT)
    #[arg(long, env = "SURF_PORT", default_value_t = 9222, global = true)]
    pub port: u16,

    /// Launch browser in headless mode when auto-starting
    #[arg(long, env = "SURF_HEADLESS", value_parser = FalseyValueParser::new(), global = true)]
    pub headless: bool,

    /// Never launch a browser; fail fast if no CDP browser listens on --port (env: SURF_ATTACH_ONLY)
    #[arg(long = "attach", env = "SURF_ATTACH_ONLY", value_parser = FalseyValueParser::new(), global = true)]
    pub attach_only: bool,

    /// Print total and per-phase/per-step timing to stderr
    #[arg(long, default_value_t = false, global = true)]
    pub timing: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the background daemon
    Daemon {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// Show daemon & browser status
    Status,
    /// Stop the background daemon
    Stop,
    /// Navigate to a URL and return a compact snapshot
    Open {
        url: String,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, default_value_t = default_max_elements())]
        max_elements: usize,
        #[arg(long, default_value_t = default_max_text_chars())]
        max_text_chars: usize,
    },
    /// Capture compact interactive snapshot (or diff vs previous)
    Snap {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        diff: bool,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, default_value_t = default_max_elements())]
        max_elements: usize,
        #[arg(long, default_value_t = default_max_text_chars())]
        max_text_chars: usize,
    },
    /// Click an element by @ref (e.g. `surf click @3`)
    Click {
        ref_id: String,
        #[arg(long, default_value_t = false)]
        full: bool,
    },
    /// Click a visible element matched by label/text (aria-label, placeholder, label, innerText)
    #[command(name = "click-label")]
    ClickLabel {
        label: String,
        #[arg(long, default_value_t = false)]
        full: bool,
    },
    /// Focus, clear, and type into an input by @ref
    Fill {
        ref_id: String,
        text: String,
        #[arg(long, default_value_t = false)]
        full: bool,
    },
    /// Focus, clear, and type into a visible input matched by label/placeholder/aria-label
    #[command(name = "fill-label")]
    FillLabel {
        label: String,
        text: String,
        #[arg(long, default_value_t = false)]
        full: bool,
    },
    /// Insert raw text at current focus
    Type { text: String },
    /// Dispatch a key press (Enter, Escape, Tab, ArrowDown, ...)
    Key { key: String },
    /// Scroll the page (up, down, top, bottom)
    Scroll {
        direction: String,
        #[arg(default_value_t = 600)]
        amount: i32,
    },
    /// Wait for ms (`500`), DOM `idle`, or `text:Loaded`
    Wait {
        target: String,
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    /// Evaluate JavaScript in the page and return JSON
    Eval { expr: String },
    /// Opt-in PNG screenshot (use only when visual check is required)
    Screenshot { path: Option<String> },
    /// List open tabs
    Tabs,
    /// Switch, open, or close a tab (`surf tab 0`, `surf tab new [url]`, `surf tab close [idx]`)
    Tab { action: String, arg: Option<String> },
    /// Execute multiple actions in a single daemon roundtrip
    Run {
        /// Pass a JSON array of Request objects instead of terse step strings
        #[arg(long)]
        json: Option<String>,
        /// Terse step strings, e.g. `surf run "fill @1 user" "fill @2 pass" "click @3" "wait idle"`
        steps: Vec<String>,
    },
}

pub fn parse_ref(raw: &str) -> Result<u32> {
    let stripped = raw.strip_prefix('@').unwrap_or(raw);
    stripped
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid ref \"{raw}\" — expected @1, @2, ..."))
}

pub fn command_to_request(cmd: Command) -> Result<Request> {
    match cmd {
        Command::Status => Ok(Request::Status),
        Command::Stop => Ok(Request::Stop),
        Command::Open {
            url,
            all,
            filter,
            max_elements,
            max_text_chars,
        } => Ok(Request::Open {
            url,
            snap: SnapOptions {
                all,
                diff: false,
                filter,
                max_elements,
                max_text_chars,
            },
        }),
        Command::Snap {
            all,
            diff,
            filter,
            max_elements,
            max_text_chars,
        } => Ok(Request::Snap {
            opts: SnapOptions {
                all,
                diff,
                filter,
                max_elements,
                max_text_chars,
            },
        }),
        Command::Click { ref_id, full } => Ok(Request::Click {
            ref_id: parse_ref(&ref_id)?,
            diff: !full,
        }),
        Command::ClickLabel { label, full } => Ok(Request::ClickLabel { label, diff: !full }),
        Command::Fill { ref_id, text, full } => Ok(Request::Fill {
            ref_id: parse_ref(&ref_id)?,
            text,
            diff: !full,
        }),
        Command::FillLabel { label, text, full } => Ok(Request::FillLabel {
            label,
            text,
            diff: !full,
        }),
        Command::Type { text } => Ok(Request::Type { text }),
        Command::Key { key } => Ok(Request::Key { key }),
        Command::Scroll { direction, amount } => {
            let dir = match direction.to_ascii_lowercase().as_str() {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "top" => ScrollDirection::Top,
                "bottom" => ScrollDirection::Bottom,
                other => bail!("invalid scroll direction \"{other}\" — use up|down|top|bottom"),
            };
            Ok(Request::Scroll {
                direction: dir,
                amount,
            })
        }
        Command::Wait { target, timeout } => {
            let wt = if target.eq_ignore_ascii_case("idle") {
                WaitTarget::Idle {
                    timeout_ms: timeout,
                }
            } else if let Some(text) = target.strip_prefix("text:") {
                WaitTarget::Text {
                    text: text.to_string(),
                    timeout_ms: timeout,
                }
            } else if let Ok(ms) = target.parse::<u64>() {
                WaitTarget::Ms { ms }
            } else {
                bail!("invalid wait target \"{target}\" — use <ms>, idle, or text:<string>");
            };
            Ok(Request::Wait { target: wt })
        }
        Command::Eval { expr } => Ok(Request::Eval { expr }),
        Command::Screenshot { path } => Ok(Request::Screenshot { path }),
        Command::Tabs => Ok(Request::Tabs),
        Command::Tab { action, arg } => match action.as_str() {
            "new" => Ok(Request::TabNew { url: arg }),
            "close" => {
                let idx = match arg {
                    Some(s) => Some(
                        s.parse::<usize>()
                            .map_err(|_| anyhow!("invalid tab index {s}"))?,
                    ),
                    None => None,
                };
                Ok(Request::TabClose { index: idx })
            }
            num => {
                let idx = num.parse::<usize>().map_err(|_| {
                    anyhow!("invalid tab action \"{num}\" — use <index>, new, or close")
                })?;
                Ok(Request::TabSwitch { index: idx })
            }
        },
        Command::Run { json, steps } => {
            if let Some(raw_json) = json {
                let parsed: Vec<Request> = serde_json::from_str(&raw_json)
                    .map_err(|e| anyhow!("invalid --json batch: {e}"))?;
                return Ok(Request::Batch { steps: parsed });
            }
            if steps.is_empty() {
                bail!("`surf run` requires at least one step or --json");
            }
            let mut reqs = Vec::with_capacity(steps.len());
            for step in steps {
                reqs.push(parse_batch_step(&step)?);
            }
            Ok(Request::Batch { steps: reqs })
        }
        Command::Daemon { .. } => bail!("daemon command handled directly"),
    }
}

pub fn parse_batch_step(step: &str) -> Result<Request> {
    let mut argv = vec!["surf".to_string()];
    argv.extend(split_words(step)?);
    let parsed = Cli::try_parse_from(argv).map_err(|e| anyhow!("invalid step \"{step}\": {e}"))?;
    command_to_request(parsed.command)
}

fn split_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some(_) if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }

    if quote.is_some() {
        bail!("unclosed quote in step: {input}");
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_attach_and_headless_flags() {
        let cli = Cli::try_parse_from(["surf", "--attach", "--headless", "status"]).unwrap();
        assert!(cli.attach_only && cli.headless);
    }

    #[test]
    fn parses_batch_steps_with_quotes() {
        let req = parse_batch_step(r#"fill @3 "hello world""#).unwrap();
        match req {
            Request::Fill { ref_id, text, diff } => {
                assert_eq!(ref_id, 3);
                assert_eq!(text, "hello world");
                assert!(diff);
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn parses_wait_variants() {
        assert!(matches!(
            parse_batch_step("wait idle").unwrap(),
            Request::Wait {
                target: WaitTarget::Idle { .. }
            }
        ));
        assert!(matches!(
            parse_batch_step("wait 250").unwrap(),
            Request::Wait {
                target: WaitTarget::Ms { ms: 250 }
            }
        ));
        assert!(matches!(
            parse_batch_step(r#"wait "text:Dashboard""#).unwrap(),
            Request::Wait {
                target: WaitTarget::Text { ref text, .. }
            } if text == "Dashboard"
        ));
    }

    #[test]
    fn parses_semantic_label_batch_steps() {
        let click_req = parse_batch_step(r#"click-label "Sign in""#).unwrap();
        match click_req {
            Request::ClickLabel { label, diff } => {
                assert_eq!(label, "Sign in");
                assert!(diff);
            }
            _ => panic!("expected ClickLabel"),
        }

        let fill_req =
            parse_batch_step(r#"fill-label "Post text" "hello from surf" --full"#).unwrap();
        match fill_req {
            Request::FillLabel { label, text, diff } => {
                assert_eq!(label, "Post text");
                assert_eq!(text, "hello from surf");
                assert!(!diff);
            }
            _ => panic!("expected FillLabel"),
        }
    }
}
