#!/usr/bin/env python3
"""Render 3DAM's Markdown docs into self-contained, dark-themed HTML pages.

Usage:  python3 build_html.py
Output: docs/html/{index,mission,design_guidelines,product_spec}.html
"""
import re
from pathlib import Path

import markdown

DOCS = Path(__file__).resolve().parent
OUT = DOCS / "html"

# (source .md, output slug, nav label, short subtitle)
PAGES = [
    ("MISSION.md",           "mission",           "Mission",     "Why 3DAM exists"),
    ("DESIGN_GUIDELINES.md", "design_guidelines", "Design",      "How it's built & behaves"),
    ("PRODUCT_SPEC.md",      "product_spec",      "Spec",        "What it is, in detail"),
    ("ROADMAP.md",           "roadmap",           "Roadmap",     "What we build, in what order"),
]
MD_TO_SLUG = {src: slug for src, slug, _, _ in PAGES}

# [[MARKER]] in the Markdown -> injected HTML fragment file (relative to docs/)
FRAGMENTS = {
    "SHOWCASE": "assets/design_showcase.html",
}

CSS = """
:root {
  --bg: #0d1117;
  --bg-soft: #161b22;
  --bg-code: #0a0e14;
  --border: #232a35;
  --border-soft: #1c222c;
  --fg: #d6dee8;
  --fg-muted: #8b98a9;
  --fg-dim: #6b7787;
  --heading: #f0f4f9;
  --accent: #35d0ba;
  --accent-soft: rgba(53, 208, 186, 0.12);
  --accent-line: rgba(53, 208, 186, 0.35);
  --radius: 10px;
  --maxw: 860px;
  --font: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  --mono: "SFMono-Regular", "JetBrains Mono", "Fira Code", ui-monospace, Menlo, Consolas, monospace;
}
* { box-sizing: border-box; }
html { scroll-behavior: smooth; }
body {
  margin: 0;
  font-family: var(--font);
  color: var(--fg);
  background:
    radial-gradient(1200px 600px at 80% -10%, rgba(53,208,186,0.06), transparent 60%),
    radial-gradient(900px 500px at -10% 10%, rgba(80,120,255,0.05), transparent 55%),
    var(--bg);
  line-height: 1.65;
  font-size: 16px;
  -webkit-font-smoothing: antialiased;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }

/* Top bar */
.topbar {
  position: sticky; top: 0; z-index: 20;
  backdrop-filter: blur(10px);
  background: rgba(13,17,23,0.72);
  border-bottom: 1px solid var(--border);
}
.topbar-inner {
  max-width: var(--maxw); margin: 0 auto; padding: 14px 24px;
  display: flex; align-items: center; gap: 22px;
}
.brand { display: flex; align-items: baseline; gap: 10px; font-weight: 700; letter-spacing: .3px; }
.brand .logo { color: var(--heading); font-size: 19px; }
.brand .logo b { color: var(--accent); }
.brand .tag { color: var(--fg-dim); font-size: 12px; font-weight: 500; }
.nav { display: flex; gap: 6px; margin-left: auto; flex-wrap: wrap; }
.nav a {
  color: var(--fg-muted); padding: 6px 12px; border-radius: 8px;
  font-size: 14px; font-weight: 500; border: 1px solid transparent;
}
.nav a:hover { color: var(--fg); background: var(--bg-soft); text-decoration: none; }
.nav a.active { color: var(--accent); background: var(--accent-soft); border-color: var(--accent-line); }

/* Page */
main { max-width: var(--maxw); margin: 0 auto; padding: 48px 24px 96px; }
.subtitle { color: var(--fg-dim); font-size: 14px; text-transform: uppercase; letter-spacing: 2px; margin: 0 0 6px; }

h1, h2, h3, h4 { color: var(--heading); line-height: 1.25; font-weight: 700; }
h1 { font-size: 2.1rem; margin: .2em 0 .6em; letter-spacing: -.5px; }
h2 {
  font-size: 1.4rem; margin: 2.4em 0 .8em; padding-bottom: .35em;
  border-bottom: 1px solid var(--border);
}
h2::before {
  content: ""; display: inline-block; width: 8px; height: 8px; margin-right: 12px;
  border-radius: 2px; background: var(--accent); vertical-align: middle;
  transform: translateY(-2px);
}
h3 { font-size: 1.12rem; margin: 1.8em 0 .5em; color: #cdd7e3; }
h4 { font-size: 1rem; margin: 1.4em 0 .4em; color: var(--fg-muted); }

p { margin: .8em 0; }
strong { color: var(--heading); font-weight: 650; }
hr { border: none; border-top: 1px solid var(--border); margin: 2.6em 0; }

ul, ol { padding-left: 1.4em; }
li { margin: .35em 0; }
li::marker { color: var(--accent); }

/* Inline + block code */
code {
  font-family: var(--mono); font-size: .88em;
  background: var(--bg-soft); border: 1px solid var(--border-soft);
  padding: .12em .4em; border-radius: 5px; color: #a7f3e4;
}
pre {
  background: var(--bg-code); border: 1px solid var(--border);
  border-radius: var(--radius); padding: 18px 20px; overflow-x: auto;
  box-shadow: inset 0 0 0 1px rgba(255,255,255,0.01);
}
pre code {
  background: none; border: none; padding: 0; color: #9fb0c3;
  font-size: .84em; line-height: 1.55;
}

/* Tables */
table {
  border-collapse: collapse; width: 100%; margin: 1.2em 0; font-size: .95em;
  border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden;
}
th, td { text-align: left; padding: 10px 14px; border-bottom: 1px solid var(--border-soft); }
th { background: var(--bg-soft); color: var(--heading); font-weight: 600; }
tr:last-child td { border-bottom: none; }
tbody tr:hover { background: rgba(255,255,255,0.015); }

/* Blockquote */
blockquote {
  margin: 1.2em 0; padding: .6em 1.1em; color: var(--fg-muted);
  border-left: 3px solid var(--accent-line); background: var(--accent-soft);
  border-radius: 0 8px 8px 0;
}

/* Footer nav between docs */
.docnav {
  display: flex; justify-content: space-between; gap: 16px; flex-wrap: wrap;
  margin-top: 3.5em; padding-top: 1.8em; border-top: 1px solid var(--border);
}
.docnav a {
  flex: 1 1 220px; padding: 14px 18px; border: 1px solid var(--border);
  border-radius: var(--radius); background: var(--bg-soft); color: var(--fg);
}
.docnav a:hover { border-color: var(--accent-line); text-decoration: none; }
.docnav a small { display: block; color: var(--fg-dim); font-size: 12px; text-transform: uppercase; letter-spacing: 1px; }
.docnav a span { color: var(--accent); font-weight: 600; }
.docnav a.next { text-align: right; }

.footer { max-width: var(--maxw); margin: 0 auto; padding: 0 24px 60px; color: var(--fg-dim); font-size: 13px; }

/* Landing cards */
.hero { text-align: center; padding: 40px 0 20px; }
.hero h1 { font-size: 3rem; }
.hero .lede { color: var(--fg-muted); font-size: 1.15rem; max-width: 640px; margin: 0 auto; }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 18px; margin: 40px 0; }
.card {
  display: block; padding: 22px; border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--bg-soft); color: var(--fg);
}
.card:hover { border-color: var(--accent-line); transform: translateY(-2px); text-decoration: none; }
.card { transition: border-color .15s ease, transform .15s ease; }
.card .k { color: var(--accent); font-size: 12px; text-transform: uppercase; letter-spacing: 2px; }
.card h3 { margin: 8px 0 6px; color: var(--heading); }
.card p { margin: 0; color: var(--fg-muted); font-size: .95em; }

@media (max-width: 600px) {
  body { font-size: 15px; }
  h1 { font-size: 1.7rem; } .hero h1 { font-size: 2.2rem; }
  main { padding: 32px 18px 72px; }
  .topbar-inner { padding: 12px 18px; }
}
""".strip()


