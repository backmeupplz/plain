#!/usr/bin/env python3
"""Renders a share card (1200x630 PNG) for a getplain.org page with the Chrome on this machine.

  python3 docs/_og/card.py docs/og-docs.png "plain docs" "Install, write, test, deploy, verify, upgrade." /docs/

Jekyll skips folders that start with an underscore, so this script isn't published. Set CHROME if Chrome isn't in
the usual macOS place. Point the page's front matter at the card: og_image: /og-docs.png and og_image_alt.
"""
import html, os, subprocess, sys, tempfile

out, title, blurb, path = sys.argv[1:5]
chrome = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
page = f"""<!doctype html><html><head><meta charset="utf-8"><style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
html, body {{ width: 1200px; height: 630px; overflow: hidden; }}
body {{ display: flex; flex-direction: column; padding: 70px 76px; color: #ecebe5;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  background: radial-gradient(720px 520px at 1060px 60px, rgba(76, 195, 138, 0.18), transparent 70%), #111110; }}
.mark {{ font: 700 44px ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: -1px; }}
.mark span {{ color: #4cc38a; }}
h1 {{ margin-top: auto; font-size: 70px; line-height: 1.05; letter-spacing: -2px; font-weight: 700; text-wrap: balance; max-width: 1020px; }}
p {{ margin-top: 24px; font-size: 28px; line-height: 1.4; color: #9a9991; text-wrap: balance; max-width: 1000px; }}
.url {{ margin-top: 32px; font: 500 24px ui-monospace, SFMono-Regular, Menlo, monospace; color: #4cc38a; }}
</style></head><body>
<div class="mark">plain<span>.</span></div>
<h1>{html.escape(title)}</h1><p>{html.escape(blurb)}</p><div class="url">getplain.org{html.escape(path)}</div>
</body></html>"""
with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False) as f:
    f.write(page)
subprocess.run([chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=1",
                "--window-size=1200,630", "--virtual-time-budget=2000", f"--screenshot={os.path.abspath(out)}", f"file://{f.name}"],
               check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
os.unlink(f.name)
print(out, os.path.getsize(out) // 1024, "KB")
