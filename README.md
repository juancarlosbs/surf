# surf

Fast, ultra-low-token browser automation daemon & CLI for AI agents (Claude Code, Codex, Antigravity, etc.), written in Rust over raw Chrome DevTools Protocol (CDP).

## Why surf?

| Bottleneck in typical browser MCPs | How `surf` solves it |
|---|---|
| Screenshot every step (~2k vision tokens) | Text-only compact `@ref` snapshots (~300 tokens); screenshots opt-in |
| Full DOM / A11y tree dump (10k–40k tokens) | Visible interactive elements + loud truncation budgets + `--filter` |
| Re-sending entire page after each click | `--diff` mode returns only added/removed lines (~40 tokens) |
| 1 LLM turn per click/type | `surf run` executes multi-step batches in 1 turn |
| Cold browser boot / MCP prompt bloat | Warm Unix-socket daemon + zero-overhead CLI + 40-line `SKILL.md` |

## Quick Start

```bash
cargo install --path .

# Auto-starts daemon + persistent Chrome profile (~/.surf/profile)
surf open https://news.ycombinator.com

# Batch multiple actions in 1 command
surf run 'click @3' 'wait idle' 'snap --diff'
```

## Attach to Your Real Chrome Session

Start Chrome with remote debugging enabled (or point `SURF_PORT` to an existing debugging port):

```bash
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 \
  --user-data-dir="$HOME/.surf/profile"
```

`surf` automatically attaches to any CDP listener on `SURF_PORT` (default `9222`) without spawning a second browser. Pass `--attach` (or `SURF_ATTACH_ONLY=1`) to never launch one: surf fails fast with `no CDP browser listening on 127.0.0.1:<port>` instead. If the port accepts connections but is not a CDP endpoint, surf fails with the original error rather than launching a browser that cannot bind it.

Flags apply per command, so for agents set it once in the environment:

```bash
export SURF_ATTACH_ONLY=1 SURF_PORT=9222
```

## Environment Variables

- `SURF_PORT` — CDP port (default: `9222`)
- `SURF_HEADLESS` — launch headless when auto-starting (`1`/`0`, `true`/`false`)
- `SURF_ATTACH_ONLY` — never launch a browser, only attach (`1`/`0`)
- `SURF_BROWSER` — custom Chromium binary path
- `SURF_HOME` — daemon socket, PID, and persistent profile directory (default: `~/.surf`)
- `SURF_TIMEOUT_MS` — per-CDP-call timeout in ms (default: `15000`)

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo build && scripts/e2e.sh   # headless browser on an isolated port/profile (SURF_E2E_PORT, default 9444)
```

CI runs both on every PR.