def page_shell(title, subtitle, body, active_slug, docnav=""):
    nav = "".join(
        f'<a href="{slug}.html" class="{ "active" if slug==active_slug else "" }">{label}</a>'
        for _, slug, label, _ in PAGES
    )
    home_active = "active" if active_slug == "index" else ""
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · 3DAM</title>
<style>{CSS}</style>
</head>
<body>
<header class="topbar"><div class="topbar-inner">
  <a class="brand" href="index.html" style="color:inherit">
    <span class="logo">3D<b>AM</b></span>
    <span class="tag">asset manager</span>
  </a>
  <nav class="nav">
    <a href="index.html" class="{home_active}">Home</a>
    {nav}
  </nav>
</div></header>
<main>
{f'<p class="subtitle">{subtitle}</p>' if subtitle else ''}
{body}
{docnav}
</main>
<footer class="footer">3DAM — free &amp; open-source game asset manager · MIT licensed</footer>
</body>
</html>
"""


def build_docnav(idx):
    prev_link = next_link = ""
    if idx > 0:
        s, slug, label, sub = PAGES[idx - 1]
        prev_link = f'<a class="prev" href="{slug}.html"><small>Previous</small><span>&larr; {label}</span></a>'
    else:
        prev_link = '<a class="prev" href="index.html"><small>Back</small><span>&larr; Home</span></a>'
    if idx < len(PAGES) - 1:
        s, slug, label, sub = PAGES[idx + 1]
        next_link = f'<a class="next" href="{slug}.html"><small>Next</small><span>{label} &rarr;</span></a>'
    return f'<nav class="docnav">{prev_link}{next_link}</nav>'


def convert():
    OUT.mkdir(exist_ok=True)
    md = markdown.Markdown(extensions=["extra", "toc", "sane_lists", "attr_list"])

    for idx, (src, slug, label, subtitle) in enumerate(PAGES):
        text = (DOCS / src).read_text(encoding="utf-8")
        # Rewrite links to sibling .md docs -> .html slugs
        for md_name, target_slug in MD_TO_SLUG.items():
            text = text.replace(f"]({md_name})", f"]({target_slug}.html)")
        md.reset()
        html_body = md.convert(text)
        # Inject rich HTML fragments at [[MARKER]] placeholders (e.g. the design showcase).
        for marker, frag_path in FRAGMENTS.items():
            token = f"<p>[[{marker}]]</p>"
            if token in html_body:
                fragment = (DOCS / frag_path).read_text(encoding="utf-8")
                html_body = html_body.replace(token, fragment)
        html = page_shell(label, subtitle, html_body, slug, build_docnav(idx))
        (OUT / f"{slug}.html").write_text(html, encoding="utf-8")
        print(f"  wrote html/{slug}.html")

    # Landing page
    card_blurbs = {
        "mission": "Problem, answer, and principles.",
        "design_guidelines": "Rules for how 3DAM is built and behaves.",
        "product_spec": "Data model, features, architecture, and tech stack.",
        "roadmap": "What we build, in what order, and why.",
    }
    cards = "".join(
        f'<a class="card" href="{slug}.html"><div class="k">{sub}</div>'
        f'<h3>{label}</h3><p>{card_blurbs.get(slug, sub)}</p></a>'
        for src, slug, label, sub in PAGES
    )
    hero = f"""
<section class="hero">
  <h1>3D<span style="color:var(--accent)">AM</span></h1>
  <p class="lede">A free, open-source asset manager for game developers — sound, image, and 3D
  models in one fast, searchable, content-aware library.</p>
</section>
<div class="cards">{cards}</div>
<p style="text-align:center;color:var(--fg-dim);font-size:14px">
  Native Rust · desktop&nbsp;+&nbsp;CLI · local-first · no cloud, no subscription
</p>
"""
    (OUT / "index.html").write_text(page_shell("Home", "", hero, "index"), encoding="utf-8")
    print("  wrote html/index.html")


if __name__ == "__main__":
    print("Building 3DAM HTML docs…")
    convert()
    print(f"Done → {OUT}")
