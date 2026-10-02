#!/usr/bin/env bash
# Renders the site from src/ + brand.json + benchmarks.json.
#   site/dist/           deployable static site: index.html (full document), SKILL.md,
#                        llms.txt, and header files for Vercel / Netlify
#   site/dist/og.png     share card; not built here (needs Chrome): run site/og.sh
#   npm/unlatch/skill/SKILL.md  the same skill, shipped in the npm package
set -euo pipefail
cd "$(dirname "$0")"
python3 - <<'PY'
import json, re, pathlib
brand = json.load(open("brand.json"))
bench = json.load(open("benchmarks.json"))

def lookup(key):
    if key.startswith("pct."):
        b = bench[key[4:]]
        return f"{b['unlatch_v'] / b['sshfs_v'] * 100:.2f}%"
    if key.startswith("pctr."):
        b = bench[key[5:]]
        return f"{b['sshfs_v'] / b['unlatch_v'] * 100:.2f}%"
    obj, parts = (bench, key[2:].split(".")) if key.startswith("b.") else (brand, key.split("."))
    for p in parts:
        obj = obj[p]
    return str(obj)

def render(text):
    out = re.sub(r"\{\{\s*([\w.]+)\s*\}\}", lambda m: lookup(m.group(1)), text)
    left = re.findall(r"\{\{[^}]*\}\}", out)
    if left:
        raise SystemExit(f"unrendered placeholders: {left}")
    return out

# Every number on the page must be the committed scorecard's.
def scorecard_rows(profile):
    md = pathlib.Path("../bench/results/SCORECARD.md").read_text()
    m = re.search(rf"^## {re.escape(profile)}\n(.*?)(?=^## |\Z)", md, re.S | re.M)
    if not m:
        raise SystemExit(f"benchmarks.json: no '## {profile}' section in bench/results/SCORECARD.md")
    rows = {}
    for line in m.group(1).splitlines():
        c = [x.strip() for x in line.split("|")]
        if len(c) >= 11 and re.fullmatch(r"T\d+", c[1]):
            rows[c[2]] = {"unlatch": c[4], "sshfs": c[5], "local": c[6], "pass": c[9], "ratio": c[10]}
    return rows

def cell_num(cell):
    return float(cell.lstrip("~").replace(",", ""))

if "profile" not in bench:
    raise SystemExit("benchmarks.json: missing `profile` (the bench/results/SCORECARD.md section its numbers come from)")
rows = scorecard_rows(bench["profile"])
for key, b in bench.items():
    if not isinstance(b, dict) or "metric" not in b:
        continue
    row = rows.get(b["metric"])
    if row is None:
        raise SystemExit(f"benchmarks.json {key}: {b['metric']} not in the {bench['profile']} scorecard section")
    for field, col in (("unlatch", "unlatch"), ("sshfs", "sshfs"), ("local", "local")):
        if f"{field}_v" not in b:
            continue
        want = cell_num(row[col])
        if b[f"{field}_v"] != want or cell_num(b[field].split()[0]) != want:
            raise SystemExit(f"benchmarks.json {key}.{field} = {b[field]!r}/{b[field + '_v']} but the scorecard says {row[col]} "
                             f"({b['metric']}, {bench['profile']}). Update benchmarks.json from bench/results/SCORECARD.md.")
    if "ratio" in b and b["ratio"] != row["ratio"]:
        raise SystemExit(f"benchmarks.json {key}.ratio = {b['ratio']!r} but the scorecard says {row['ratio']!r} ({b['metric']}, {bench['profile']})")
    if row["pass"] == "❌" and (b.get("target_met") is not False or "target" not in b.get("note", "")):
        raise SystemExit(f"benchmarks.json {key}: the scorecard marks {b['metric']} as missing its target; set target_met false and a note")

page = render(pathlib.Path("src/index.html").read_text())
skill = render(pathlib.Path("src/SKILL.md").read_text())
# The installer's `<npm> skill` ships the same text. Until the npm
# package is renamed to brand.json's `npm`, leave its copy alone: a skill telling agents to run
# `npx -y unlatch ...` must not ship inside the package that is still published as another name.
npm_skill = None
for names in sorted(pathlib.Path("../npm").glob("*/lib/names.js")):
    m = re.search(r"const CLI = '([^']+)'", names.read_text())
    if m and m.group(1) == brand["npm"]:
        npm_skill = names.parent.parent / "skill" / "SKILL.md"
if npm_skill:
    npm_skill.write_text(skill)
else:
    print(f"note: no npm package named {brand['npm']!r} yet; npm/*/skill/SKILL.md left as is")

head, body = page.split("<!--/head-->", 1)
head = re.sub(r"<!--.*?-->", "", head, count=1, flags=re.S)
b = brand
# Favicon: a reverse-video "u" with the magenta block cursor from the headline.
icon = ("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E"
        "%3Crect width='32' height='32' rx='6' fill='%2314171C'/%3E"
        "%3Cpath d='M7.5 9v8.5a5 5 0 0 0 10 0V9' fill='none' stroke='%23ECEEEE' stroke-width='3.6'/%3E"
        "%3Crect x='21' y='9' width='5' height='14' fill='%23F06AAE'/%3E%3C/svg%3E")
og_note = "" if pathlib.Path("dist/og.png").exists() else " (dist/og.png missing: run site/og.sh)"

doc = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="description" content="{b['description']}">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#ECEEEE" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#0F1114" media="(prefers-color-scheme: dark)">
<link rel="canonical" href="{b['site_url']}/">
<link rel="icon" href="{icon}">
<link rel="alternate" type="text/markdown" href="/SKILL.md" title="{b['name']} agent setup">
<meta property="og:type" content="website">
<meta property="og:title" content="{b['name']}: {b['tagline']}">
<meta property="og:description" content="{b['description']}">
<meta property="og:url" content="{b['site_url']}/">
<meta property="og:site_name" content="{b['name']}">
<meta property="og:image" content="{b['site_url']}/og.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="{b['name']}: {b['tagline']} npx {b['npm']}">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:image" content="{b['site_url']}/og.png">
{head.strip()}
<style>:root{{padding-top:env(safe-area-inset-top,0px);padding-bottom:env(safe-area-inset-bottom,0px)}}</style>
</head>
<body>
{body.strip()}
</body>
</html>
"""
dist = pathlib.Path("dist"); dist.mkdir(exist_ok=True)
(dist / "index.html").write_text(doc)
(dist / "SKILL.md").write_text(skill)
(dist / "llms.txt").write_text(skill)
(dist / "_headers").write_text(
    "/SKILL.md\n  Content-Type: text/markdown; charset=utf-8\n"
    "/llms.txt\n  Content-Type: text/plain; charset=utf-8\n")
(dist / "vercel.json").write_text(json.dumps({"headers": [
    {"source": "/SKILL.md", "headers": [{"key": "Content-Type", "value": "text/markdown; charset=utf-8"}]},
    {"source": "/llms.txt", "headers": [{"key": "Content-Type", "value": "text/plain; charset=utf-8"}]},
]}, indent=2) + "\n")
print("built" + (f" {npm_skill}" if npm_skill else "") +
      ", dist/{index.html,SKILL.md,llms.txt,_headers,vercel.json}" + og_note)
PY
