#!/usr/bin/env bash
# End-to-end checks against an isolated headless browser (own port + profile).
# Usage: scripts/e2e.sh [path/to/surf]   env: SURF_E2E_PORT (default 9444), SURF_BROWSER
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
surf=${1:-$repo/target/debug/surf}
fixtures="file://$repo/tests/e2e"
port=${SURF_E2E_PORT:-9444}
home=$(mktemp -d /tmp/surf-e2e.XXXXXX)

export SURF_HOME=$home SURF_PORT=$port SURF_HEADLESS=1
unset SURF_ATTACH_ONLY

cleanup() {
    "$surf" stop >/dev/null 2>&1 || true
    pkill -f "user-data-dir=$home/profile" 2>/dev/null || true
    for _ in $(seq 50); do
        pgrep -f "user-data-dir=$home/profile" >/dev/null || break
        sleep 0.1
    done
    rm -rf "$home" 2>/dev/null || echo "warning: could not remove $home"
}
trap cleanup EXIT

run() {
    local rc=0 out
    out=$("$surf" "$@" 2>&1) || rc=$?
    printf '%s' "$out"
    if ((rc != 0)); then printf '\n[surf %s exited %d]' "$*" "$rc"; fi
}

failures=0
check() {
    local name=$1 out=$2 needle=$3 mode=${4:-contains}
    if [[ $mode == contains && $out == *"$needle"* ]] || [[ $mode == absent && $out != *"$needle"* ]]; then
        echo "ok   $name"
    else
        echo "FAIL $name: expected output to be $mode \"$needle\""
        echo "$out" | sed 's/^/     | /'
        failures=$((failures + 1))
    fi
}

out=$(run --attach open "$fixtures/form.html")
check "attach-only fails fast" "$out" "no CDP browser listening on 127.0.0.1:$port"
run stop >/dev/null

start=$(date +%s)
out=$(SURF_BROWSER=/usr/bin/false run tabs)
check "dead browser fails fast" "$out" "browser exited"
elapsed=$(($(date +%s) - start))
check "dead browser fails within 5s (took ${elapsed}s)" "$((elapsed < 5))" "1"
run stop >/dev/null

out=$(run --timing open "$fixtures/form.html")
echo "$out" | grep -E '^\[surf|^  ' | sed 's/^/     boot: /' || true
check "open lists inputs" "$out" '@1 [input:text] "Search"'
check "open includes page text" "$out" "footer text"
check "timing shows phases" "$out" "navigate"

out=$(run fill-label Search "rust cdp")
check "fill-label echoes DOM value" "$out" 'filled "Search" val="rust cdp"'

out=$(run fill-label "Post text" hello)
check "fill-label contenteditable" "$out" 'filled "Post text" val="hello"'

out=$(run fill @1 abc)
check "fill by ref" "$out" 'filled @1 val="abc"'

out=$(run click-label Go)
check "click diff shows new element" "$out" '+ @4 [btn] "Undo"'
check "click diff omits page text" "$out" "footer text" absent

out=$(run snap --diff)
check "snap --diff unchanged" "$out" "(no visible changes)"

out=$(run --timing run "fill-label Search xyz" "click-label Go")
check "batch defers intermediate snapshot" "$out" "step1/2 snapshot" absent
check "batch final diff" "$out" 'val="xyz"'

out=$(run run "open $fixtures/next.html" "click-label Next")
check "batch with navigation returns full snapshot" "$out" "title: page two"
check "batch with navigation is not a diff" "$out" "- @" absent

out=$(run wait "text:Done")
check "wait text" "$out" "found text: Done"

out=$(run eval "document.title")
check "eval" "$out" "page two"

out=$(run login "$fixtures/form.html")
check "login opens tab with sign-in hint" "$out" "sign in using the opened window"
run tab close >/dev/null

out=$(run tab new "$fixtures/form.html")
check "tab new opens url" "$out" "title: form"
out=$(run tabs)
check "tabs lists both" "$out" "[1]"
out=$(run tab close)
check "tab close" "$out" "closed tab form"

out=$(run eval "document.title")
check "reconnects after active tab closed" "$out" "page two"

if ((failures > 0)); then
    echo "$failures e2e check(s) failed"
    exit 1
fi
echo "all e2e checks passed"
