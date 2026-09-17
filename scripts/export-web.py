#!/usr/bin/env python3
"""Export the explorer frontend for a separate static webserver."""
import argparse
from pathlib import Path
import shutil

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('output', type=Path)
args = parser.parse_args()
web = Path(__file__).resolve().parents[1] / 'web'
args.output.mkdir(parents=True, exist_ok=True)
for name in ('index.html', 'app.js', 'style.css'):
    shutil.copyfile(web / name, args.output / name)
html = (web / 'index.html').read_text().replace(
    '<p class="empty">Loading chain data…</p>', (web / 'protocol.html').read_text())
(args.output / 'protocol.html').write_text(html)
print(f'Exported frontend to {args.output}')
