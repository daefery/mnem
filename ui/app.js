// mnem viewer: live feed of observations, session summaries and prompts across
// Claude Code, Codex and pi. Markup mirrors the claude-mem viewer so its stylesheet
// applies unchanged.
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const PAGE = 40;
  const state = {
    project: "",
    query: "",
    items: [],
    keys: new Set(),
    before: null,
    newest: 0,
    loading: false,
    hasMore: true,
    generation: 0,
  };

  // ---------- helpers ----------

  function el(tag, attrs = {}, ...children) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (v === undefined || v === null || v === false) continue;
      if (k === "class") e.className = v;
      else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else e.setAttribute(k, v);
    }
    for (const c of children.flat()) {
      if (c === null || c === undefined || c === false) continue;
      e.append(c instanceof Node ? c : document.createTextNode(String(c)));
    }
    return e;
  }

  function svg(markup) {
    const t = document.createElement("template");
    t.innerHTML = markup.trim();
    return t.content.firstChild;
  }

  const fmtDate = (ms) => new Date(ms).toLocaleString();

  function shortProject(p) {
    if (!p) return "";
    if (p.startsWith("/")) return p.split("/").filter(Boolean).slice(-1)[0] || p;
    const [repo, checkout] = p.split("#");
    const name = repo.split("/").slice(-1)[0];
    return checkout ? `${name}#${checkout}` : name;
  }

  function stripRoot(path) {
    for (const m of ["/src/", "/crates/", "/packages/", "/apps/", "/docs/", "/tests/"]) {
      const i = path.indexOf(m);
      if (i !== -1) return path.slice(i + 1);
    }
    const parts = path.split("/");
    return parts.length > 3 ? parts.slice(-3).join("/") : path;
  }

  const list = (v) => (Array.isArray(v) ? v : []);

  // ---------- cards (same classes as claude-mem's React components) ----------

  function sourceBadge(src) {
    return el("span", { class: `card-source source-${src || "claude"}` }, src || "claude");
  }

  function observationCard(o) {
    let mode = "subtitle";
    const facts = list(o.facts);
    const concepts = list(o.concepts);
    const read = list(o.files_read).map(stripRoot);
    const modified = list(o.files_modified).map(stripRoot);
    const hasFacts = facts.length || concepts.length || read.length || modified.length;

    const content = el("div", { class: "view-mode-content" });
    const meta = el("div", { class: "card-meta" });
    const factsBtn = hasFacts
      ? el("button", { class: "view-mode-toggle", onclick: () => toggle("facts") },
          svg('<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 11 12 14 22 4"></polyline><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"></path></svg>'),
          el("span", {}, "facts"))
      : null;
    const narrBtn = o.narrative
      ? el("button", { class: "view-mode-toggle", onclick: () => toggle("narrative") },
          svg('<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"></path><polyline points="14 2 14 8 20 8"></polyline><line x1="16" y1="13" x2="8" y2="13"></line><line x1="16" y1="17" x2="8" y2="17"></line></svg>'),
          el("span", {}, "narrative"))
      : null;

    function render() {
      content.replaceChildren();
      meta.replaceChildren(el("span", { class: "meta-date" }, `#${o.id} • ${fmtDate(o.created_at_epoch)}`));
      factsBtn?.classList.toggle("active", mode === "facts");
      narrBtn?.classList.toggle("active", mode === "narrative");
      if (mode === "subtitle" && o.subtitle) content.append(el("div", { class: "card-subtitle" }, o.subtitle));
      if (mode === "facts" && facts.length) content.append(el("ul", { class: "facts-list" }, facts.map((f) => el("li", {}, f))));
      if (mode === "narrative") content.append(el("div", { class: "narrative" }, o.narrative));
      if (mode === "facts" && (concepts.length || read.length || modified.length)) {
        meta.append(el("div", { style: "display:flex;flex-wrap:wrap;gap:8px;align-items:center" },
          concepts.map((c) => el("span", {
            style: "padding:2px 8px;background:var(--color-type-badge-bg);color:var(--color-type-badge-text);border-radius:3px;font-weight:500;font-size:10px",
          }, c)),
          read.length ? el("span", { class: "meta-files" }, el("span", { class: "file-label" }, "read:"), " " + read.join(", ")) : null,
          modified.length ? el("span", { class: "meta-files" }, el("span", { class: "file-label" }, "modified:"), " " + modified.join(", ")) : null));
      }
    }
    function toggle(m) {
      mode = mode === m ? "subtitle" : m;
      render();
    }
    render();

    return el("div", { class: "card" },
      el("div", { class: "card-header" },
        el("div", { class: "card-header-left" },
          el("span", { class: `card-type type-${o.type}` }, o.type || "observation"),
          sourceBadge(o.platform_source),
          el("span", { class: "card-project", title: o.project }, shortProject(o.project))),
        el("div", { class: "view-mode-toggles" }, factsBtn, narrBtn)),
      el("div", { class: "card-title" }, o.title || "Untitled"),
      content,
      meta);
  }

  const SECTIONS = [
    ["investigated", "Investigated"],
    ["learned", "Learned"],
    ["completed", "Completed"],
    ["next_steps", "Next Steps"],
  ];

  function summaryCard(s) {
    const sections = SECTIONS.filter(([k]) => s[k]);
    return el("article", { class: "card summary-card" },
      el("header", { class: "summary-card-header" },
        el("div", { class: "summary-badge-row" },
          el("span", { class: "card-type summary-badge" }, "Session Summary"),
          sourceBadge(s.platform_source),
          el("span", { class: "summary-project-badge", title: s.project }, shortProject(s.project))),
        s.request ? el("h2", { class: "summary-title" }, s.request) : null),
      el("div", { class: "summary-sections" },
        sections.map(([k, label], i) =>
          el("section", { class: "summary-section", style: `animation-delay:${i * 50}ms` },
            el("div", { class: "summary-section-header" },
              el("img", { src: `/icons/icon-thick-${k.replace("_", "-")}.svg`, alt: label, class: `summary-section-icon summary-section-icon--${k}` }),
              el("h3", { class: "summary-section-label" }, label)),
            el("div", { class: "summary-section-content" }, s[k])))),
      el("footer", { class: "summary-card-footer" },
        el("span", { class: "summary-meta-id" }, `Session #${s.id}`),
        el("span", { class: "summary-meta-divider" }, "•"),
        el("time", { class: "summary-meta-date" }, fmtDate(s.created_at_epoch))));
  }

  function promptCard(p) {
    return el("div", { class: "card prompt-card" },
      el("div", { class: "card-header" },
        el("div", { class: "card-header-left" },
          el("span", { class: "card-type" }, "Prompt"),
          sourceBadge(p.platform_source),
          el("span", { class: "card-project", title: p.project }, shortProject(p.project)))),
      el("div", { class: "card-content" }, p.prompt_text),
      el("div", { class: "card-meta" }, el("span", { class: "meta-date" }, `E${p.id} • ${fmtDate(p.created_at_epoch)}`)));
  }

  const cardFor = (it) =>
    it.itemType === "observation" ? observationCard(it) : it.itemType === "summary" ? summaryCard(it) : promptCard(it);
  const keyOf = (it) => `${it.itemType}-${it.id}`;

  // ---------- data ----------

  function params(extra) {
    const q = new URLSearchParams({ limit: PAGE, ...extra });
    if (state.project) q.set("project", state.project);
    if (state.query) q.set("q", state.query);
    return q;
  }

  async function getJSON(url) {
    const r = await fetch(url);
    if (!r.ok) throw new Error(`${r.status} ${url}`);
    return r.json();
  }

  const feedContent = $("feed-content");
  const sentinel = el("div", { style: "height:20px;margin:10px 0" });
  const footer = el("div", { class: "mnem-empty" });

  function renderFooter() {
    footer.replaceChildren();
    if (state.loading) {
      footer.append(el("div", { class: "spinner", style: "display:inline-block;margin-right:10px" }), "Loading more...");
    } else if (!state.items.length) {
      footer.append(state.query ? "No matches" : "No items to display");
    } else if (!state.hasMore) {
      footer.append("No more items to load");
    }
  }

  async function loadMore() {
    if (state.loading || !state.hasMore) return;
    const gen = state.generation;
    state.loading = true;
    renderFooter();
    try {
      const extra = state.before !== null ? { before: state.before } : {};
      const data = await getJSON(`/api/feed?${params(extra)}`);
      if (gen !== state.generation) return;
      // First page of a search: the most relevant memories (keywords and meaning), then
      // the remaining matches newest first.
      if (data.best && data.best.length) {
        feedContent.insertBefore(el("div", { class: "mnem-section" }, "Best matches"), sentinel);
        for (const it of data.best) {
          state.keys.add(keyOf(it));
          state.items.push(it);
          feedContent.insertBefore(cardFor(it), sentinel);
        }
        feedContent.insertBefore(el("div", { class: "mnem-section" }, "Other matches, newest first"), sentinel);
      }
      for (const it of data.items) {
        const k = keyOf(it);
        if (state.keys.has(k)) continue;
        state.keys.add(k);
        state.items.push(it);
        feedContent.insertBefore(cardFor(it), sentinel);
        state.newest = Math.max(state.newest, it.created_at_epoch);
      }
      state.before = data.next_before;
      state.hasMore = data.next_before !== null && data.items.length > 0;
    } catch (e) {
      console.error(e);
      state.hasMore = false;
    } finally {
      if (gen === state.generation) {
        state.loading = false;
        renderFooter();
      }
    }
  }

  function reset() {
    state.generation++;
    state.items = [];
    state.keys = new Set();
    state.before = null;
    state.newest = 0;
    state.hasMore = true;
    state.loading = false;
    feedContent.replaceChildren(sentinel, footer);
    loadMore();
  }

  // New items arrive by polling; the logomark spins while they land.
  async function poll() {
    if (state.query || !state.newest) return;
    try {
      const data = await getJSON(`/api/feed?${params({ after: state.newest })}`);
      const fresh = data.items.filter((it) => !state.keys.has(keyOf(it)));
      if (!fresh.length) return;
      const logo = $("logomark");
      logo.classList.add("spinning");
      setTimeout(() => logo.classList.remove("spinning"), 1200);
      for (const it of fresh.sort((a, b) => a.created_at_epoch - b.created_at_epoch)) {
        state.keys.add(keyOf(it));
        state.items.unshift(it);
        feedContent.prepend(cardFor(it));
        state.newest = Math.max(state.newest, it.created_at_epoch);
      }
    } catch (e) {
      console.error(e);
    }
  }

  async function loadProjects() {
    const data = await getJSON("/api/projects");
    const sel = $("project");
    const ctx = $("context-project");
    for (const p of data.projects) {
      sel.append(el("option", { value: p.project }, `${shortProject(p.project)} (${p.count})`));
      ctx.append(el("option", { value: p.project }, shortProject(p.project)));
    }
    sel.value = state.project;
  }

  async function loadHealth() {
    try {
      const s = await getJSON("/api/stats");
      const h = $("health");
      const alerts = s.alerts || [];
      const behind = s.files_behind > 0 || alerts.length > 0;
      h.classList.toggle("behind", behind);
      $("health-label").textContent = alerts.length
        ? `${alerts.length} alert${alerts.length > 1 ? "s" : ""}`
        : s.files_behind > 0
          ? `${s.files_behind} behind`
          : `${s.memories.toLocaleString()} memories · ${s.sessions.toLocaleString()} sessions`;
      h.title =
        (alerts.length ? alerts.map((a) => `⚠ ${a}`).join("\n") + "\n" : "") +
        `Capture ${s.files_behind > 0 ? "behind" : "caught up"} · newest event ${s.newest_event_ago} ago · ` +
        `${s.events.toLocaleString()} events · ${s.pending_distill} session(s) awaiting distillation`;
    } catch (e) {
      console.error(e);
    }
  }

  // ---------- theme ----------

  const THEME_ICONS = {
    system: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"></rect><line x1="8" y1="21" x2="16" y2="21"></line><line x1="12" y1="17" x2="12" y2="21"></line></svg>',
    light: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="5"></circle><line x1="12" y1="1" x2="12" y2="3"></line><line x1="12" y1="21" x2="12" y2="23"></line><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"></line><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"></line><line x1="1" y1="12" x2="3" y2="12"></line><line x1="21" y1="12" x2="23" y2="12"></line><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"></line><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"></line></svg>',
    dark: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"></path></svg>',
  };
  const store = {
    get: (k) => { try { return localStorage.getItem(k); } catch { return null; } },
    set: (k, v) => { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
  };

  function applyTheme(pref) {
    if (pref === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", pref);
    const btn = $("theme");
    btn.replaceChildren(svg(THEME_ICONS[pref]));
    btn.title = `Theme: ${pref}`;
    store.set("mnem-theme", pref);
  }

  // ---------- context preview ----------

  async function showContext(project) {
    $("context-modal").style.display = "flex";
    const sel = $("context-project");
    const target = project || sel.value || (sel.options[0] && sel.options[0].value) || "";
    sel.value = target;
    $("context-text").textContent = "Loading…";
    try {
      const r = await fetch(`/api/context?project=${encodeURIComponent(target)}`);
      $("context-text").textContent = await r.text();
    } catch (e) {
      $("context-text").textContent = String(e);
    }
  }

  // ---------- wiring ----------

  function init() {
    const url = new URL(location.href);
    state.project = url.searchParams.get("project") || "";
    state.query = url.searchParams.get("q") || "";
    $("search").value = state.query;

    let pref = store.get("mnem-theme") || "system";
    applyTheme(pref);
    $("theme").addEventListener("click", () => {
      const cycle = ["system", "light", "dark"];
      pref = cycle[(cycle.indexOf(pref) + 1) % cycle.length];
      applyTheme(pref);
    });

    const syncUrl = () => {
      const u = new URL(location.href);
      state.project ? u.searchParams.set("project", state.project) : u.searchParams.delete("project");
      state.query ? u.searchParams.set("q", state.query) : u.searchParams.delete("q");
      history.replaceState(null, "", u);
    };
    $("project").addEventListener("change", (e) => {
      state.project = e.target.value;
      syncUrl();
      reset();
    });
    let t;
    $("search").addEventListener("input", (e) => {
      clearTimeout(t);
      t = setTimeout(() => {
        state.query = e.target.value.trim();
        syncUrl();
        reset();
      }, 250);
    });

    $("context-btn").addEventListener("click", () => showContext(state.project));
    $("context-close").addEventListener("click", () => ($("context-modal").style.display = "none"));
    $("context-modal").addEventListener("click", (e) => {
      if (e.target.id === "context-modal") e.target.style.display = "none";
    });
    $("context-project").addEventListener("change", (e) => showContext(e.target.value));
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") $("context-modal").style.display = "none";
      if (e.key === "/" && document.activeElement !== $("search")) {
        e.preventDefault();
        $("search").focus();
      }
    });

    const feed = $("feed");
    const toTop = $("to-top");
    feed.addEventListener("scroll", () => (toTop.style.display = feed.scrollTop > 600 ? "flex" : "none"));
    toTop.addEventListener("click", () => feed.scrollTo({ top: 0, behavior: "smooth" }));

    new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting) loadMore();
    }, { root: feed, rootMargin: "400px" }).observe(sentinel);

    loadProjects().catch(console.error);
    loadHealth();
    reset();
    setInterval(poll, 4000);
    setInterval(loadHealth, 15000);
  }

  init();
})();
