use crate::protocol::SnapOptions;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

const SNAPSHOT_JS: &str = include_str!("js/snapshot.js");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSnapshot {
    pub url: String,
    pub title: String,
    pub total_interactive: usize,
    pub shown_interactive: usize,
    pub elements: Vec<RawElement>,
    pub headings: Vec<String>,
    pub text: String,
    pub text_truncated: bool,
    pub total_text_chars: usize,
}

#[derive(Debug, Deserialize)]
pub struct RawElement {
    pub id: u32,
    pub kind: String,
    pub label: String,
    pub val: String,
    pub href: String,
    pub disabled: bool,
}

pub fn build_snapshot_expr(opts: &SnapOptions) -> String {
    let args = json!({
        "all": opts.all,
        "filter": opts.filter,
        "maxElements": opts.max_elements,
        "maxTextChars": opts.max_text_chars,
    });
    format!("({SNAPSHOT_JS})({args})")
}

pub fn parse_snapshot(val: Value) -> Result<RawSnapshot> {
    serde_json::from_value(val).context("failed to decode snapshot payload")
}

pub fn render_snapshot(snap: &RawSnapshot, opts: &SnapOptions) -> String {
    let mut out = render_interactive(snap);
    append_text(&mut out, snap, opts);
    out
}

pub fn render_interactive(snap: &RawSnapshot) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str(&format!("url: {}\n", snap.url));
    if !snap.title.is_empty() {
        out.push_str(&format!("title: {}\n", snap.title));
    }
    if !snap.headings.is_empty() {
        out.push_str(&format!("headings: {}\n", snap.headings.join(" | ")));
    }
    out.push_str("---\n");

    if snap.elements.is_empty() {
        out.push_str("(no visible interactive elements)\n");
    } else {
        for el in &snap.elements {
            out.push_str(&format!("@{} [{}]", el.id, el.kind));
            if !el.label.is_empty() {
                out.push_str(&format!(" \"{}\"", el.label));
            }
            if !el.val.is_empty() {
                out.push_str(&format!(" val=\"{}\"", el.val));
            }
            if !el.href.is_empty() {
                out.push_str(&format!(" -> {}", el.href));
            }
            if el.disabled {
                out.push_str(" (disabled)");
            }
            out.push('\n');
        }
    }

    if snap.shown_interactive < snap.total_interactive {
        out.push_str(&format!(
            "[truncated: showing {}/{} interactive elements; pass --max-elements {} or --filter \"query\"]\n",
            snap.shown_interactive, snap.total_interactive, snap.total_interactive
        ));
    }

    out
}

fn append_text(out: &mut String, snap: &RawSnapshot, opts: &SnapOptions) {
    if !snap.text.is_empty() && opts.filter.is_none() {
        out.push_str("---\n");
        out.push_str(&snap.text);
        out.push('\n');
        if snap.text_truncated {
            out.push_str(&format!(
                "[truncated: showing {}/{} text chars; pass --max-text-chars {}]\n",
                snap.text.len(),
                snap.total_text_chars,
                snap.total_text_chars
            ));
        }
    }
}

pub fn diff_snapshots(prev: &str, curr: &str) -> String {
    if prev == curr {
        return "(no visible changes)\n".to_string();
    }

    let prev_lines: Vec<&str> = prev.lines().collect();
    let curr_lines: Vec<&str> = curr.lines().collect();

    let prev_set: HashSet<&str> = prev_lines.iter().copied().collect();
    let curr_set: HashSet<&str> = curr_lines.iter().copied().collect();

    let mut out = String::new();

    let prev_url = prev_lines.iter().find(|l| l.starts_with("url: "));
    let curr_url = curr_lines.iter().find(|l| l.starts_with("url: "));
    if prev_url != curr_url
        && let Some(u) = curr_url
    {
        out.push_str(u);
        out.push('\n');
    }

    let mut changes = 0usize;
    for line in &prev_lines {
        if *line == "---" || line.starts_with("url: ") {
            continue;
        }
        if !curr_set.contains(line) {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
            changes += 1;
        }
    }
    for line in &curr_lines {
        if *line == "---" || line.starts_with("url: ") {
            continue;
        }
        if !prev_set.contains(line) {
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
            changes += 1;
        }
    }

    if changes == 0 && out.is_empty() {
        "(no visible changes)\n".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_compact_snapshot_and_truncation_notice() {
        let snap = RawSnapshot {
            url: "https://example.com".into(),
            title: "Example".into(),
            total_interactive: 5,
            shown_interactive: 2,
            elements: vec![
                RawElement {
                    id: 1,
                    kind: "input:text".into(),
                    label: "Email".into(),
                    val: "".into(),
                    href: "".into(),
                    disabled: false,
                },
                RawElement {
                    id: 2,
                    kind: "btn".into(),
                    label: "Sign in".into(),
                    val: "".into(),
                    href: "".into(),
                    disabled: false,
                },
            ],
            headings: vec!["Login".into()],
            text: "Welcome".into(),
            text_truncated: true,
            total_text_chars: 100,
        };

        let rendered = render_snapshot(&snap, &SnapOptions::default());
        assert!(rendered.contains("@1 [input:text] \"Email\""));
        assert!(rendered.contains("@2 [btn] \"Sign in\""));
        assert!(rendered.contains("showing 2/5 interactive elements"));
        assert!(rendered.contains("showing 7/100 text chars"));

        let interactive = render_interactive(&snap);
        assert!(interactive.contains("@2 [btn] \"Sign in\""));
        assert!(!interactive.contains("Welcome"));
    }

    #[test]
    fn diffs_only_changed_lines() {
        let prev = "url: https://a.com\n---\n@1 [btn] \"Open\"\n@2 [link] \"Home\"\n";
        let curr = "url: https://a.com\n---\n@1 [btn] \"Close\"\n@2 [link] \"Home\"\n@3 [link] \"Settings\"\n";
        let d = diff_snapshots(prev, curr);
        assert!(d.contains("- @1 [btn] \"Open\""));
        assert!(d.contains("+ @1 [btn] \"Close\""));
        assert!(d.contains("+ @3 [link] \"Settings\""));
        assert!(!d.contains("Home"));
    }

    #[test]
    fn identical_snapshots_return_no_changes() {
        let s = "url: https://a.com\n---\n@1 [btn] \"Go\"\n";
        assert_eq!(diff_snapshots(s, s), "(no visible changes)\n");
    }
}
