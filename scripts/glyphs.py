#!/usr/bin/env python3
"""Check that every glyph the interface draws exists in egui's bundled fonts.

egui ships a small font set: Ubuntu-Light, a subset of Noto Emoji, and an icon
font. A codepoint outside it renders as an empty box, silently, and only once
the interface is on screen. Several shipped that way before this check
existed — a printer, a card index, two disclosure triangles.

Run after changing src/ui/icons.rs or SpecialUse::icon():

    python3 scripts/glyphs.py

Requires fonttools (pip install fonttools).
"""

import glob
import re
import sys
import unicodedata

FONT_DIR = "epaint_default_fonts-*/fonts"
# Codepoints that are meant to be invisible. They have no glyph by design, and
# the renderer strips them before drawing (see `html::layout::is_invisible`),
# so they appear in the source only as the bounds of that filter.
INVISIBLE = (
    {0x034F, 0xFEFF, 0x00AD, 0x00A0}
    | set(range(0x200B, 0x2010))  # zero-width spaces, joiners, bidi marks
    | set(range(0x2028, 0x202F))  # separators and bidi overrides
    | set(range(0x2060, 0x2065))  # word joiner, invisible operators
)


def bundled_codepoints() -> set[int]:
    from fontTools.ttLib import TTFont

    roots = glob.glob(f"{glob.escape(_registry())}/{FONT_DIR}/*.ttf")
    if not roots:
        sys.exit("could not find epaint's bundled fonts; build the project first")

    covered: set[int] = set()
    for path in roots:
        font = TTFont(path, fontNumber=0, lazy=True)
        for table in font["cmap"].tables:
            covered |= set(table.cmap.keys())
    return covered


def _registry() -> str:
    import os

    home = os.path.expanduser("~/.cargo/registry/src")
    entries = glob.glob(f"{home}/*")
    if not entries:
        sys.exit("no cargo registry found")
    return entries[0]


def used_codepoints() -> dict[int, set[str]]:
    """Every non-ASCII codepoint in a Rust string literal under src/."""
    found: dict[int, set[str]] = {}
    for path in glob.glob("src/**/*.rs", recursive=True):
        text = open(path, encoding="utf-8").read()
        for match in re.finditer(r"\\u\{([0-9a-fA-F]+)\}", text):
            found.setdefault(int(match.group(1), 16), set()).add(path)
        for char in text:
            if ord(char) > 0x2000 and not char.isalpha():
                found.setdefault(ord(char), set()).add(path)
    return found


def main() -> int:
    covered = bundled_codepoints()
    missing = []
    for codepoint, files in sorted(used_codepoints().items()):
        if codepoint in covered or codepoint in INVISIBLE:
            continue
        name = unicodedata.name(chr(codepoint), "?")
        missing.append(f"  U+{codepoint:05X}  {name}\n      {', '.join(sorted(files))}")

    if missing:
        print("These will render as empty boxes:\n")
        print("\n".join(missing))
        return 1

    print("All glyphs are covered by the bundled fonts.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
