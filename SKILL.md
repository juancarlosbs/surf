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
| `surf click @N` | Click element + return diff |
| `surf click-label "<text>"` | Click visible element by aria-label, placeholder, label, or innerText + return diff |
| `surf fill @N "text"` | Focus, clear, type + return diff |
| `surf fill-label "<label>" "<value>"` | Focus, clear, type into visible input by label/placeholder/aria-label + return diff |
| `surf key Enter\|Escape\|Tab` | Send key press |
| `surf scroll up\|down\|top\|bottom [px]` | Scroll viewport |
| `surf wait <ms>\|idle\|text:<str>` | Wait for time, DOM settle, or text |
| `surf run "<step1>" "<step2>" ...` | Execute batch in 1 roundtrip |
| `surf eval "<js>"` | Evaluate JS expression |
| `surf tabs` / `surf tab <idx\|new\|close>` | Manage browser tabs |
| `surf status` / `surf stop` | Daemon status / shutdown |

## Rules
- Never take a screenshot unless visual layout/CSS debugging is explicitly needed.
- Always batch multi-step interactions (`open` + `wait` + `fill-label` / `click-label` or `fill` + `click`) with `surf run`.
- Prefer `click-label` / `fill-label` when labels are known to avoid an extra `surf snap` turn.
- If a ref fails (`not found or detached`), run `surf snap` to refresh refs.
