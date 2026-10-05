#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

case "$(uname -m)" in
    arm64) artifact_arch="arm64" ;;
    x86_64) artifact_arch="intel" ;;
    *)
        echo "error: unsupported macOS architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

output="$repo_root/dist/mousehop-macos-$artifact_arch.dmg"

usage() {
    cat <<EOF
Build a distributable Mousehop macOS DMG on this Mac.

Usage: $(basename "$0") [--output PATH]

Options:
  --output PATH  DMG output path
                 (default: dist/mousehop-macos-$artifact_arch.dmg)
  -h, --help     Show this help

The script creates an ad-hoc-signed local build when signing credentials are
unset. To produce a Developer ID signed and notarized build, export the
variables documented in scripts/sign-and-notarize.sh before running it.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --output)
            [[ $# -ge 2 ]] || { echo "error: --output needs a path" >&2; exit 2; }
            output="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ "$output" != /* ]]; then
    output="$(pwd)/$output"
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: this packaging script must run on macOS" >&2
    exit 1
fi

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        echo "Install local packaging tools with:" >&2
        echo "  brew install gtk4 libadwaita imagemagick librsvg create-dmg" >&2
        echo "  cargo install cargo-bundle" >&2
        exit 1
    fi
}

for command in cargo brew create-dmg codesign hdiutil iconutil install_name_tool magick otool rsvg-convert; do
    require_command "$command"
done

if ! cargo bundle --help >/dev/null 2>&1; then
    echo "error: cargo-bundle is not installed" >&2
    echo "Install it with: cargo install cargo-bundle" >&2
    exit 1
fi

app="$repo_root/target/release/bundle/osx/Mousehop.app"
working_dmg="$repo_root/target/release/bundle/osx/mousehop-macos-$artifact_arch.dmg"

mkdir -p "$(dirname "$output")" "$(dirname "$working_dmg")"

cd "$repo_root"
echo "==> Generating macOS icons"
"$script_dir/makeicns.sh"

echo "==> Creating Mousehop.app"
# cargo-bundle resolves icon/resource globs from its current directory.
(
    cd "$repo_root/mousehop"
    cargo bundle --release
)

echo "==> Bundling Homebrew dylibs and resources"
"$script_dir/copy-macos-dylib.sh" "$app/Contents/MacOS/mousehop"

echo "==> Signing Mousehop.app"
"$script_dir/sign-and-notarize.sh" --app "$app"

echo "==> Creating DMG"
rm -f "$working_dmg"
create-dmg \
    --volname "Mousehop" \
    --window-pos 200 120 \
    --window-size 600 400 \
    --icon-size 96 \
    --icon "Mousehop.app" 150 200 \
    --app-drop-link 450 200 \
    --no-internet-enable \
    "$working_dmg" "$app"

echo "==> Signing/notarizing DMG when credentials are available"
"$script_dir/sign-and-notarize.sh" --dmg "$working_dmg"

echo "==> Verifying DMG"
hdiutil verify "$working_dmg" >/dev/null

if [[ "$working_dmg" != "$output" ]]; then
    cp -f "$working_dmg" "$output"
fi

echo "Done: $output"
