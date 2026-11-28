# surf

Fast, ultra-low-token browser automation CLI for AI agents (Claude Code, Codex, Antigravity, etc.), written in Rust over raw Chrome DevTools Protocol (CDP).

## Why surf?

| Bottleneck in typical browser MCPs | How `surf` solves it |
|---|---|
| Screenshot every step (~2k vision tokens) | Text-only compact `@ref` snapshots (~300 tokens); screenshots opt-in |
| Full DOM / A11y tree dump (10k–40k tokens) | Visible interactive elements + loud truncation budgets + `--filter` |
| Re-sending entire page after each click | `--diff` mode returns only added/removed lines (~40 tokens) |
| 1 LLM turn per click/type | `surf run` executes multi-step batches in 1 turn |
| Cold browser boot / MCP prompt bloat | Persistent browser + ~4ms per-command CLI (no daemon) + short `SKILL.md` |

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/juancarlosbs/surf/main/scripts/install.sh | sh
```

Prebuilt for macOS (arm64, x64) and Linux (x64, arm64; static musl). Installs to `~/.local/bin` (`SURF_INSTALL_DIR` to change, `SURF_VERSION=v0.1.0` to pin). From source: `cargo install --git https://github.com/juancarlosbs/surf`.

## Quick Start

```bash
# Auto-launches a browser with a persistent profile (~/.surf/profile)
surf open https://news.ycombinator.com

# Batch multiple actions in 1 command
surf run 'click @3' 'wait idle' 'snap --diff'
```

## Use Your Logged-In Browser

surf drives a Chromium browser over CDP. Pick one setup:

### A. surf profile (recommended)

surf launches its own browser with a persistent profile (`~/.surf/profile`). Sign in once, with a window:

```bash
surf login https://github.com/login   # opens a window; sign in, then close it or keep it
surf open https://github.com          # later runs reuse the cookies, headless included
```

If `surf login` shows no window, a headless browser from an earlier run already owns the port: run `surf stop` and rerun.

### B. Your everyday browser

Start it with remote debugging, then attach:

```bash
# Chrome / Brave / Edge: since Chrome 136, --remote-debugging-port is ignored on the
# default profile, so it needs a separate --user-data-dir (sign in there once).
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 --user-data-dir="$HOME/.chrome-surf"

# Dia, Arc and other Chromium browsers: quit it, then relaunch with the flag.
"/Applications/Dia.app/Contents/MacOS/Dia" --remote-debugging-port=9222

export SURF_ATTACH_ONLY=1 SURF_PORT=9222   # never launch a second browser
surf tabs
```

`surf` attaches to any CDP listener on `SURF_PORT` (default `9222`). With `--attach` (or `SURF_ATTACH_ONLY=1`) it never launches one and fails fast with `no CDP browser listening on 127.0.0.1:<port>`. If the port accepts connections but is not a CDP endpoint, surf fails with the original error rather than launching a browser that cannot bind it. Flags apply per command, so agents should set the env vars once.

## How it works

No daemon. Each `surf` command connects to the browser over CDP, runs, and exits (~1ms more than a warm daemon, see #14). The active tab and diff baselines live in `SURF_HOME/sessions/<name>.json`; a per-session file lock serializes commands of one session. `surf stop` closes the browser surf launched and never one it only attached to.

## Parallel agents

Give each agent its own session so they do not switch each other's tab or diff baselines:

```bash
SURF_SESSION=research surf open https://example.com
SURF_SESSION=checkout surf open https://shop.example.com   # gets its own tab
surf status                                                # lists sessions
```

`default` keeps using the browser's current tab, which is what you want when attaching to your everyday browser. Other sessions open a dedicated tab on first use and reopen one if it was closed. All sessions share one browser.

## Environment Variables

- `SURF_PORT` — CDP port (default: `9222`)
- `SURF_HEADLESS` — launch headless when auto-starting (`1`/`0`, `true`/`false`)
- `SURF_ATTACH_ONLY` — never launch a browser, only attach (`1`/`0`)
- `SURF_BROWSER` — custom Chromium binary path
- `SURF_HOME` — session state (`sessions/`), launched-browser record (`browser.json`), `browser.log` and persistent profile directory (default: `~/.surf`)
- `SURF_SESSION` — session name (default `default`); see below
- `SURF_TIMEOUT_MS` — per-CDP-call timeout in ms (default: `15000`)

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo build && scripts/e2e.sh   # headless browser on an isolated port/profile (SURF_E2E_PORT, default 9444)
```

CI runs both on every PR.
