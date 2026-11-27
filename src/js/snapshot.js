((opts) => {
  const INTERACTIVE_TAGS = new Set(["A", "BUTTON", "INPUT", "TEXTAREA", "SELECT", "SUMMARY"]);
  const INTERACTIVE_ROLES = new Set([
    "button", "link", "textbox", "searchbox", "combobox", "checkbox",
    "radio", "tab", "menuitem", "option", "switch"
  ]);

  const vw = window.innerWidth || document.documentElement.clientWidth;
  const vh = window.innerHeight || document.documentElement.clientHeight;
  const filter = opts.filter ? opts.filter.toLowerCase() : null;
  const maxElements = opts.maxElements || 150;
  const maxTextChars = opts.maxTextChars || 4000;

  const clean = (s, maxLen = 80) => {
    if (!s) return "";
    const c = String(s).replace(/\s+/g, " ").trim();
    return c.length > maxLen ? c.slice(0, maxLen - 1) + "…" : c;
  };

  const isVisible = (el, checkViewport) => {
    const style = window.getComputedStyle(el);
    if (!style || style.display === "none" || style.visibility === "hidden" || parseFloat(style.opacity) === 0) {
      return false;
    }
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return false;
    if (checkViewport) {
      const margin = 150;
      if (rect.bottom < -margin || rect.top > vh + margin || rect.right < -margin || rect.left > vw + margin) {
        return false;
      }
    }
    return true;
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

  const getKind = (el) => {
    const tag = el.tagName.toLowerCase();
    if (tag === "input") return "input:" + (el.type || "text");
    if (tag === "a") return "link";
    if (tag === "button") return "btn";
    if (tag === "textarea") return "textarea";
    if (tag === "select") return "select";
    if (el.isContentEditable) return "edit";
    const role = (el.getAttribute("role") || "").toLowerCase();
    return role || tag;
  };

  const getLabel = (el) => {
    const aria = el.getAttribute("aria-label");
    if (aria) return clean(aria);
    const labelledBy = el.getAttribute("aria-labelledby");
    if (labelledBy) {
      const refEl = document.getElementById(labelledBy);
      if (refEl) return clean(refEl.innerText || refEl.textContent);
    }
    if (el.id) {
      const lbl = document.querySelector(`label[for="${CSS.escape(el.id)}"]`);
      if (lbl) return clean(lbl.innerText || lbl.textContent);
    }
    const ph = el.getAttribute("placeholder");
    if (ph) return clean(ph);
    const title = el.getAttribute("title");
    if (title) return clean(title);
    const alt = el.getAttribute("alt");
    if (alt) return clean(alt);
    return clean(el.innerText || el.textContent || el.value || "");
  };

  const getVal = (el) => {
    const tag = el.tagName;
    if (tag === "INPUT") {
      if (el.type === "checkbox" || el.type === "radio") return el.checked ? "checked" : "unchecked";
      if (el.type === "password") return el.value ? "***" : "";
      return clean(el.value, 60);
    }
    if (tag === "TEXTAREA") return clean(el.value, 60);
    if (tag === "SELECT") {
      const opt = el.options && el.options[el.selectedIndex];
      return opt ? clean(opt.text, 60) : clean(el.value, 60);
    }
    if (el.isContentEditable) return clean(el.innerText, 60);
    return "";
  };

  const refs = new Map();
  const elements = [];
  const headings = [];
  let totalInteractive = 0;
  let refCounter = 1;

  const walk = (root) => {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    let node = walker.nextNode();
    while (node) {
      const tag = node.tagName;
      if (tag === "H1" || tag === "H2" || tag === "H3") {
        if (isVisible(node, !opts.all)) {
          const h = clean(node.innerText || node.textContent, 100);
          if (h && headings.length < 10) headings.push(h);
        }
      }
      if (isInteractive(node) && isVisible(node, !opts.all)) {
        const kind = getKind(node);
        const label = getLabel(node);
        const val = getVal(node);
        const href = tag === "A" ? clean(node.getAttribute("href") || "", 80) : "";
        const disabled = Boolean(node.disabled || node.getAttribute("aria-disabled") === "true");

        const haystack = `${kind} ${label} ${val} ${href}`.toLowerCase();
        if (!filter || haystack.includes(filter)) {
          totalInteractive++;
          if (elements.length < maxElements) {
            const id = refCounter++;
            refs.set(id, node);
            elements.push({ id, kind, label, val, href, disabled });
          }
        }
      }
      if (node.shadowRoot) walk(node.shadowRoot);
      node = walker.nextNode();
    }
  };

  if (document.body) walk(document.body);
  window.__surfRefs = refs;

  let rawText = document.body ? (document.body.innerText || "") : "";
  rawText = rawText.replace(/[ \t]+/g, " ").replace(/\n{3,}/g, "\n\n").trim();
  const totalTextChars = rawText.length;
  const textTruncated = totalTextChars > maxTextChars;
  const text = textTruncated ? rawText.slice(0, maxTextChars) : rawText;

  return {
    url: location.href,
    title: document.title || "",
    totalInteractive,
    shownInteractive: elements.length,
    elements,
    headings,
    text,
    textTruncated,
    totalTextChars
  };
})
