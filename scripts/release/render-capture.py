#!/usr/bin/env python3
"""Render a tmux capture-pane -e capture to SVG without changing its text."""
import html
from pathlib import Path
import re
import sys

source, destination, theme = sys.argv[1:]
lines = Path(source).read_text().rstrip().splitlines()
foreground, background = ("#d0d0d0", "#14181d") if theme == "dark" else ("#262626", "#ffffff")
width = max(len(re.sub(r"\x1b\[[0-9;]*m", "", line)) for line in lines)
svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width * 8.4 + 40:g}" height="{len(lines) * 21 + 40}" viewBox="0 0 {width * 8.4 + 40:g} {len(lines) * 21 + 40}">',
       '<title>Ballast live terminal capture</title>', f'<rect width="{width * 8.4 + 40:g}" height="{len(lines) * 21 + 40}" rx="10" fill="{background}"/>',
       '<g font-family="Menlo,Consolas,monospace" font-size="14" xml:space="preserve">']
color, bold = foreground, False
for row, line in enumerate(lines):
    column = 0
    for part in re.split(r"(\x1b\[[0-9;]*m)", line):
        if part.startswith("\x1b["):
            codes = [int(code or 0) for code in part[2:-1].split(";")]
            index = 0
            while index < len(codes):
                code = codes[index]
                if code == 0: color, bold = foreground, False
                elif code == 1: bold = True
                elif code == 22: bold = False
                elif code == 39: color = foreground
                elif code == 38 and codes[index + 1] == 2:
                    color = '#%02x%02x%02x' % tuple(codes[index + 2:index + 5])
                    index += 4
                else: raise ValueError(f"Unsupported SGR: {codes}")
                index += 1
        elif part:
            svg.append(f'<text x="{20 + column * 8.4:g}" y="{36 + row * 21}" fill="{color}" font-weight="{700 if bold else 400}" textLength="{len(part) * 8.4:g}" lengthAdjust="spacingAndGlyphs">{html.escape(part).replace(chr(32), "&#160;")}</text>')
            column += len(part)
svg.extend(['</g>', '</svg>'])
Path(destination).write_text('\n'.join(svg) + '\n')
