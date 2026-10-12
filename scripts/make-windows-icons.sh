#!/usr/bin/env bash
set -euo pipefail

# Run from the repository root. Generated ICOs are committed so normal builds
# need only a Windows resource compiler, not SVG/ImageMagick tools.
output="mousehop/assets/windows"
work="target/windows-icon-work"
mkdir -p "$output" "$work"
rsvg-convert -w 256 -h 256 mousehop-gtk/resources/com.mousehop.Mousehop.svg -o "$work/app.png"
rsvg-convert -w 256 -h 256 mousehop-gtk/resources/icons/mousehop-tray.svg -o "$work/tray.png"
magick "$work/tray.png" -channel RGB -evaluate set 0 +channel "$work/tray-light.png"
magick "$work/tray.png" -channel RGB -evaluate set 100% +channel "$work/tray-dark.png"
for name in app tray-light tray-dark; do
    magick "$work/$name.png" -define icon:auto-resize=256,128,64,48,40,32,24,20,16 "$output/$name.ico"
done
