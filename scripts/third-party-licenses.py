#!/usr/bin/env python3
"""Write THIRD-PARTY-LICENSES.txt: every crate the build links, its licence, and the
licence texts the crate ships, as MIT, Apache-2.0 and the like require for binaries.
Also notes ONNX Runtime (linked statically in --features fastembed builds).

    python3 scripts/third-party-licenses.py [--features fastembed] [--target T] > OUT

Fails if a crate declares no licence and ships no licence file.
"""
import json, os, subprocess, sys

args = sys.argv[1:]
cmd = ["cargo", "metadata", "--format-version", "1", "--locked"]
target = None
if "--features" in args:
    cmd += ["--features", args[args.index("--features") + 1]]
if "--target" in args:
    target = args[args.index("--target") + 1]
    cmd += ["--filter-platform", target]
meta = json.loads(subprocess.check_output(cmd))
pkgs = {p["id"]: p for p in meta["packages"]}
root = meta["resolve"]["root"]
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}

# Only what the binary links: normal (and build) dependencies reachable from ravnori,
# not dev-dependencies.
seen, todo = set(), [root]
while todo:
    nid = todo.pop()
    if nid in seen:
        continue
    seen.add(nid)
    for d in nodes[nid]["deps"]:
        if any(k.get("kind") in (None, "build") for k in d["dep_kinds"]):
            todo.append(d["pkg"])
seen.discard(root)

LICENSE_NAMES = ("license", "licence", "copying", "notice", "unlicense")
out, missing = [], []
# Identical licence texts (the same Apache-2.0 in hundreds of crates) are printed once
# and referred to after that.
texts: dict = {}
for pid in sorted(seen, key=lambda i: (pkgs[i]["name"], pkgs[i]["version"])):
    p = pkgs[pid]
    d = os.path.dirname(p["manifest_path"])
    files = sorted(
        f for f in os.listdir(d)
        if f.lower().startswith(LICENSE_NAMES) and os.path.isfile(os.path.join(d, f))
    )
    lic = p.get("license") or (f"see {p['license_file']}" if p.get("license_file") else None)
    if not lic and not files:
        missing.append(f"{p['name']} {p['version']}")
        continue
    out.append("=" * 78)
    out.append(f"{p['name']} {p['version']}  ({lic or 'licence file only'})")
    if p.get("repository"):
        out.append(p["repository"])
    out.append("=" * 78)
    for f in files:
        try:
            text = open(os.path.join(d, f), encoding="utf-8", errors="replace").read().strip()
        except OSError:
            continue
        key = " ".join(text.split())
        if key in texts:
            out.append(f"--- {f}: same text as {texts[key]} ---")
            continue
        texts[key] = f"{p['name']} {p['version']} {f}"
        out.append(f"--- {f} ---")
        out.append(text)
    out.append("")

if missing:
    sys.exit("no licence declared or shipped by: " + ", ".join(missing))

header = [
    "Third-party software in ravnori",
    "",
    "ravnori is licensed under the GNU Affero General Public License v3.0 (see LICENSE).",
    "This binary also contains the following third-party software, each under its own",
    "licence, reproduced below as those licences require.",
    "",
]
if "fastembed" in " ".join(args):
    header += [
        "ONNX Runtime (Microsoft, MIT License, https://github.com/microsoft/onnxruntime)",
        "is linked statically into this build through the ort crate.",
        "",
    ]
print("\n".join(header + out))
