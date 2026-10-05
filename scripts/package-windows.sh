#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
target="x86_64-pc-windows-msvc"
cache_dir="$repo_root/target/windows-package-cache"
output="$repo_root/dist/mousehop-windows-x86_64.zip"
runtime_zip=""

usage() {
    cat <<EOF
Cross-compile and package Mousehop for Windows from macOS.

Usage: $(basename "$0") [--runtime-zip PATH] [--output PATH]

Options:
  --runtime-zip PATH  Existing Mousehop Windows ZIP used as the GTK runtime.
                      If omitted, the newest dist/*windows*x86_64.zip is used.
                      When none exists, the latest official release is cached.
  --output PATH       ZIP output path
                      (default: dist/mousehop-windows-x86_64.zip)
  -h, --help          Show this help

Required one-time tools:
  brew install llvm pkgconf
  cargo install cargo-xwin

The runtime ZIP supplies Windows GTK/libadwaita DLLs. The script recreates the
MSVC import libraries on the Mac, builds the current source with cargo-xwin,
then replaces only mousehop.exe in a fresh runtime directory.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runtime-zip)
            [[ $# -ge 2 ]] || { echo "error: --runtime-zip needs a path" >&2; exit 2; }
            runtime_zip="$2"
            shift 2
            ;;
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
    echo "error: this cross-packaging script must run on macOS" >&2
    exit 1
fi

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required command not found: $1" >&2
        echo "Install local cross-build tools with:" >&2
        echo "  brew install llvm pkgconf" >&2
        echo "  cargo install cargo-xwin" >&2
        exit 1
    fi
}

for command in cargo pkg-config unzip zip shasum; do
    require_command "$command"
done

if ! cargo xwin --version >/dev/null 2>&1; then
    echo "error: cargo-xwin is not installed" >&2
    echo "Install it with: cargo install cargo-xwin" >&2
    exit 1
fi

llvm_prefix="${LLVM_PREFIX:-}"
if [[ -z "$llvm_prefix" ]] && command -v brew >/dev/null 2>&1; then
    llvm_prefix="$(brew --prefix llvm 2>/dev/null || true)"
fi
llvm_readobj="${LLVM_READOBJ:-${llvm_prefix:+$llvm_prefix/bin/llvm-readobj}}"
llvm_dlltool="${LLVM_DLLTOOL:-${llvm_prefix:+$llvm_prefix/bin/llvm-dlltool}}"
if [[ -z "$llvm_readobj" || ! -x "$llvm_readobj" || -z "$llvm_dlltool" || ! -x "$llvm_dlltool" ]]; then
    echo "error: llvm-readobj and llvm-dlltool are required" >&2
    echo "Install them with: brew install llvm" >&2
    exit 1
fi

mkdir -p "$cache_dir" "$(dirname "$output")"

if [[ -z "$runtime_zip" ]]; then
    newest_runtime="$(find "$repo_root/dist" -maxdepth 1 -type f -name '*windows*x86_64.zip' -print 2>/dev/null \
        | while IFS= read -r file; do printf '%s\t%s\n' "$(stat -f '%m' "$file")" "$file"; done \
        | sort -nr | head -1 | cut -f2- || true)"
    if [[ -n "$newest_runtime" ]]; then
        runtime_zip="$newest_runtime"
    else
        require_command curl
        runtime_zip="$cache_dir/mousehop-windows-runtime.zip"
        if [[ ! -f "$runtime_zip" ]]; then
            echo "==> Downloading the latest official Windows GTK runtime"
            curl --fail --location --retry 3 \
                --output "$runtime_zip" \
                "https://github.com/jondkinney/mousehop/releases/latest/download/mousehop-windows-x86_64.zip"
        fi
    fi
fi

if [[ ! -f "$runtime_zip" ]]; then
    echo "error: Windows runtime ZIP not found: $runtime_zip" >&2
    exit 1
fi

runtime_zip="$(cd "$(dirname "$runtime_zip")" && pwd)/$(basename "$runtime_zip")"
runtime_hash="$(shasum -a 256 "$runtime_zip" | awk '{print $1}')"
prefix_schema="1"
prefix_identity="$prefix_schema:$runtime_hash"
prefix="$cache_dir/gtk-prefix"
prefix_stamp="$prefix/.runtime-sha256"

prepare_prefix() {
    local extract_dir="$cache_dir/runtime-extract"
    local def_dir="$cache_dir/defs"
    local dll_source
    local lib_name
    local dll_name
    local dll_path
    local def_path
    local exports

    rm -rf "$extract_dir" "$def_dir" "$prefix"
    mkdir -p "$extract_dir" "$def_dir" "$prefix/bin" "$prefix/lib/pkgconfig"
    unzip -q "$runtime_zip" -d "$extract_dir"

    dll_source="$(find "$extract_dir" -type f -name 'gtk-4-1.dll' -print -quit)"
    if [[ -z "$dll_source" ]]; then
        echo "error: runtime ZIP does not contain gtk-4-1.dll" >&2
        exit 1
    fi
    dll_source="$(dirname "$dll_source")"
    find "$dll_source" -maxdepth 1 -type f -name '*.dll' -exec cp -f {} "$prefix/bin/" \;

    while IFS='|' read -r lib_name dll_name; do
        [[ -n "$lib_name" ]] || continue
        dll_path="$prefix/bin/$dll_name"
        if [[ ! -f "$dll_path" ]]; then
            echo "error: required GTK runtime library is missing: $dll_name" >&2
            exit 1
        fi
        def_path="$def_dir/$lib_name.def"
        {
            echo "LIBRARY $dll_name"
            echo "EXPORTS"
            "$llvm_readobj" --coff-exports "$dll_path" | awk '/^  Name: / { print "  " $2 }'
        } > "$def_path"
        exports="$(awk 'END { print NR - 2 }' "$def_path")"
        if [[ "$exports" -le 0 ]]; then
            echo "error: no exports found in $dll_name" >&2
            exit 1
        fi
        "$llvm_dlltool" -m i386:x86-64 -d "$def_path" -l "$prefix/lib/$lib_name.lib"
    done <<'EOF'
adwaita-1|adwaita-1-0.dll
gtk-4|gtk-4-1.dll
pangocairo-1.0|pangocairo-1.0-0.dll
pangowin32-1.0|pangowin32-1.0-0.dll
pango-1.0|pango-1.0-0.dll
harfbuzz|harfbuzz.dll
gdk_pixbuf-2.0|gdk_pixbuf-2.0-0.dll
cairo-gobject|cairo-gobject-2.dll
cairo|cairo-2.dll
graphene-1.0|graphene-1.0-0.dll
gio-2.0|gio-2.0-0.dll
gobject-2.0|gobject-2.0-0.dll
glib-2.0|glib-2.0-0.dll
intl|intl.dll
EOF

    # gtk-rs build scripts query pkg-config for these package names. Headers
    # are not compiled by the Rust bindings; the generated files only provide
    # versions, search paths, and import-library names for the MSVC linker.
    all_libs="-ladwaita-1 -lgtk-4 -lpangocairo-1.0 -lpangowin32-1.0 -lpango-1.0 -lharfbuzz -lgdk_pixbuf-2.0 -lcairo-gobject -lcairo -lgraphene-1.0 -lgio-2.0 -lgobject-2.0 -lglib-2.0 -lintl"
    for package in glib-2.0 gobject-2.0 gio-2.0 cairo cairo-gobject pango gdk-pixbuf-2.0 graphene-gobject-1.0 gtk4 libadwaita-1; do
        cat > "$prefix/lib/pkgconfig/$package.pc" <<EOF
prefix=$prefix
libdir=\${prefix}/lib

Name: $package
Description: Mousehop Windows cross-build metadata for $package
Version: 99.0.0
Libs: -L\${libdir} $all_libs
Cflags:
EOF
    done

    printf '%s\n' "$prefix_identity" > "$prefix_stamp"
}

if [[ ! -f "$prefix_stamp" || "$(cat "$prefix_stamp")" != "$prefix_identity" ]]; then
    echo "==> Preparing Windows GTK import libraries from $(basename "$runtime_zip")"
    prepare_prefix
else
    echo "==> Reusing cached Windows GTK import libraries"
fi

echo "==> Cross-compiling Mousehop for $target"
cd "$repo_root"
PKG_CONFIG_ALLOW_CROSS_x86_64_pc_windows_msvc=1 \
PKG_CONFIG_LIBDIR_x86_64_pc_windows_msvc="$prefix/lib/pkgconfig" \
PKG_CONFIG_PATH_x86_64_pc_windows_msvc="$prefix/lib/pkgconfig" \
cargo xwin build --release --target "$target" -p mousehop

exe="$repo_root/target/$target/release/mousehop.exe"
if [[ ! -f "$exe" ]]; then
    echo "error: build completed without producing $exe" >&2
    exit 1
fi

staging_root="$cache_dir/package"
staging_dir="$staging_root/mousehop-windows"
rm -rf "$staging_root"
mkdir -p "$staging_dir"
cp -f "$prefix/bin/"*.dll "$staging_dir/"
cp -f "$exe" "$staging_dir/mousehop.exe"

echo "==> Creating Windows ZIP"
rm -f "$output"
(
    cd "$staging_root"
    zip -q -r "$output" mousehop-windows -x '*.DS_Store'
)
unzip -tq "$output" >/dev/null

echo "Done: $output"
