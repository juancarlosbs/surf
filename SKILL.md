---
name: surf
description: Fast, low-token browser automation via `surf` CLI (persistent CDP daemon). Use whenever you need to browse, click, fill forms, inspect web apps, or interact with the user's Chrome session.
---

# surf — Fast Browser Automation for Agents

Prefer `surf` over screenshot-based browser tools. It talks to a warm local Chrome daemon over CDP and returns compact `@ref` snapshots (~300 tokens) and diffs (~40 tokens).

## Core Workflow

1. **Open & inspect**:
   ```bash
   surf open https://example.com
   ```
   Output lists visible interactive elements as `@1 [input:text] "Search"`, `@2 [btn] "Submit"`, etc.

2. **Batch multiple actions in 1 turn** (always prefer `surf run` over sequential tool calls):
   ```bash
   surf run 'fill @1 "my query"' 'click @2' 'wait idle' 'snap --diff'
   ```
   Or skip the initial `surf snap` and `wait` turns completely with semantic label commands (`click-label` / `fill-label`), which auto-wait up to 5s and resolve visible elements directly in the daemon by `aria-label`, `placeholder`, `<label>`, or `innerText`:
   ```bash
   surf run 'open https://example.com' 'fill-label "Search" "rust cdp"' 'click-label "Submit"'
   ```

3. **Navigate & filter large pages**:
   ```bash
   surf snap --filter "login"     # only matching interactive elements
   surf snap --all                # include off-viewport elements
   surf snap --diff               # only what changed since last snap
   ```

4. **Escape hatches**:
   ```bash
   surf eval 'document.title'     # read page state / JSON directly
   surf screenshot /tmp/page.png  # opt-in visual check only when layout matters
   ```

## Commands

| Command | Purpose |
|---|---|
| `surf open <url>` | Navigate + wait ready + compact snapshot |
| `surf snap [--diff] [--all] [--filter q]` | Compact interactive snapshot |
| `surf click @N` | Click element + return diff of interactive elements |
| `surf click-label "<text>"` | Click visible element by aria-label, placeholder, label, or innerText + return diff |
| `surf fill @N "text"` | Focus, clear, type + `filled @N val="..."` (real DOM value) + diff |
| `surf fill-label "<label>" "<value>"` | Same as `fill`, matched by label/placeholder/aria-label |
| `surf key Enter\|Escape\|Tab` | Send key press |
| `surf scroll up\|down\|top\|bottom [px]` | Scroll viewport |
| `surf wait <ms>\|idle\|text:<str>\|label:<text>` | Wait for time, DOM settle (350ms quiet), text, or a visible element |
| `surf run "<step1>" "<step2>" ...` | Execute batch in 1 roundtrip |
| `surf eval "<js>"` | Evaluate JS expression |
| `surf tabs` / `surf tab <idx\|new\|close>` | Manage browser tabs |
| `surf status` / `surf stop` | Daemon status / shutdown |
| `surf login [url]` | Open a window on the surf profile so the user can sign in once (ask the user to do it) |
| `surf --attach ...` (`SURF_ATTACH_ONLY=1`) | Never launch a browser; fail fast if none listens on the CDP port |
| `surf --timing ...` | Print total + per-step/per-phase timing to stderr |

## Notes
- Action diffs (`click`, `fill`, ...) cover interactive elements only; page text comes only from `surf open` / `surf snap`.
- Inside `surf run`, only the last step (and explicit `snap` steps) take a snapshot. Refs from your last snapshot stay valid until the page navigates or re-renders that element. After an `open` (or `tab`) inside a batch, use `click-label`/`fill-label` or add a `snap` step before using `@N`.
- `--attach`, `--headless` and `--port` apply to each command, even when the daemon is already running.
- Driving an existing browser (e.g. Dia/Chrome on 9222)? Set `SURF_ATTACH_ONLY=1` and `SURF_PORT` in the agent environment instead of passing `--attach` per call: a command without it may auto-launch a separate browser.

## Safety (logged-in sessions)
`surf` drives the user's real, logged-in browser (X, GitHub, Gmail, ...).
- Never click Post, Send, Create, Delete, Submit, or any public/irreversible action without explicit user confirmation. Draft requests stop at the filled field.
- Prefer `surf eval` for cheap field/state verification instead of extra snapshots or screenshots.
- Measure with `surf --timing` before claiming a speedup.

## Rules
- Never take a screenshot unless visual layout/CSS debugging is explicitly needed.
- Always batch multi-step interactions (`open` + `wait` + `fill-label` / `click-label` or `fill` + `click`) with `surf run`.
- Prefer `click-label` / `fill-label` when labels are known to avoid an extra `surf snap` turn.
- Prefer `wait label:"..."` or `wait text:...` over `wait idle`: they return as soon as the thing appears; `idle` always costs at least 350ms.
- If a ref fails (`not found or detached`), run `surf snap` to refresh refs.
