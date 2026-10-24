#!/usr/bin/env python3
"""Build the static Anamanti website: website-src/pages/*.html -> website/*.html.

Every page shares one <head>, header and footer (website-src/partials/), so the
nav, footer and meta tags are edited in exactly one place. The output is plain
static HTML committed to website/ and deployed as-is (no build step in CI).

Page file format (website-src/pages/<name>.html):

    title: Page title
    description: One-sentence meta description
    og: assets/img/<share-image>          (relative to the site root)
    ---
    <page-specific CSS, may be empty>
    ---
    <the contents of <main>>

Usage: python3 website-src/build.py   (also regenerates sitemap.xml)
"""
import pathlib
import sys

SITE = "https://www.anamanti.com"
ROOT = pathlib.Path(__file__).resolve().parent
OUT = ROOT.parent / "website"
# Pages listed in the sitemap, in nav order. 404 is built but not listed.
SITEMAP = ["index", "core", "display", "hardware", "integrations", "technology", "install", "join"]


def url_path(name: str) -> str:
    return "/" if name == "index" else f"/{name}"


def build_page(src: pathlib.Path, head: str, header: str, footer: str) -> str:
    name = src.stem
    meta, style, main = src.read_text().split("\n---\n", 2)
    fields = dict(line.split(":", 1) for line in meta.strip().splitlines())
    title, desc, og = (fields[k].strip() for k in ("title", "description", "og"))
    canonical = SITE + url_path(name)
    href = url_path(name)
    nav = header.replace(f'<a href="{href}">', f'<a href="{href}" aria-current="page">', 1)
    nav = nav.replace(f'<a class="nav-cta" href="{href}">', f'<a class="nav-cta" href="{href}" aria-current="page">', 1)
    robots = '<meta name="robots" content="noindex" />\n' if name == "404" else ""
    canon = "" if name == "404" else f'<link rel="canonical" href="{canonical}" />\n'
    style = style.strip()
    style_block = f"<style>\n{style}\n</style>\n" if style else ""
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<title>{title}</title>
<meta name="description" content="{desc}" />
{robots}{canon}<meta property="og:type" content="website" />
<meta property="og:site_name" content="Anamanti" />
<meta property="og:title" content="{title}" />
<meta property="og:description" content="{desc}" />
<meta property="og:url" content="{canonical}" />
<meta property="og:image" content="{SITE}/{og}" />
<meta name="twitter:card" content="summary_large_image" />
{head}{style_block}</head>
<body>
{nav}<main id="main">
{main.strip()}
</main>
{footer}<script src="/assets/site.js" defer></script>
</body>
</html>
"""


def main() -> int:
    head = (ROOT / "partials/head.html").read_text()
    header = (ROOT / "partials/header.html").read_text()
    footer = (ROOT / "partials/footer.html").read_text()
    pages = sorted((ROOT / "pages").glob("*.html"))
    for src in pages:
        (OUT / src.name).write_text(build_page(src, head, header, footer))
        print("built", src.name)
    names = {p.stem for p in pages}
    urls = "".join(f"  <url><loc>{SITE}{url_path(n)}</loc></url>\n" for n in SITEMAP if n in names)
    (OUT / "sitemap.xml").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n' + urls + "</urlset>\n")
    print("built sitemap.xml")
    return 0


if __name__ == "__main__":
    sys.exit(main())
