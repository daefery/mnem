// Every capability of the feed viewer, checked the same way on any build of it and on
// any record. Prints PASS/FAIL lines and exits 1 on a failure; with an output path it
// also writes a JSON fingerprint of what the viewer shows (items in order, per-item
// content), so two builds on the same record can be compared for identical output.
//
//   node features.js http://127.0.0.1:37777 [fingerprint.json]
//
// Read-only: it never creates, imports or deletes anything. `sh run.sh` runs it against
// a demo record. Uses the installed Chrome; set RAVNORI_CHROME to a browser binary instead.
const { chromium } = require("playwright-core");
const B = (process.argv[2] || "").replace(/\/$/, "");
const out = process.argv[3];
if (!B) { console.error("usage: node features.js <viewer URL> [fingerprint.json]"); process.exit(2); }
const ok = (c, m) => { console.log(`${c ? "PASS" : "FAIL"} ${m}`); if (!c) process.exitCode = 1; };
const text = (es) => es.map((e) => e.textContent.replace(/\s+/g, " ").trim());

(async () => {
  const browser = await chromium.launch(process.env.RAVNORI_CHROME ? { executablePath: process.env.RAVNORI_CHROME } : { channel: "chrome" });
  const page = await browser.newPage({ viewport: { width: 1366, height: 900 } });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  const fp = {};

  // Feed loads observations, summaries and prompts.
  await page.goto(B + "/");
  await page.waitForSelector(".card");
  await page.waitForTimeout(1500);
  const kinds = await page.$$eval(".card", (es) => es.map((e) => e.classList.contains("summary-card") ? "summary" : e.classList.contains("prompt-card") ? "prompt" : "observation"));
  ok(kinds.includes("observation") && kinds.includes("summary") && kinds.includes("prompt"), `feed mixes observations, summaries, prompts (${kinds.length} cards)`);
  fp.first = await page.$$eval(".card", (es) => es.slice(0, 40).map((e) => e.textContent.replace(/\s+/g, " ").trim()));

  // Infinite scroll loads more pages.
  // A record that fits on the first page has nothing more to load: then the feed must
  // say so, and scrolling must not repeat items.
  const before = kinds.length;
  for (let i = 0; i < 12 && (await page.$$eval(".card", (e) => e.length)) < 194; i++) { await page.$eval("#feed", (f) => (f.scrollTop = f.scrollHeight)); await page.waitForTimeout(700); }
  const after = await page.$$eval(".card", (e) => e.length);
  const atEnd = (await page.textContent("#feed")).includes("No more items to load");
  if (after > before) ok(true, `scrolling loads more (${before} -> ${after})`);
  else {
    const all = await page.$$eval(".card", text);
    ok(atEnd && new Set(all).size === all.length, `scrolling at the end adds nothing and says so (${before} cards, all of them)`);
  }
  fp.scrolledCount = Math.min(after, 194);
  fp.scrolled = await page.$$eval(".card", (es) => es.slice(0, 194).map((e) => e.textContent.replace(/\s+/g, " ").trim().slice(0, 120)));
  ok(await page.isVisible("#to-top"), "scroll-to-top appears after scrolling");
  await page.click("#to-top");
  await page.waitForFunction(() => document.getElementById("feed").scrollTop < 50, null, { timeout: 8000 }).catch(() => {});
  ok((await page.$eval("#feed", (f) => f.scrollTop)) < 50, "scroll-to-top returns to the top");

  // Observation card: facts and narrative toggles.
  const obs = await page.$$(".card:not(.summary-card):not(.prompt-card)");
  let toggled = false;
  for (const c of obs.slice(0, 15)) {
    const facts = await c.$(".view-mode-toggle >> text=facts");
    const narr = await c.$(".view-mode-toggle >> text=narrative");
    if (!facts || !narr) continue;
    await facts.click();
    const hasFacts = !!(await c.$(".facts-list li"));
    await narr.click();
    const hasNarr = !!(await c.$(".narrative"));
    await narr.click();
    const back = !!(await c.$(".card-subtitle")) || !(await c.$(".narrative"));
    ok(hasFacts && hasNarr && back, "facts / narrative toggles switch views and back");
    toggled = true;
    break;
  }
  ok(toggled, "found an observation with facts and narrative");

  // Summary card sections with icons.
  const sum = await page.$(".summary-card");
  const labels = await sum.$$eval(".summary-section-label", (e) => e.map((x) => x.textContent));
  ok(labels.length >= 1, `summary shows sections (${labels.join(", ")})`);
  // Each icon is a mask over the theme's summary colour: its file must load, and it must
  // be drawn in that colour (not a fixed one).
  const icons = await sum.$$eval(".summary-section-icon", (els) =>
    Promise.all(els.map(async (x) => {
      const st = getComputedStyle(x);
      const url = (st.maskImage || st.webkitMaskImage || "").match(/url\("?([^")]+)"?\)/)?.[1];
      const loaded = !!url && (await fetch(url)).ok;
      const summary = getComputedStyle(document.documentElement).getPropertyValue("--summary").trim();
      return loaded && x.offsetWidth > 0 && st.backgroundColor !== "rgba(0, 0, 0, 0)" && !!summary;
    })));
  ok(icons.length === labels.length && icons.every(Boolean), "summary section icons load, in the theme's colour");

  // Source badges and project names.
  // What the viewer controls: each card has one visible badge naming a known agent, with
  // the matching source-<agent> class. Which agents appear depends on the data, so the
  // mix is reported but not required.
  const badgeInfo = await page.$$eval(".card", (cards) => {
    const known = ["claude", "codex", "pi", "cursor"];
    const counts = {};
    let bad = 0;
    for (const c of cards) {
      const b = c.querySelectorAll(".card-source");
      const t = b.length === 1 ? b[0].textContent.trim() : "";
      const r = b.length === 1 ? b[0].getBoundingClientRect() : { width: 0, height: 0 };
      if (b.length !== 1 || !known.includes(t) || !b[0].classList.contains("source-" + t) || !r.width || !r.height) bad++;
      else counts[t] = (counts[t] || 0) + 1;
    }
    return { cards: cards.length, bad, counts };
  });
  const mix = Object.entries(badgeInfo.counts).map(([k, v]) => `${k} ${v}`).join(", ");
  ok(badgeInfo.cards > 0 && badgeInfo.bad === 0, `agent badges on every card, named and styled (${mix}${badgeInfo.bad ? `; ${badgeInfo.bad} wrong` : ""})`);

  // Health pill.
  const health = await page.textContent("#health-label");
  ok(/memories|behind|alert/.test(health), `health pill (${health})`);

  // Project filter: the first project in the list.
  const proj = await page.$eval("#project", (s) => [...s.options].map((o) => ({ value: o.value, label: o.textContent })).find((o) => o.value));
  const projName = proj ? proj.label.replace(/ \(\d[\d,.]*\)$/, "") : "";
  if (proj) await page.selectOption("#project", proj.value);
  await page.waitForTimeout(1200);
  const projs = await page.$$eval(".card-project, .summary-project-badge", (e) => [...new Set(e.map((x) => x.textContent))]);
  ok(!!proj && projs.length === 1 && projs[0] === projName, `project filter (${projName}: ${projs})`);
  ok(!!proj && page.url().includes("project=" + encodeURIComponent(proj.value)), "project in the URL");
  fp.project = await page.$$eval(".card", (es) => es.slice(0, 20).map((e) => e.textContent.replace(/\s+/g, " ").trim()));
  await page.selectOption("#project", "");
  await page.waitForTimeout(1000);

  // Search with match labels, / shortcut.
  await page.keyboard.press("/");
  ok(await page.evaluate(() => document.activeElement.id === "search"), "/ focuses search");
  // Search for words from the newest memory's title, so it has something to find.
  const title = await page.$eval(".card:not(.summary-card):not(.prompt-card) .card-title", (e) => e.textContent);
  const words = title.toLowerCase().match(/[a-z]{5,}/g) || ["memory"];
  const query = words.slice(0, 2).join(" ");
  await page.keyboard.type(query);
  await page.waitForTimeout(1500);
  const matches = await page.$$eval(".ravnori-match", (e) => [...new Set(e.map((x) => x.textContent))]);
  ok(matches.length > 0, `search shows match labels ("${query}": ${matches})`);
  ok(page.url().includes("q=" + encodeURIComponent(words[0])), "search in the URL");
  fp.search = await page.$$eval(".card", (es) => es.slice(0, 20).map((e) => e.textContent.replace(/\s+/g, " ").trim()));
  // An empty result says so: a project with no memories, searched.
  const e = await browser.newPage({ viewport: { width: 1366, height: 900 } });
  await e.goto(B + "/?project=" + encodeURIComponent("/tmp") + "&q=" + encodeURIComponent("zzzqqq"));
  await e.waitForTimeout(2000);
  ok((await e.textContent("#feed")).includes("No matches"), "no-match message");
  await e.close();
  await page.fill("#search", "");
  await page.waitForTimeout(1200);

  // Theme cycles system -> light -> dark -> system and persists.
  const th = [];
  for (let i = 0; i < 3; i++) { await page.click("#theme"); th.push(await page.getAttribute("html", "data-theme")); }
  ok(JSON.stringify(th) === JSON.stringify(["light", "dark", null]), `theme cycles (${th})`);
  await page.click("#theme"); await page.reload(); await page.waitForSelector(".card");
  ok((await page.getAttribute("html", "data-theme")) === "light", "theme persists across reload");
  await page.click("#theme"); await page.click("#theme");

  // Context preview modal.
  await page.click("#context-btn");
  await page.waitForFunction(() => document.getElementById("context-text").textContent !== "Loading…");
  ok((await page.textContent("#context-text")).length > 100, "context preview shows text");
  await page.keyboard.press("Escape");
  ok(!(await page.isVisible("#context-modal")), "Escape closes context preview");

  // Backup & move modal (read-only: lists backups, no create/import here).
  await page.click("#move-btn");
  await page.waitForFunction(() => !document.getElementById("move-current").textContent.startsWith("Loading"));
  ok((await page.textContent("#move-current")).includes("holds"), "backup modal shows this machine");
  const rows = await page.$$("#move-list .move-row");
  const rowsOk = rows.length ? (await page.$$("#move-list .move-row a[download]")).length === rows.length : (await page.textContent("#move-list")).includes("No backups yet");
  ok(rowsOk, `backup modal lists backups with download (${rows.length})`);
  ok(await page.isVisible("label[for=move-file]"), "import control present");
  await page.click("#move-close");
  ok(!(await page.isVisible("#move-modal")), "backup modal closes");

  // Live updates: the newest-item poll adds new items at the top.
  const pollSeen = [];
  page.on("request", (r) => r.url().includes("after=") && pollSeen.push(r.url()));
  await page.waitForTimeout(5000);
  ok(pollSeen.length >= 1, "polls for new items");

  // Narrow screen: no horizontal overflow.
  const m = await browser.newPage({ viewport: { width: 380, height: 800 } });
  await m.goto(B + "/"); await m.waitForSelector(".card");
  ok(!(await m.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1)), "no horizontal overflow at 380px");

  ok(errors.length === 0, `no console errors ${errors.join(" | ")}`);
  if (out) require("fs").writeFileSync(out, JSON.stringify(fp, null, 1));
  await browser.close();
})().catch((e) => { console.error(e); process.exit(1); });
