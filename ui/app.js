// mnem viewer: filters, a list of memories and prompts, and the selected memory in
// full, including what only mnem knows about it: whether the code it describes is
// still there, and whether agents it was offered to opened it. Reading a memory here
// is never recorded as an agent using it.
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const PAGE = 50;
  const state = {
    project: "",
    query: "",
    types: new Set(),
    agent: "",
    view: "",
    items: [],
    keys: new Set(),
    before: null,
    offset: 0,
    newest: null,
    error: null,
    loading: false,
    hasMore: true,
    generation: 0,
    selected: null,
    detailGen: 0,
  };

  // ---------- helpers ----------

  function el(tag, attrs = {}, ...children) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (v === undefined || v === null || v === false) continue;
      if (k === "class") e.className = v;
      // setProperty, not assignment: custom properties (--x) are ignored when assigned.
      else if (k === "style" && typeof v === "object") for (const [p, x] of Object.entries(v)) e.style.setProperty(p.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`), x);
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
  const fmtTime = (ms) => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const dayKey = (ms) => new Date(ms).toDateString();
  function dayLabel(ms) {
    const d = new Date(ms);
    const today = new Date();
    const y = new Date(today);
    y.setDate(today.getDate() - 1);
    const base = d.toLocaleDateString([], { day: "numeric", month: "short", year: d.getFullYear() === today.getFullYear() ? undefined : "numeric" });
    if (d.toDateString() === today.toDateString()) return `Today · ${base}`;
    if (d.toDateString() === y.toDateString()) return `Yesterday · ${base}`;
    return d.toLocaleDateString([], { weekday: "short" }) + ` · ${base}`;
  }

  function shortProject(p) {
    if (!p) return "(no project)";
    if (p.startsWith("/")) return p.split("/").filter(Boolean).slice(-1)[0] || p;
    const [repo, checkout] = p.split("#");
    const name = repo.split("/").slice(-1)[0];
    return checkout ? `${name}#${checkout}` : name;
  }

  function stripRoot(path) {
    for (const m of ["/src/", "/crates/", "/packages/", "/apps/", "/docs/", "/tests/", "/ui/"]) {
      const i = path.indexOf(m);
      if (i !== -1) return path.slice(i + 1);
    }
    const parts = path.split("/");
    return parts.length > 3 ? parts.slice(-3).join("/") : path;
  }

  const list = (v) => (Array.isArray(v) ? v : []);

  // ---------- types and agents ----------

  const TYPES = [
    ["decision", "decisions"],
    ["change", "changes"],
    ["feature", "features"],
    ["bugfix", "bugfixes"],
    ["discovery", "discoveries"],
    ["refactor", "refactors"],
    ["security_note,security_alert", "security"],
    ["summary", "summaries"],
    ["prompt", "prompts"],
  ];
  const AGENTS = ["claude", "codex", "pi"];

  function typeColor(t) {
    if (!t) return "var(--t-other)";
    if (t.startsWith("security")) return "var(--t-security)";
    const known = ["decision", "change", "feature", "bugfix", "discovery", "refactor", "summary", "prompt", "pinned"];
    return known.includes(t) ? `var(--t-${t})` : "var(--t-other)";
  }
  const typeLabel = (t) => (t || "observation").replace("_", " ");
  const itemType = (it) => (it.pinned ? "pinned" : it.itemType === "observation" ? it.type : it.itemType);
  // Pinned facts are the user's, not any agent's.
  const agentOf = (it) => (it.pinned ? null : it.platform_source);
  // A pinned fact's text is its narrative; its title is the same text, cut.
  const firstLine = (s, n = 140) => {
    const t = (s || "").trim().split("\n")[0];
    return t.length > n ? `${t.slice(0, n - 1)}…` : t;
  };

  function typeMark(t) {
    return el("span", { class: "type", style: { "--tc": typeColor(t) } }, typeLabel(t));
  }
  const agentBadge = (a) => el("span", { class: `agent ${a || ""}` }, a || "?");

  // ---------- views ----------

  const ICON = {
    all: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="8" y1="6" x2="21" y2="6"></line><line x1="8" y1="12" x2="21" y2="12"></line><line x1="8" y1="18" x2="21" y2="18"></line><circle cx="4" cy="6" r="1"></circle><circle cx="4" cy="12" r="1"></circle><circle cx="4" cy="18" r="1"></circle></svg>',
    pinned: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 17v5"></path><path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z"></path></svg>',
    unopened: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9.88 9.88a3 3 0 1 0 4.24 4.24"></path><path d="M10.73 5.08A10.43 10.43 0 0 1 12 5c7 0 10 7 10 7a13.16 13.16 0 0 1-1.67 2.68"></path><path d="M6.61 6.61A13.53 13.53 0 0 0 2 12s3 7 10 7a9.74 9.74 0 0 0 5.39-1.61"></path><line x1="2" y1="2" x2="22" y2="22"></line></svg>',
    edited: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"></polyline><polyline points="8 6 2 12 8 18"></polyline></svg>',
  };
  const VIEWS = [
    ["", "Everything", ICON.all, ""],
    ["pinned", "Pinned facts", ICON.pinned, "Facts every agent sees at session start."],
    ["unopened", "Never opened", ICON.unopened, "Offered to agents, never fetched in full."],
    ["edited", "Changed code", ICON.edited, "Their session edited files: open one to see if the code is still there."],
  ];

  // ---------- list ----------

  const listEl = $("list");
  const detailEl = $("detail");
  const sentinel = el("div", { style: { height: "1px" } });
  const foot = el("div", { class: "list-foot" });
  const keyOf = (it) => `${it.itemType}-${it.id}`;
  let lastDay = null;

  const MATCH = { words: "words", meaning: "meaning", both: "words + meaning" };

  function row(it) {
    const t = itemType(it);
    let title, sub;
    if (it.itemType === "prompt") {
      title = `“${(it.prompt_text || "").trim()}”`;
    } else if (it.itemType === "summary") {
      title = it.request || "Session summary";
      sub = it.completed || it.learned;
    } else if (it.pinned) {
      title = firstLine(it.narrative || it.title);
    } else {
      title = it.title || "Untitled";
      sub = it.subtitle;
    }
    const meta = el("div", { class: "item-meta" },
      typeMark(t),
      agentOf(it) ? agentBadge(agentOf(it)) : null,
      state.project ? null : el("span", { class: "proj", title: it.project || "" }, shortProject(it.project)),
      it.match ? el("span", { class: "match", title: "What matched your search" }, MATCH[it.match] || it.match) : null,
      el("span", { class: "when", title: fmtDate(it.created_at_epoch) }, fmtTime(it.created_at_epoch)));
    const b = el("button", {
      class: `item ${it.itemType}`,
      role: "option",
      tabindex: "-1",
      "data-key": keyOf(it),
      id: `opt-${keyOf(it)}`,
      onclick: () => select(it),
    }, meta, el("div", { class: "item-title" }, title), sub ? el("div", { class: "item-sub" }, sub) : null);
    b._item = it;
    return b;
  }

  // Add an item at the end of the list, under a day header in time order (search
  // results are in relevance order and get none).
  function append(it) {
    if (!state.query) {
      const k = dayKey(it.created_at_epoch);
      if (k !== lastDay) {
        lastDay = k;
        listEl.insertBefore(dayHeader(it.created_at_epoch), sentinel);
      }
    }
    listEl.insertBefore(row(it), sentinel);
  }

  function renderFoot() {
    foot.replaceChildren();
    if (state.loading) foot.append(el("span", { class: "spinner" }), "Loading…");
    else if (state.error) foot.append(`Could not load more (${state.error}). `, el("button", { class: "btn", onclick: () => reset() }, "Retry"));
    else if (!state.items.length) foot.append(state.query ? "No matches." : state.view === "unopened" ? "Nothing here: every offered memory was opened, or none were offered yet." : "Nothing here yet.");
    else if (!state.hasMore) foot.append(`${state.items.length.toLocaleString()} shown · that is all`);
  }

  function params(extra) {
    const q = new URLSearchParams({ limit: PAGE, ...extra });
    if (state.project) q.set("project", state.project);
    if (state.query) q.set("q", state.query);
    if (state.types.size) q.set("type", [...state.types].join(","));
    if (state.agent) q.set("agent", state.agent);
    if (state.view) q.set("view", state.view);
    return q;
  }

  async function getJSON(url) {
    const r = await fetch(url);
    if (!r.ok) throw new Error(`${r.status} ${url}`);
    return r.json();
  }

  async function loadMore() {
    if (state.loading || !state.hasMore) return;
    const gen = state.generation;
    state.loading = true;
    renderFoot();
    try {
      // Searches page by rank (offset); the plain feed pages by time (before).
      const extra = state.query ? { offset: state.offset } : state.before !== null ? { before: state.before } : {};
      const data = await getJSON(`/api/feed?${params(extra)}`);
      if (gen !== state.generation) return;
      for (const it of data.items) {
        const k = keyOf(it);
        if (state.keys.has(k)) continue;
        state.keys.add(k);
        state.items.push(it);
        append(it);
      }
      // Live updates continue after the newest item of the first page.
      // An empty feed polls from before every place, time 0 included.
      if (state.newest === null && !state.query) state.newest = data.newest ?? "0.-1.0";
      if (state.query) {
        state.offset = data.next_offset ?? state.offset;
        state.hasMore = data.next_offset != null && data.items.length > 0;
      } else {
        state.before = data.next_before;
        state.hasMore = data.next_before !== null && data.items.length > 0;
      }
      if (!state.selected) {
        // A #id link is a request to read that memory (on a phone too); otherwise the
        // newest one is shown beside the list without taking the phone's screen.
        const want = /^\d+$/.test(location.hash.slice(1)) ? location.hash.slice(1) : "";
        const target = want && state.items.find((it) => it.itemType !== "prompt" && `${it.id}` === want);
        if (target) select(target, { focus: false });
        else if (want) showById(Number(want));
        else {
          const first = state.items.find((it) => it.itemType !== "prompt") || state.items[0];
          if (first) select(first, { focus: false, read: false });
        }
      }
    } catch (e) {
      console.error(e);
      if (gen !== state.generation) return;
      state.hasMore = false;
      state.error = e.message;
    } finally {
      if (gen === state.generation) {
        state.loading = false;
        renderFoot();
        // Keep filling while the sentinel is still on screen.
        requestAnimationFrame(() => {
          if (state.hasMore && sentinel.getBoundingClientRect().top < listEl.getBoundingClientRect().bottom + 400) loadMore();
        });
      }
    }
  }

  function reset() {
    state.generation++;
    // A detail still loading from before belongs to the old list.
    state.detailGen++;
    document.body.classList.remove("reading");
    detailEl.replaceChildren(el("div", { class: "empty" }, "Select a memory to read it."));
    state.items = [];
    state.keys = new Set();
    state.before = null;
    state.offset = 0;
    state.newest = null;
    state.error = null;
    state.hasMore = true;
    state.loading = false;
    state.selected = null;
    lastDay = null;
    listEl.replaceChildren(sentinel, foot);
    listEl.scrollTop = 0;
    loadMore();
  }

  // New items arrive by polling; the logomark spins while they land.
  // Items are newest first under day headers; a new one goes under its own day's
  // header, which is added when the top of the list is an older day.
  let polling = false;
  async function poll() {
    if (polling || state.loading || state.query || state.newest === null || document.hidden) return;
    const gen = state.generation;
    polling = true;
    try {
      const data = await getJSON(`/api/feed?${params({ after: state.newest })}`);
      // The list was reset while this was in flight: these belong to the old filters.
      if (gen !== state.generation) return;
      // The page is newest first; oldest first here, so each lands on top of the last.
      const fresh = data.items.filter((it) => !state.keys.has(keyOf(it))).reverse();
      for (const it of fresh) {
        state.keys.add(keyOf(it));
        state.items.unshift(it);
        prependRow(it);
      }
      state.newest = data.newest ?? state.newest;
      if (fresh.length) {
        const logo = $("logomark");
        logo.classList.remove("spinning");
        void logo.offsetWidth;
        logo.classList.add("spinning");
      }
      // A full page means more are waiting.
      if (data.more) setTimeout(poll, 0);
    } catch (e) {
      console.error(e);
    } finally {
      polling = false;
    }
  }

  function prependRow(it) {
    const r = row(it);
    const top = listEl.firstElementChild;
    const k = dayKey(it.created_at_epoch);
    if (top && top.classList.contains("day") && top.dataset.day === k) {
      top.after(r);
    } else {
      // The older day's header stays above its own rows.
      listEl.prepend(dayHeader(it.created_at_epoch), r);
    }
    if (lastDay === null) lastDay = k;
  }

  function dayHeader(ms) {
    return el("div", { class: "day", role: "presentation", "data-day": dayKey(ms) }, dayLabel(ms));
  }

  // ---------- selection and keyboard ----------

  function rows() {
    return [...listEl.querySelectorAll(".item")];
  }

  function unselect() {
    for (const r of listEl.querySelectorAll(".item.on")) {
      r.classList.remove("on");
      r.removeAttribute("aria-selected");
    }
    listEl.removeAttribute("aria-activedescendant");
  }

  function select(it, { focus = true, read = true } = {}) {
    state.selected = keyOf(it);
    unselect();
    const r = listEl.querySelector(`[data-key="${state.selected}"]`);
    if (r) {
      r.classList.add("on");
      r.setAttribute("aria-selected", "true");
      listEl.setAttribute("aria-activedescendant", r.id);
      r.scrollIntoView({ block: "nearest" });
      if (focus) listEl.focus({ preventScroll: true });
    }
    // Only what the user chose goes in the address; the one shown by default does not.
    if (read) history.replaceState(null, "", `${location.pathname}${location.search}${it.itemType !== "prompt" ? `#${it.id}` : ""}`);
    // On a phone the detail replaces the list; only a chosen item opens it.
    if (read) document.body.classList.add("reading");
    openDetail(it);
  }

  // A memory by id that is not in the loaded list (an older one, or another filter's).
  function showById(id) {
    unselect();
    state.selected = `id-${id}`;
    document.body.classList.add("reading");
    openDetail({ itemType: "observation", id });
  }

  function move(step) {
    const all = rows();
    if (!all.length) return;
    const i = all.findIndex((r) => r.dataset.key === state.selected);
    const next = all[Math.min(all.length - 1, Math.max(0, i + step))];
    if (next) select(next._item);
    if (i + step >= all.length - 5) loadMore();
  }

  // ---------- detail ----------


  function sec(label, ...body) {
    const content = body.flat().filter(Boolean);
    return content.length ? el("section", { class: "d-sec" }, el("h4", {}, label), ...content) : null;
  }

  function header(it, extra) {
    return el("div", { class: "d-meta" },
      el("button", { class: "btn back", onclick: () => document.body.classList.remove("reading") }, "← List"),
      typeMark(itemType(it)),
      agentOf(it) ? agentBadge(agentOf(it)) : null,
      el("span", { title: it.project || "" }, shortProject(it.project)),
      el("span", {}, "·"),
      el("span", {}, fmtDate(it.created_at_epoch)),
      el("span", {}, "·"),
      el("span", { class: "id" }, it.itemType === "prompt" ? `E${it.id}` : `#${it.id}`),
      extra);
  }

  function fileRow(f) {
    let cls = "same", mark = "●", stateText = "unchanged since";
    if (f.kept) {
      cls = f.kept.intact ? "kept" : f.kept.kept * 5 >= f.kept.of ? "partly" : "gone";
      mark = f.kept.intact ? "●" : cls === "partly" ? "◐" : "○";
      stateText = f.kept.intact ? "edits still there" : cls === "partly" ? `${f.kept.kept} of ${f.kept.of} lines kept` : "edits gone";
    } else if (f.change) {
      cls = f.change.includes("no longer exists") ? "gone" : "changed";
      mark = cls === "gone" ? "○" : "◌";
      stateText = f.change;
    }
    return el("div", { class: `file ${cls}`, title: f.change ? `File: ${f.change}` : "" },
      el("span", { class: "mark" }, mark),
      el("span", { class: "path" }, f.path, f.modified ? "" : el("span", { class: "state" }, "  (read)")),
      el("span", { class: "state" }, stateText));
  }

  function filesSection(d) {
    const now = list(d.files_now);
    const recorded = [...new Set([...list(d.files_modified), ...list(d.files_read)])];
    if (now.length || d.files_unchecked) {
      return sec("Files · is the code still there?",
        now.length ? el("div", { class: "files" }, now.map(fileRow)) : null,
        d.files_unchecked ? el("div", { class: "file-note" }, `${d.files_unchecked}${now.length ? " more" : ""} not checked (too many files, or git was slow).`) : null,
        recorded.length > now.length + (d.files_unchecked || 0) ? el("div", { class: "file-note" }, `${recorded.length - now.length - (d.files_unchecked || 0)} more not found on this machine.`) : null);
    }
    if (!recorded.length) return null;
    const mod = list(d.files_modified).map(stripRoot);
    const read = list(d.files_read).map(stripRoot);
    return sec("Files",
      mod.length ? el("div", { class: "plain-files" }, `modified: ${mod.join(", ")}`) : null,
      read.length ? el("div", { class: "plain-files" }, `read: ${read.join(", ")}`) : null,
      el("div", { class: "file-note" }, "Not found in a repository on this machine, so whether the code survives is unknown."));
  }

  function uptakeStrip(d) {
    // Not loaded yet: say so rather than flash "not offered".
    if (!d.uptake) return el("div", { class: "d-strip" }, el("span", { class: "note" }, "Checking uptake and files…"));
    const u = d.uptake;
    const origin = d.origin === "mnem" ? "distilled by mnem" : `imported from ${d.origin}`;
    if (!u.offered) {
      return el("div", { class: "d-strip" },
        el("span", {}, "Not offered to an agent yet"),
        el("span", { class: "note" }, origin));
    }
    return el("div", { class: "d-strip" },
      el("span", {}, "offered ", el("b", {}, `${u.offered}×`), ` in ${u.sessions} session${u.sessions === 1 ? "" : "s"}`),
      el("span", {}, "opened ", el("b", {}, `${u.fetched}×`)),
      u.last_offered_ago ? el("span", {}, `last offered ${u.last_offered_ago} ago`) : null,
      el("span", { class: "note" }, origin));
  }

  const SUMMARY_SECTIONS = [
    ["investigated", "Investigated"],
    ["learned", "Learned"],
    ["completed", "Completed"],
    ["next_steps", "Next steps"],
  ];

  function renderDetail(d) {
    const actions = el("div", { class: "d-actions" },
      d.itemType !== "prompt" ? el("button", { class: "btn", onclick: (e) => copy(e.target, `#${d.id}`) }, "Copy id") : null,
      d.itemType !== "prompt" ? el("button", { class: "btn", onclick: (e) => copy(e.target, `${location.origin}/#${d.id}`) }, "Copy link") : null,
      d.project ? el("button", { class: "btn", onclick: () => setProject(d.project) }, `Only ${shortProject(d.project)}`) : null);

    if (d.itemType === "prompt") {
      return [header(d), el("h2", { class: "d-title", style: { fontWeight: 400 } }, d.prompt_text), actions];
    }
    if (d.itemType === "summary") {
      return [
        header(d),
        el("h2", { class: "d-title" }, d.request || "Session summary"),
        uptakeStrip(d),
        el("div", { class: "summary-grid", style: { marginTop: "16px" } },
          SUMMARY_SECTIONS.filter(([k]) => d[k]).map(([k, label]) => sec(label, el("p", { class: "prose" }, d[k])))),
        actions,
      ];
    }
    if (d.pinned) {
      return [
        header(d),
        el("h2", { class: "d-title" }, "Pinned fact"),
        el("p", { class: "d-sub" }, `Every agent sees this at session start in ${d.project === "*" ? "every project" : shortProject(d.project)}. Forget it with `, el("code", {}, `mnem forget ${d.id}`), "."),
        sec("Fact", el("p", { class: "prose" }, d.narrative || d.title)),
        actions,
      ];
    }
    const facts = list(d.facts);
    const concepts = list(d.concepts);
    return [
      header(d),
      el("h2", { class: "d-title" }, d.title || "Untitled"),
      d.subtitle ? el("p", { class: "d-sub" }, d.subtitle) : null,
      uptakeStrip(d),
      facts.length ? sec("Facts", el("ul", {}, facts.map((f) => el("li", {}, f)))) : null,
      d.narrative && d.narrative !== d.title ? sec("Narrative", el("p", { class: "prose" }, d.narrative)) : null,
      filesSection(d),
      concepts.length ? sec("Concepts", el("div", { class: "concepts" }, concepts.map((c) => el("span", { class: "concept" }, c)))) : null,
      actions,
    ];
  }

  async function openDetail(it) {
    const gen = ++state.detailGen;
    if (it.itemType === "prompt") {
      detailEl.replaceChildren(...renderDetail(it).filter(Boolean));
      return;
    }
    // Show what the list already has, then fill in files and uptake.
    if (it.title || it.request || it.narrative) detailEl.replaceChildren(...renderDetail({ ...it, uptake: null }).filter(Boolean));
    try {
      const d = await getJSON(`/api/memory/${it.id}`);
      if (gen !== state.detailGen) return;
      detailEl.replaceChildren(...renderDetail(d).filter(Boolean));
      detailEl.scrollTop = 0;
    } catch (e) {
      if (gen !== state.detailGen) return;
      detailEl.replaceChildren(el("div", { class: "empty" }, `Memory #${it.id} could not be read (${e.message}).`));
    }
  }

  async function copy(button, text) {
    try {
      await navigator.clipboard.writeText(text);
      const was = button.textContent;
      button.textContent = "Copied";
      setTimeout(() => (button.textContent = was), 1200);
    } catch {
      prompt("Copy:", text);
    }
  }

  // ---------- filters ----------

  function renderViews() {
    $("views").replaceChildren(...VIEWS.map(([v, label, icon, hint]) =>
      el("button", { class: `nav-item${state.view === v ? " on" : ""}`, "aria-current": state.view === v ? "true" : null, title: hint, onclick: () => setView(v) },
        svg(icon), el("span", { class: "name" }, label))));
  }

  // Project names as shown: the short name, or its last two parts where two projects
  // share one (two checkouts called `code`).
  let projectList = [];
  const projectName = new Map();
  function nameProjects(projects) {
    const seen = new Map();
    for (const p of projects) seen.set(shortProject(p), (seen.get(shortProject(p)) || 0) + 1);
    for (const p of projects) {
      const s = shortProject(p);
      const parts = p.split("#")[0].split("/").filter(Boolean);
      projectName.set(p, seen.get(s) > 1 && parts.length > 1 ? `${parts.slice(-2).join("/")}${p.includes("#") ? `#${p.split("#")[1]}` : ""}` : s);
    }
  }
  const nameOf = (p) => projectName.get(p) || shortProject(p);

  function renderProjects() {
    const all = el("button", { class: `nav-item${state.project ? "" : " on"}`, "aria-current": state.project ? null : "true", onclick: () => setProject("") },
      el("span", { class: "name" }, "All projects"));
    $("projects").replaceChildren(all, ...projectList.map((p) =>
      el("button", { class: `nav-item${state.project === p.project ? " on" : ""}`, "aria-current": state.project === p.project ? "true" : null, title: p.project, onclick: () => setProject(p.project) },
        el("span", { class: "name" }, nameOf(p.project)),
        el("span", { class: "count", title: "sessions" }, p.count))));
  }

  function renderChips() {
    $("type-chips").replaceChildren(...TYPES.map(([v, label]) => {
      const on = v.split(",").every((t) => state.types.has(t));
      const colorOf = v.split(",")[0];
      return el("button", { class: `chip${on ? " on" : ""}`, "aria-pressed": on ? "true" : "false", onclick: () => toggleType(v) },
        el("span", { class: "sw", style: { background: typeColor(colorOf) } }), label);
    }));
    $("agent-chips").replaceChildren(
      ...AGENTS.map((a) => el("button", {
        class: `chip${state.agent === a ? " on" : ""}`,
        "aria-pressed": state.agent === a ? "true" : "false",
        onclick: () => setAgent(state.agent === a ? "" : a),
      }, a)));
  }

  function toggleType(v) {
    const parts = v.split(",");
    const on = parts.every((t) => state.types.has(t));
    for (const t of parts) on ? state.types.delete(t) : state.types.add(t);
    refilter();
  }
  function setAgent(a) {
    state.agent = a;
    refilter();
  }
  function setView(v) {
    state.view = v;
    refilter();
  }
  function setProject(p) {
    state.project = p;
    setNav(false);
    refilter();
  }

  function setNav(open) {
    document.body.classList.toggle("nav-open", open);
    $("nav-toggle").setAttribute("aria-expanded", open ? "true" : "false");
  }

  function refilter() {
    syncUrl();
    renderViews();
    renderProjects();
    renderChips();
    reset();
  }

  // Filters and search in the address; a new list drops the #id of the old selection.
  function syncUrl() {
    const u = new URL(location.href);
    u.hash = "";
    const set = (k, v) => (v ? u.searchParams.set(k, v) : u.searchParams.delete(k));
    set("project", state.project);
    set("q", state.query);
    set("type", [...state.types].join(","));
    set("agent", state.agent);
    set("view", state.view);
    history.replaceState(null, "", u);
  }

  async function loadProjects() {
    const data = await getJSON("/api/projects");
    // A session without a project cannot be filtered to (empty means every project).
    projectList = data.projects.filter((p) => p.project);
    nameProjects(projectList.map((p) => p.project));
    renderProjects();
    const ctx = $("context-project");
    for (const p of projectList) ctx.append(el("option", { value: p.project }, nameOf(p.project)));
  }

  // ---------- health strip ----------

  let lastHealth = "";
  async function loadHealth() {
    try {
      const s = await getJSON("/api/stats");
      const alerts = s.alerts || [];
      const parts = [];
      const behind = s.files_behind > 0;
      parts.push(el("span", { class: behind ? "warn" : "", title: `${s.events.toLocaleString()} events · ${s.sessions.toLocaleString()} sessions · ${s.memories.toLocaleString()} memories` },
        el("span", { class: "dot" }),
        behind ? `${s.files_behind} transcript${s.files_behind > 1 ? "s" : ""} behind` : `capture ${s.newest_event_ago} ago`));
      const distillWarn = s.last_distill_error || s.pending_distill > 10;
      parts.push(el("span", { class: `extra ${distillWarn ? "warn" : ""}`, title: s.last_distill_error ? `Last error: ${s.last_distill_error}` : "Sessions waiting to become memories" },
        el("span", { class: "dot" }),
        s.pending_distill ? `${s.pending_distill} to distill` : "distilled"));
      parts.push(el("span", { class: `extra ${s.backup_stale ? "warn" : ""}`, title: "Newest verified backup" },
        el("span", { class: "dot" }),
        s.backup_ago ? `backup ${s.backup_ago} ago` : "no backup"));
      if (alerts.length) {
        parts.push(el("span", { class: "bad", title: alerts.map((a) => `⚠ ${a}`).join("\n") },
          el("span", { class: "dot" }), `${alerts.length} alert${alerts.length > 1 ? "s" : ""}`));
      }
      // Replaced only when it says something new, so it is not re-announced every poll.
      const text = parts.map((p) => `${p.className}|${p.textContent}|${p.title}`).join("/");
      if (text !== lastHealth) {
        lastHealth = text;
        $("health").replaceChildren(...parts);
      }
    } catch (e) {
      $("health").replaceChildren(el("span", { class: "bad" }, el("span", { class: "dot" }), "mnem not answering"));
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
    $("context-modal").hidden = false;
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

  // ---------- backup & move ----------

  const fmtBytes = (n) => (n >= 1e9 ? `${(n / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(n / 1e6))} MB`);
  const fmtNum = (n) => Number(n).toLocaleString();
  const countsText = (c) => `${fmtNum(c.sessions)} sessions · ${fmtNum(c.events)} events · ${fmtNum(c.memories)} memories`;

  // Actions that change something carry this header; the server refuses them without it.
  async function postJSON(url) {
    const r = await fetch(url, { method: "POST", headers: { "X-Mnem": "1" } });
    const data = await r.json().catch(() => ({ error: `${r.status}` }));
    if (!r.ok) throw Object.assign(new Error(data.error || `${r.status}`), { data });
    return data;
  }

  function setStatus(id, text, error = false) {
    const s = $(id);
    s.textContent = text;
    s.classList.toggle("error", error);
  }

  async function showMove() {
    $("move-modal").hidden = false;
    try {
      const data = await getJSON("/api/backups");
      const c = data.current;
      $("move-current").replaceChildren(
        el("b", {}, c.host), ` holds ${countsText(c)} (${fmtBytes(c.bytes)}).`);
      $("move-list").replaceChildren(
        ...data.backups.map((b) =>
          el("div", { class: "move-row" },
            el("div", { class: "move-meta" },
              el("b", {}, fmtDate(b.created_at)), ` · ${fmtBytes(b.bytes)} · ${countsText(b)}`,
              b.host ? ` · from ${b.host}` : "",
              b.has_settings ? " · with settings" : ""),
            el("a", { class: "move-btn", href: `/api/backups/${encodeURIComponent(b.file)}`, download: b.file }, "Download"))),
      );
      if (!data.backups.length) $("move-list").replaceChildren(el("p", { class: "move-help" }, "No backups yet."));
    } catch (e) {
      $("move-current").textContent = `Could not read backups: ${e.message}`;
    }
  }

  async function createBackup() {
    const b = $("move-create");
    b.disabled = true;
    setStatus("move-create-status", "Taking and checking a snapshot…");
    try {
      const { backup } = await postJSON("/api/backups");
      setStatus("move-create-status", `Done: ${fmtBytes(backup.bytes)}, integrity and checksum verified. Download it below.`);
      await showMove();
    } catch (e) {
      setStatus("move-create-status", `Backup failed: ${e.message}`, true);
    } finally {
      b.disabled = false;
    }
  }

  // Upload with progress (fetch cannot report upload progress).
  function upload(file) {
    return new Promise((resolve, reject) => {
      const x = new XMLHttpRequest();
      x.open("POST", "/api/import");
      x.setRequestHeader("X-Mnem", "1");
      x.setRequestHeader("Content-Type", "application/octet-stream");
      x.upload.onprogress = (e) => {
        if (e.lengthComputable) $("move-progress-bar").style.width = `${(100 * e.loaded) / e.total}%`;
        if (e.loaded === e.total) setStatus("move-upload-status", "Checking the backup (integrity, schema, search)…");
      };
      x.onload = () => {
        let data = {};
        try { data = JSON.parse(x.responseText); } catch { /* keep empty */ }
        x.status < 300 ? resolve(data) : reject(new Error(data.error || `${x.status}`));
      };
      x.onerror = () => reject(new Error("upload failed"));
      x.send(file);
    });
  }

  async function chooseFile(file) {
    if (!file) return;
    const preview = $("move-preview");
    preview.hidden = true;
    $("move-progress").hidden = false;
    $("move-progress-bar").style.width = "0";
    setStatus("move-upload-status", `Uploading ${file.name} (${fmtBytes(file.size)})…`);
    try {
      const p = await upload(file);
      setStatus("move-upload-status", "Backup checked: it is sound and this mnem can read it.");
      renderPreview(p);
    } catch (e) {
      setStatus("move-upload-status", `Not imported: ${e.message}`, true);
    } finally {
      $("move-progress").hidden = true;
      $("move-file").value = "";
    }
  }

  const show = (v) => (v === null || v === undefined ? "(not set)" : typeof v === "string" ? v : JSON.stringify(v));

  // What taking the backup's settings would change; off unless the user opts in.
  function settingsBlock(review) {
    if (!review) return { node: el("p", { class: "move-help" }, "This backup carries no settings; this machine keeps its own."), box: null };
    if (!review.valid) {
      return { node: el("p", { class: "move-help error" }, `The backup’s settings are not valid (${review.error}); they will not be used.`), box: null };
    }
    if (!review.changes.length) {
      return { node: el("p", { class: "move-help" }, "The backup’s settings match this machine’s."), box: null };
    }
    const box = el("input", { type: "checkbox", id: "move-settings" });
    const rows = review.changes.map(([key, here, theirs]) =>
      el("li", { class: review.sensitive.includes(key) ? "sensitive" : null },
        el("code", {}, key), `: ${show(here)} → ${show(theirs)}`));
    return {
      box,
      node: el("div", { class: "move-settings" },
        el("label", {}, box, el("span", {}, "Also use the backup’s settings (this machine’s are kept as config.json.bak). They would change:")),
        el("ul", {}, rows),
        review.sensitive.length
          ? el("p", { class: "move-help error" }, "Marked lines change where prompts and memories are sent or which key is used. Only tick this if you recognise them.")
          : null,
        review.warning ? el("p", { class: "move-help error" }, review.warning) : null),
    };
  }

  function renderPreview(p) {
    const preview = $("move-preview");
    const o = p.origin;
    const { node: settingsNode, box: settings } = settingsBlock(p.settings);
    const apply = el("button", { class: "move-btn danger" }, "Replace this machine’s memory with the backup");
    const cancel = el("button", { class: "move-btn quiet" }, "Cancel");
    const result = el("p", { class: "move-help", role: "status" });
    preview.replaceChildren(
      el("div", { class: "move-compare" },
        el("div", {}, el("b", {}, `Backup${o.host ? ` from ${o.host}` : ""}`),
          o.taken_at ? `${fmtDate(o.taken_at)} · ` : "", countsText(p.backup)),
        el("div", {}, el("b", {}, `This machine now (${p.current.host})`), countsText(p.current))),
      el("p", { class: "move-warn" },
        "Importing replaces this machine’s memory with the backup. The current memory is saved as a backup first, and transcripts still on this machine are read again afterwards, so their sessions come back."),
      settingsNode,
      el("div", { class: "move-actions" }, apply, cancel),
      result,
    );
    preview.hidden = false;
    cancel.addEventListener("click", async () => {
      await postJSON(`/api/import/discard?file=${encodeURIComponent(p.file)}`).catch(() => {});
      preview.hidden = true;
      setStatus("move-upload-status", "Import cancelled; nothing changed.");
    });
    apply.addEventListener("click", async () => {
      apply.disabled = cancel.disabled = true;
      result.classList.remove("error");
      result.textContent = "Saving the current memory, then restoring the backup…";
      try {
        const useSettings = settings && settings.checked ? "1" : "0";
        const r = await postJSON(`/api/import/apply?file=${encodeURIComponent(p.file)}&settings=${useSettings}`);
        result.replaceChildren(`Imported: this machine now holds ${countsText(r.current)}.`);
        if (r.foreign_transcripts) {
          result.append(` ${fmtNum(r.foreign_transcripts)} transcripts from the other machine are kept as history.`);
        }
        if (r.settings_error) {
          result.append(` The settings could not be written (${r.settings_error}); this machine keeps its own.`);
        } else if (r.settings_applied) {
          if (r.can_restart) {
            const restart = el("button", { class: "move-btn primary" }, "Restart mnem to use the settings");
            restart.addEventListener("click", () => restartMnem(restart, result));
            result.append(" ", restart);
          } else {
            result.append(" Restart mnem to use the restored settings.");
          }
        }
        reset();
        loadHealth();
      } catch (e) {
        const changed = e.data && e.data.changed;
        result.classList.add("error");
        result.textContent = changed
          ? `The memory was replaced, but: ${e.message}`
          : `Import failed, nothing changed: ${e.message}`;
        apply.disabled = cancel.disabled = !!changed;
      }
    });
  }

  async function restartMnem(button, result) {
    button.disabled = true;
    try {
      await postJSON("/api/restart");
    } catch (e) {
      result.append(` ${e.message}`);
      return;
    }
    result.textContent = "Restarting mnem (about 10 s)…";
    await new Promise((r) => setTimeout(r, 3000));
    for (let i = 0; i < 40; i++) {
      try {
        await getJSON("/api/stats");
        location.reload();
        return;
      } catch {
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
    result.textContent = "mnem has not come back yet; check `systemctl --user status mnem-watch`.";
  }

  // ---------- wiring ----------

  function closeModals() {
    $("context-modal").hidden = true;
    $("move-modal").hidden = true;
  }

  function init() {
    const url = new URL(location.href);
    state.project = url.searchParams.get("project") || "";
    state.query = url.searchParams.get("q") || "";
    state.agent = url.searchParams.get("agent") || "";
    state.view = url.searchParams.get("view") || "";
    for (const t of (url.searchParams.get("type") || "").split(",")) if (t) state.types.add(t);
    $("search").value = state.query;

    let pref = store.get("mnem-theme");
    if (!THEME_ICONS[pref]) pref = "system";
    applyTheme(pref);
    $("theme").addEventListener("click", () => {
      const cycle = ["system", "light", "dark"];
      pref = cycle[(cycle.indexOf(pref) + 1) % cycle.length];
      applyTheme(pref);
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
    $("search").addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown" || e.key === "Enter") {
        e.preventDefault();
        const first = rows()[0];
        if (first) select(first._item);
      }
    });

    $("nav-toggle").addEventListener("click", () => setNav(!document.body.classList.contains("nav-open")));
    $("context-btn").addEventListener("click", () => showContext(state.project));
    $("context-close").addEventListener("click", closeModals);
    $("context-project").addEventListener("change", (e) => showContext(e.target.value));
    $("move-btn").addEventListener("click", showMove);
    $("move-close").addEventListener("click", closeModals);
    for (const id of ["context-modal", "move-modal"]) {
      $(id).addEventListener("click", (e) => {
        if (e.target.id === id) closeModals();
      });
    }
    $("move-create").addEventListener("click", createBackup);
    $("move-file").addEventListener("change", (e) => chooseFile(e.target.files[0]));

    document.addEventListener("keydown", (e) => {
      const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);
      const modal = !$("context-modal").hidden || !$("move-modal").hidden;
      if (e.key === "Escape") {
        if (modal) return closeModals();
        if (typing) return document.activeElement.blur();
        if (document.body.classList.contains("nav-open")) return setNav(false);
        if (document.body.classList.contains("reading")) return document.body.classList.remove("reading");
      }
      if (typing || modal || e.metaKey || e.ctrlKey || e.altKey) return;
      // Arrows move the list only from the list itself (elsewhere they scroll); j/k anywhere.
      const inList = document.activeElement === document.body || listEl.contains(document.activeElement);
      if (e.key === "/") {
        e.preventDefault();
        $("search").focus();
        $("search").select();
      } else if (e.key === "j" || (e.key === "ArrowDown" && inList)) {
        e.preventDefault();
        move(1);
      } else if (e.key === "k" || (e.key === "ArrowUp" && inList)) {
        e.preventDefault();
        move(-1);
      }
    });

    new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting) loadMore();
    }, { root: listEl, rootMargin: "400px" }).observe(sentinel);

    // A #id link opened in this tab: show that memory (in the list if it is loaded).
    window.addEventListener("hashchange", () => {
      const id = location.hash.slice(1);
      if (!/^\d+$/.test(id)) return;
      const it = state.items.find((x) => x.itemType !== "prompt" && `${x.id}` === id);
      if (it) select(it);
      else showById(Number(id));
    });

    renderViews();
    renderProjects();
    renderChips();
    loadProjects().catch(console.error);
    loadHealth();
    reset();
    setInterval(poll, 4000);
    setInterval(loadHealth, 15000);
  }

  init();
})();
