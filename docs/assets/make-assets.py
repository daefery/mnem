#!/usr/bin/env python3
"""Draw the README images (light and dark): the banner, how mnem works, and terminal
panels rendered from real command output.

    python3 docs/assets/make-assets.py [ask.txt] [trace.txt]

ask.txt / trace.txt: output captured from `mnem ask` and `mnem trace` (the demo record
from docs/demo/make-demo.py for ask, this repository for trace); without them the
panels are not redrawn.
"""
import html
import os
import sys
import textwrap

HERE = os.path.dirname(os.path.abspath(__file__))
SANS = "ui-sans-serif, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace"

THEMES = {
    "dark": dict(bg="#0d1117", panel="#161b22", panel2="#1c2430", ink="#e6edf3", ink2="#9da7b3",
                 ink3="#6e7781", rule="#30363d", teal="#2dd4bf", teal_soft="#0f3d39", amber="#f0a64a",
                 violet="#b79cff", term="#0b0f14", term_bar="#1b222c"),
    "light": dict(bg="#ffffff", panel="#f6f8fa", panel2="#eef2f5", ink="#1f2328", ink2="#475260",
                  ink3="#6e7781", rule="#d0d7de", teal="#0f766e", teal_soft="#d6efec", amber="#b45309",
                  violet="#6d28d9", term="#0d1117", term_bar="#1f2630"),
}

LOGO = """<g transform="translate({x},{y}) scale({s})">
  <rect x="4" y="4" width="56" height="56" rx="14" fill="#0f766e"/>
  <rect x="14" y="17" width="36" height="7" rx="3.5" fill="#99f6e4"/>
  <rect x="14" y="28.5" width="28" height="7" rx="3.5" fill="#5eead4"/>
  <rect x="14" y="40" width="20" height="7" rx="3.5" fill="#2dd4bf"/>
</g>"""


def esc(s):
    return html.escape(s, quote=False)


def write(name, svg):
    with open(os.path.join(HERE, name), "w") as f:
        f.write(svg)


def hero(t):
    W, H = 1280, 420
    pills = ""
    x = 64
    for name in ["Claude Code", "Codex", "pi"]:
        w = 22 + 9.6 * len(name)
        pills += f'<rect x="{x}" y="300" width="{w:.0f}" height="34" rx="17" fill="{t["teal_soft"]}" stroke="{t["teal"]}" stroke-opacity="0.5"/>'
        pills += f'<text x="{x + w / 2:.0f}" y="322" text-anchor="middle" font-family="{SANS}" font-size="15" font-weight="600" fill="{t["teal"]}">{name}</text>'
        x += w + 10
    pills += f'<text x="{x + 6:.0f}" y="322" font-family="{SANS}" font-size="15" fill="{t["ink3"]}">· one memory, shared</text>'
    card_x, card_y = 790, 70
    card = f"""
  <rect x="{card_x}" y="{card_y}" width="430" height="280" rx="16" fill="{t['panel']}" stroke="{t['rule']}"/>
  <rect x="{card_x + 24}" y="{card_y + 24}" width="86" height="24" rx="6" fill="{t['teal_soft']}"/>
  <text x="{card_x + 67}" y="{card_y + 41}" text-anchor="middle" font-family="{MONO}" font-size="12" font-weight="700" fill="{t['teal']}">DECISION</text>
  <rect x="{card_x + 118}" y="{card_y + 24}" width="72" height="24" rx="12" fill="none" stroke="{t['amber']}" stroke-opacity="0.6"/>
  <text x="{card_x + 154}" y="{card_y + 41}" text-anchor="middle" font-family="{MONO}" font-size="12" font-weight="700" fill="{t['amber']}">CLAUDE</text>
  <text x="{card_x + 24}" y="{card_y + 86}" font-family="{SANS}" font-size="19" font-weight="700" fill="{t['ink']}">Deploy order for orders v2:</text>
  <text x="{card_x + 24}" y="{card_y + 112}" font-family="{SANS}" font-size="19" font-weight="700" fill="{t['ink']}">migration first, then workers</text>
  <text x="{card_x + 24}" y="{card_y + 146}" font-family="{SANS}" font-size="14" fill="{t['ink2']}">New workers read order_items, old ones ignore it.</text>
  <line x1="{card_x + 24}" y1="{card_y + 170}" x2="{card_x + 406}" y2="{card_y + 170}" stroke="{t['rule']}"/>
  <text x="{card_x + 24}" y="{card_y + 198}" font-family="{MONO}" font-size="13" fill="{t['ink3']}">evidence  E2041 E2044 E2058</text>
  <text x="{card_x + 24}" y="{card_y + 224}" font-family="{MONO}" font-size="13" fill="{t['teal']}">✓ its edited lines are still there</text>
  <text x="{card_x + 24}" y="{card_y + 250}" font-family="{MONO}" font-size="13" fill="{t['ink3']}">offered 3× · opened 1× · 2d ago</text>"""
    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="mnem: memory for coding agents">
  <rect width="{W}" height="{H}" rx="20" fill="{t['bg']}"/>
  <rect x="0.5" y="0.5" width="{W - 1}" height="{H - 1}" rx="20" fill="none" stroke="{t['rule']}"/>
  {LOGO.format(x=60, y=62, s=0.95)}
  <text x="134" y="112" font-family="{MONO}" font-size="46" font-weight="700" fill="{t['ink']}">mnem</text>
  <text x="64" y="186" font-family="{SANS}" font-size="31" font-weight="700" fill="{t['ink']}">Your coding agents already write</text>
  <text x="64" y="226" font-family="{SANS}" font-size="31" font-weight="700" fill="{t['ink']}">everything down. <tspan fill="{t['teal']}">mnem remembers it.</tspan></text>
  <text x="64" y="268" font-family="{SANS}" font-size="17" fill="{t['ink2']}">One local record of every session · memories with evidence · nothing lost</text>
  {pills}
  {card}
