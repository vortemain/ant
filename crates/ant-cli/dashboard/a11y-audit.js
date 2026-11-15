// WCAG 2.1 AA spot-audit for the dashboard (D-044). Not served: paste it
// into the browser's developer console (or a browser-automation
// `evaluate`) on each page, in light and dark, after UI changes. It checks
// text contrast against the real background, accessible names, image alt
// text, heading order, landmarks, duplicate ids, positive tabindex, lang,
// title, and table headers, and returns the issues it finds.
(() => {
  const issues = [];
  const parse = (c) => {
    const m = c.match(/rgba?\(([^)]+)\)/);
    if (!m) return null;
    const p = m[1].split(/[ ,/]+/).filter(Boolean).map(Number);
    return { r: p[0], g: p[1], b: p[2], a: p.length > 3 ? p[3] : 1 };
  };
  const lum = ({ r, g, b }) => {
    const f = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; };
    return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
  };
  const blend = (top, bottom) => ({
    r: top.r * top.a + bottom.r * (1 - top.a),
    g: top.g * top.a + bottom.g * (1 - top.a),
    b: top.b * top.a + bottom.b * (1 - top.a),
    a: 1,
  });
  const background = (el) => {
    const layers = [];
    for (let n = el; n; n = n.parentElement) {
      const cs = getComputedStyle(n);
      if (cs.backgroundImage && cs.backgroundImage !== "none") return null; // gradient: skip
      const c = parse(cs.backgroundColor);
      if (c && c.a > 0) { layers.push(c); if (c.a >= 1) break; }
    }
    let color = { r: 255, g: 255, b: 255, a: 1 };
    for (let i = layers.length - 1; i >= 0; i -= 1) color = blend(layers[i], color);
    return color;
  };
  const visible = (el) => {
    const r = el.getBoundingClientRect();
    const cs = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && cs.visibility !== "hidden" && cs.display !== "none" && !el.closest("[hidden]") && !el.closest(".sr-only");
  };
  // 1. Contrast of visible text.
  const seen = new Set();
  for (const el of document.querySelectorAll("body *")) {
    if (!visible(el)) continue;
    const text = [...el.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent.trim()).join("");
    if (!text) continue;
    const cs = getComputedStyle(el);
    const fg = parse(cs.color);
    const bg = background(el);
    if (!fg || !bg) continue;
    const color = blend(fg, bg);
    const [l1, l2] = [lum(color), lum(bg)].sort((a, b) => b - a);
    const ratio = (l1 + 0.05) / (l2 + 0.05);
    const size = parseFloat(cs.fontSize);
    const bold = Number(cs.fontWeight) >= 700;
    const large = size >= 24 || (bold && size >= 18.66);
    const need = large ? 3 : 4.5;
    if (ratio < need) {
      const key = `${el.className}|${cs.color}`;
      if (!seen.has(key)) {
        seen.add(key);
        issues.push(`contrast ${ratio.toFixed(2)} < ${need}: <${el.tagName.toLowerCase()} class="${el.className}"> "${text.slice(0, 30)}"`);
      }
    }
  }
  // 2. Names for controls.
  const name = (el) => (el.getAttribute("aria-label") || el.getAttribute("aria-labelledby") && document.getElementById(el.getAttribute("aria-labelledby"))?.textContent || el.textContent || el.title || (el.labels && el.labels[0]?.textContent) || "").trim();
  for (const el of document.querySelectorAll("button, a[href], input, select, [role=button], [role=progressbar]")) {
    if (!visible(el) && el.tagName !== "INPUT") continue;
    if (!name(el)) issues.push(`no accessible name: <${el.tagName.toLowerCase()} class="${el.className}" id="${el.id}">`);
  }
  // 3. Images.
  for (const img of document.querySelectorAll("img")) if (!img.hasAttribute("alt")) issues.push(`img without alt: ${img.src}`);
  // 4. Headings.
  const headings = [...document.querySelectorAll("h1,h2,h3,h4,h5,h6")].filter(visible).map((h) => Number(h.tagName[1]));
  if (headings.filter((h) => h === 1).length !== 1) issues.push(`expected one h1, found ${headings.filter((h) => h === 1).length}`);
  for (let i = 1; i < headings.length; i += 1) if (headings[i] > headings[i - 1] + 1) issues.push(`heading jumps h${headings[i - 1]} -> h${headings[i]}`);
  // 5. Landmarks.
  for (const sel of ["main", "nav", "header", "aside"]) if (!document.querySelector(sel)) issues.push(`missing <${sel}> landmark`);
  // 6. Duplicate ids, positive tabindex, lang, title.
  const ids = {};
  for (const el of document.querySelectorAll("[id]")) ids[el.id] = (ids[el.id] || 0) + 1;
  for (const [id, n] of Object.entries(ids)) if (n > 1) issues.push(`duplicate id "${id}" x${n}`);
  for (const el of document.querySelectorAll("[tabindex]")) if (Number(el.getAttribute("tabindex")) > 0) issues.push(`positive tabindex on ${el.tagName}`);
  if (!document.documentElement.lang) issues.push("html lang missing");
  if (!document.title) issues.push("title missing");
  // 7. Tables.
  for (const t of document.querySelectorAll("table")) if (visible(t) && !t.querySelector("th")) issues.push(`table without headers: #${t.id}`);
  return { page: location.hash || "#overview", theme: document.documentElement.dataset.theme || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark(system)" : "light(system)"), issues };
})();
