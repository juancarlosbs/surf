((opts) => {
  const INTERACTIVE_TAGS = new Set(["A", "BUTTON", "INPUT", "TEXTAREA", "SELECT", "SUMMARY"]);
  const INTERACTIVE_ROLES = new Set([
    "button", "link", "textbox", "searchbox", "combobox", "checkbox",
    "radio", "tab", "menuitem", "option", "switch"
  ]);
  const NON_TEXT_INPUT_TYPES = new Set([
    "hidden", "submit", "button", "reset", "image", "checkbox", "radio"
  ]);

  const norm = (s) => String(s || "").replace(/\s+/g, " ").trim().toLowerCase();
  const q = norm(opts.query);
  if (!q) return { found: false };

  const mode = opts.mode || "click";
  const vw = window.innerWidth || document.documentElement.clientWidth;
  const vh = window.innerHeight || document.documentElement.clientHeight;

  const isVisible = (el) => {
    const style = window.getComputedStyle(el);
    if (!style || style.display === "none" || style.visibility === "hidden" || parseFloat(style.opacity) === 0) {
      return false;
    }
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };

  const isInViewport = (el) => {
    const r = el.getBoundingClientRect();
    const margin = 150;
    return !(r.bottom < -margin || r.top > vh + margin || r.right < -margin || r.left > vw + margin);
  };

  const isInteractive = (el) => {
    const tag = el.tagName;
    if (tag === "INPUT" && el.type === "hidden") return false;
    if (tag === "A" && !el.hasAttribute("href") && !el.onclick) return false;
    if (INTERACTIVE_TAGS.has(tag)) return true;
    const role = (el.getAttribute("role") || "").toLowerCase();
    if (INTERACTIVE_ROLES.has(role)) return true;
    if (el.isContentEditable) return true;
    if (el.hasAttribute("onclick")) return true;
    return false;
  };

  const isEditable = (el) => {
    const tag = el.tagName;
    if (tag === "TEXTAREA") return true;
    if (tag === "INPUT") {
      const t = (el.type || "text").toLowerCase();
      return !NON_TEXT_INPUT_TYPES.has(t);
    }
    if (el.isContentEditable) return true;
    const role = (el.getAttribute("role") || "").toLowerCase();
    return role === "textbox" || role === "searchbox" || role === "combobox";
  };

  const getCandidates = (el) => {
    const out = [];
    const push = (val, bonus) => {
      const t = norm(val);
      if (t) out.push({ text: t, bonus });
    };

    push(el.getAttribute("aria-label"), 12);

    const labelledBy = el.getAttribute("aria-labelledby");
    if (labelledBy) {
      const parts = labelledBy
        .split(/\s+/)
        .map((id) => document.getElementById(id))
        .filter(Boolean)
        .map((refEl) => refEl.innerText || refEl.textContent || "");
      push(parts.join(" "), 12);
    }

    push(el.getAttribute("placeholder"), 12);
    push(el.getAttribute("aria-placeholder"), 12);
    push(el.getAttribute("data-placeholder"), 12);

    if (el.id) {
      const lbl = document.querySelector(`label[for="${CSS.escape(el.id)}"]`);
      if (lbl) push(lbl.innerText || lbl.textContent, 12);
    }
    const parentLabel = el.closest && el.closest("label");
    if (parentLabel) {
      push(parentLabel.innerText || parentLabel.textContent, 10);
    }

    push(el.getAttribute("title"), 8);
    push(el.getAttribute("alt"), 8);

    if (mode === "fill" && el.isContentEditable) {
      const root = el.closest('[class*="DraftEditor"], [role="presentation"]') || el.parentElement;
      if (root) {
        const phEl = root.querySelector('[class*="placeholder" i], [data-placeholder]');
        if (phEl) push(phEl.innerText || phEl.textContent || phEl.getAttribute("data-placeholder"), 10);
      }
    }

    if (el.tagName === "INPUT" && (el.type === "submit" || el.type === "button")) {
      push(el.value, 6);
    }
    push(el.innerText || el.textContent, 0);

    return out;
  };

  let bestEl = null;
  let bestScore = -1;

  const scoreElement = (el) => {
    const interactive = isInteractive(el);
    if (mode === "fill") {
      if (!isEditable(el)) return -1;
    } else if (!interactive) {
      // Allow non-interactive elements only if they are small leaf-ish elements with matching text
      if (el.children.length > 3) return -1;
    }

    if (!isVisible(el)) return -1;

    const disabled = Boolean(el.disabled || el.getAttribute("aria-disabled") === "true");
    const inVp = isInViewport(el);
    const candidates = getCandidates(el);

    let elBest = -1;
    for (const c of candidates) {
      let matchScore = 0;
      if (c.text === q) {
        matchScore = 100;
      } else if (c.text.startsWith(q)) {
        matchScore = 75;
      } else if (c.text.includes(q)) {
        matchScore = 50;
      } else {
        continue;
      }

      const lenPenalty = Math.min(Math.max(c.text.length - q.length, 0), 40) * 0.25;
      const total =
        matchScore +
        c.bonus +
        (inVp ? 5 : 0) +
        (interactive ? 20 : 0) +
        (disabled ? 0 : 15) -
        lenPenalty;

      if (total > elBest) elBest = total;
    }
    return elBest;
  };

  const walk = (root) => {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    let node = walker.nextNode();
    while (node) {
      const s = scoreElement(node);
      if (s > bestScore) {
        bestScore = s;
        bestEl = node;
      }
      if (node.shadowRoot) walk(node.shadowRoot);
      node = walker.nextNode();
    }
  };

  if (document.body) walk(document.body);
  if (!bestEl) return { found: false };

  bestEl.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
  const r = bestEl.getBoundingClientRect();
  const x = r.left + r.width / 2;
  const y = r.top + r.height / 2;

  if (mode === "click") {
    return { found: true, x, y };
  }

  const isContentEditable = Boolean(bestEl.isContentEditable);
  const hasContent = Boolean(
    (bestEl.value && bestEl.value.length) ||
    (isContentEditable && bestEl.innerText && bestEl.innerText.trim().length)
  );

  if (isContentEditable) {
    window.__surfActiveEditable = bestEl;
  } else {
    bestEl.focus();
    if ("select" in bestEl && typeof bestEl.select === "function") {
      bestEl.select();
    }
  }

  return { found: true, x, y, isContentEditable, hasContent };
})
