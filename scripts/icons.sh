#!/usr/bin/env bash
# Renders every icon the application and its packaging need, from the one
# master in assets/remail.svg.
#
# The results are committed, so neither a build nor CI needs a renderer
# installed. Run this after editing the SVG:
#
#     ./scripts/icons.sh
#
# Requires rsvg-convert (librsvg2-bin) and ImageMagick.
set -euo pipefail
cd "$(dirname "$0")/.."

# hicolor sizes, plus 512 for the window icon and for stores that want it.
png_sizes=(16 24 32 48 64 128 256 512)

# An .ico stores each dimension in a single byte, so 256 is the ceiling.
ico_sizes=(16 24 32 48 64 128 256)

for size in "${png_sizes[@]}"; do
    rsvg-convert -w "$size" -h "$size" assets/remail.svg -o "assets/icons/remail-$size.png"
done

ico_inputs=()
for size in "${ico_sizes[@]}"; do
    ico_inputs+=("assets/icons/remail-$size.png")
done
# One file holding every size; Explorer picks per context. The 256 is stored
# PNG-compressed, which is what keeps the file small.
convert "${ico_inputs[@]}" assets/remail.ico

echo "wrote ${#png_sizes[@]} PNGs and assets/remail.ico"