</svg>
"""
    return svg


def box(x, y, w, h, t, title, lines, accent=None, fill=None):
    a = accent or t["rule"]
    out = f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="14" fill="{fill or t["panel"]}" stroke="{a}"/>'
    out += f'<text x="{x + 18}" y="{y + 32}" font-family="{SANS}" font-size="17" font-weight="700" fill="{t["ink"]}">{esc(title)}</text>'
    for i, l in enumerate(lines):
        out += f'<text x="{x + 18}" y="{y + 60 + i * 23}" font-family="{SANS}" font-size="14" fill="{t["ink2"]}">{esc(l)}</text>'
    return out


def arrow(x1, y1, x2, y2, t, label=None, color=None):
    c = color or t["ink3"]
    out = f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{c}" stroke-width="2" marker-end="url(#ah-{id(t)})"/>'
    if label:
        out += f'<text x="{(x1 + x2) / 2}" y="{min(y1, y2) - 8}" text-anchor="middle" font-family="{SANS}" font-size="13" fill="{t["ink3"]}">{esc(label)}</text>'
    return out


def how(t):
    W, H = 1280, 540
    parts = []
    parts.append(f'<defs><marker id="ah-{id(t)}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="8" markerHeight="8" orient="auto"><path d="M0,0 L10,5 L0,10 z" fill="{t["ink3"]}"/></marker></defs>')
    parts.append(f'<rect width="{W}" height="{H}" rx="20" fill="{t["bg"]}"/><rect x="0.5" y="0.5" width="{W - 1}" height="{H - 1}" rx="20" fill="none" stroke="{t["rule"]}"/>')
    rows_y = [70, 190, 310]
    # Agents (left)
    for y, (name, path) in zip(rows_y, [("Claude Code", "~/.claude/projects"), ("Codex", "~/.codex/sessions"), ("pi", "~/.pi/agent/sessions")]):
        parts.append(box(40, y, 250, 96, t, name, [f"writes {path}"]))
    # mnem (middle)
    mx, my, mw, mh = 380, 70, 470, 336
    parts.append(f'<rect x="{mx}" y="{my}" width="{mw}" height="{mh}" rx="16" fill="{t["panel2"]}" stroke="{t["teal"]}" stroke-width="2"/>')
    parts.append(LOGO.format(x=mx + 18, y=my + 16, s=0.5))
    parts.append(f'<text x="{mx + 58}" y="{my + 44}" font-family="{MONO}" font-size="22" font-weight="700" fill="{t["ink"]}">mnem</text>')
    rows = [
        ("Capture", "reads transcripts as they grow; a crash or", "a missed hook delays it, never loses it"),
        ("Record", "one local SQLite: prompts, edits, commands;", "deduplicated, secrets redacted, deletable"),
        ("Distil", "memories that cite the events they came from,", "with your own Claude Code, Codex or endpoint"),
        ("Check", "whether a memory's own code is still there", ""),
    ]
    y = my + 88
    for k, a, b in rows:
        parts.append(f'<text x="{mx + 22}" y="{y}" font-family="{SANS}" font-size="14" font-weight="700" fill="{t["teal"]}">{k}</text>')
        parts.append(f'<text x="{mx + 96}" y="{y}" font-family="{SANS}" font-size="14" fill="{t["ink2"]}">{esc(a)}</text>')
        if b:
            parts.append(f'<text x="{mx + 96}" y="{y + 21}" font-family="{SANS}" font-size="14" fill="{t["ink2"]}">{esc(b)}</text>')
        y += 64 if b else 40
    # Outputs (right)
    outs = [
        ("Back to your agents", ["session start, each prompt, opening a file", "MCP tools: search, get_observations, …"], t["teal"]),
        ("To you", ["the viewer at 127.0.0.1:37777", "mnem ask “why did we …”, with sources"], None),
        ("To your tools", ["record API: read-only, local, token", "mnem trace: Agent Trace for commits"], None),
    ]
    for y, (title, lines, accent) in zip(rows_y, outs):
        parts.append(box(940, y, 300, 96, t, title, lines, accent=accent))
    # Arrows: agents -> mnem, mnem -> outputs, each to the middle of its box
    for y in rows_y:
        parts.append(arrow(296, y + 48, mx - 8, y + 48, t))
        parts.append(arrow(mx + mw + 6, y + 48, 932, y + 48, t))
    parts.append(f'<text x="640" y="458" text-anchor="middle" font-family="{SANS}" font-size="15" fill="{t["ink2"]}">Everything stays on your machine. The only thing that leaves it: redacted excerpts sent to the model <tspan font-weight="700">you</tspan> choose, to write memories.</text>')
    parts.append(f'<text x="640" y="488" text-anchor="middle" font-family="{SANS}" font-size="15" fill="{t["ink2"]}">One binary · no Node, Python, Docker or vector database · Linux, WSL and macOS</text>')
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" role="img" aria-label="How mnem works">{"".join(parts)}</svg>\n'


def terminal(t, title, command, lines, color_of):
    width = 1000
    wrapped = []
    for l in lines:
        for w in (textwrap.wrap(l, 104, subsequent_indent="    ") or [""]):
            wrapped.append((w, color_of(l)))
    H = 70 + 24 * (len(wrapped) + 2)
    p = [f'<rect width="{width}" height="{H}" rx="12" fill="{t["term"]}"/>',
         f'<rect width="{width}" height="40" rx="12" fill="{t["term_bar"]}"/><rect y="28" width="{width}" height="12" fill="{t["term_bar"]}"/>',
         '<circle cx="24" cy="20" r="6" fill="#ff5f57"/><circle cx="44" cy="20" r="6" fill="#febc2e"/><circle cx="64" cy="20" r="6" fill="#28c840"/>',
         f'<text x="{width / 2}" y="25" text-anchor="middle" font-family="{SANS}" font-size="13" fill="#8b949e">{esc(title)}</text>',
         f'<text x="24" y="74" font-family="{MONO}" font-size="14" fill="#2dd4bf">$ <tspan fill="#e6edf3">{esc(command)}</tspan></text>']
    for i, (l, c) in enumerate(wrapped):
        p.append(f'<text x="24" y="{106 + i * 24}" font-family="{MONO}" font-size="14" fill="{c}" xml:space="preserve">{esc(l)}</text>')
    label = html.escape(command, quote=True)
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{H}" viewBox="0 0 {width} {H}" role="img" aria-label="{label}">{"".join(p)}</svg>\n'


def main():
    for name, t in THEMES.items():
        write(f"hero-{name}.svg", hero(t))
        write(f"how-{name}.svg", how(t))
    if len(sys.argv) > 2:
        ask = open(sys.argv[1]).read().rstrip("\n").splitlines()
        trace = open(sys.argv[2]).read().rstrip("\n").splitlines()

        def ask_color(l):
            if l.startswith("* "):
                return "#2dd4bf"
            if l.startswith("  #") or l.startswith("Sources"):
                return "#8b949e"
            return "#e6edf3"

        # A terminal is dark in either theme: one file each.
        t = THEMES["dark"]
        write("ask.svg", terminal(t, "mnem ask", 'mnem ask "why did customers get charged twice"', ask, ask_color))
        write("trace.svg", terminal(t, "mnem trace", "mnem trace --commits 8 --out .agent-trace", trace,
                                    lambda l: "#2dd4bf" if "written by agents" in l else "#e6edf3"))
    print("assets written to", HERE)


main()
