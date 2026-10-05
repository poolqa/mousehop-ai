#!/usr/bin/env bash
set -euo pipefail

command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }
script_dir="$(python3 -c 'import os, sys; print(os.path.dirname(os.path.realpath(sys.argv[1])))' "${BASH_SOURCE[0]}")"
repo_root="$(dirname "$script_dir")"
package_zip=""
runtime_zip=""
output="$repo_root/dist/mousehop-windows-x86_64-setup.exe"
usage() {
    cat <<'HELP'
Build a Windows x64 installer with NSIS (on macOS: brew install nsis).

Usage: scripts/package-windows-installer.sh [options]
  --package-zip PATH  Package an already-built service-capable Mousehop ZIP.
                     Otherwise run package-windows.sh to build current source.
                     The native installer helper is always built from source.
  --runtime-zip PATH  GTK runtime ZIP forwarded to package-windows.sh.
  --output PATH      Setup EXE (default: dist/mousehop-windows-x86_64-setup.exe).
  -h, --help         Show help.

The installer requires administrator consent. It installs into Program Files,
registers the LocalSystem Mousehop service, and preserves ProgramData settings
on upgrades/uninstall. No PowerShell is required on the Windows machine.
Windows 10/11 x64; physical console session only.
The native helper requires cargo-xwin on macOS, or MSVC Rust on Windows.
HELP
}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --package-zip|--runtime-zip|--output)
            [[ $# -ge 2 && -n "$2" ]] || { echo "error: $1 requires a path" >&2; exit 2; }
            case "$1" in
                --package-zip) package_zip="$2" ;;
                --runtime-zip) runtime_zip="$2" ;;
                --output) output="$2" ;;
            esac
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[[ -z "$package_zip" || -z "$runtime_zip" ]] || { echo "error: choose --package-zip or --runtime-zip" >&2; exit 2; }
for tool in cargo makensis python3; do
    command -v "$tool" >/dev/null || { echo "error: missing $tool (macOS: brew install nsis)" >&2; exit 1; }
done
[[ "$output" = /* ]] || output="$PWD/$output"
if [[ -z "$package_zip" ]]; then
    package_zip="$repo_root/dist/mousehop-windows-x86_64.zip"
    args=(--output "$package_zip")
    [[ -z "$runtime_zip" ]] || args+=(--runtime-zip "$runtime_zip")
    bash "$script_dir/package-windows.sh" "${args[@]}"
fi
[[ -f "$package_zip" ]] || { echo "error: ZIP not found: $package_zip" >&2; exit 1; }
staging="$(mktemp -d "${TMPDIR:-/tmp}/mousehop-installer.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
# Extract only a flat EXE/DLL payload, never an arbitrary path from the archive.
# Both release ZIP layouts (flat and mousehop-windows/) are accepted.
python3 - "$package_zip" "$staging" "$repo_root/Cargo.toml" <<'PY'
import pathlib, re, struct, sys, zipfile
archive, staging, manifest = sys.argv[1:]
root = pathlib.Path(staging)
with zipfile.ZipFile(archive) as z:
    names = z.namelist()
    candidates = [n for n in names if pathlib.PurePosixPath(n).name == 'mousehop.exe']
    if len(candidates) != 1:
        raise SystemExit('error: ZIP must contain exactly one mousehop.exe')
    parent = pathlib.PurePosixPath(candidates[0]).parent
    seen = set()
    for item in z.infolist():
        path = pathlib.PurePosixPath(item.filename)
        if item.is_dir():
            continue
        if '..' in path.parts or path.is_absolute() or '\\' in item.filename or ':' in item.filename:
            raise SystemExit('error: unsafe ZIP path')
        if path.parent != parent or (path.name != 'mousehop.exe' and path.suffix.lower() != '.dll'):
            continue
        if path.name.lower() in seen:
            raise SystemExit('error: duplicate Windows filename')
        seen.add(path.name.lower())
        (root / path.name).write_bytes(z.read(item))
exe = (root / 'mousehop.exe').read_bytes()
# An old portable binary has no SCM entry point; don't produce a broken installer.
if b'windows-worker' not in exe or b'StartServiceCtrlDispatcherW' not in exe:
    raise SystemExit('error: ZIP predates Windows Service support; rebuild current source')
if len(exe) < 64 or exe[:2] != b'MZ':
    raise SystemExit('error: mousehop.exe is not a PE executable')
pe = struct.unpack_from('<I', exe, 60)[0]
if pe + 6 > len(exe) or exe[pe:pe+4] != b'PE\0\0' or struct.unpack_from('<H', exe, pe+4)[0] != 0x8664:
    raise SystemExit('error: installer requires a Windows x64 executable')
if not (root / 'gtk-4-1.dll').is_file():
    raise SystemExit('error: ZIP is missing the GTK runtime')
text = pathlib.Path(manifest).read_text()
version = re.search(r'^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"', text, re.M)
if not version:
    raise SystemExit('error: cannot determine workspace version')
(root / 'version.txt').write_text(version[1])
PY
echo "==> Building the native Windows installer helper"
installer_helper="$repo_root/target/x86_64-pc-windows-msvc/release/mousehop-installer.exe"
helper_args=(build --release --target x86_64-pc-windows-msvc --manifest-path "$repo_root/Cargo.toml" -p mousehop-installer)
# Static CRT: the bootstrap helper runs before GTK/runtime DLLs are extracted.
# It must depend only on Windows system libraries.
if [[ "$(uname -s)" == "Darwin" ]]; then
    CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static" cargo xwin "${helper_args[@]}"
else
    CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static" cargo "${helper_args[@]}"
fi
[[ -s "$installer_helper" ]] || { echo "error: native installer helper was not built" >&2; exit 1; }
mkdir -p "$(dirname "$output")"
makensis -V3 "-DINSTALLER_HELPER=$installer_helper" "-DPAYLOAD=$staging" "-DSETUP_OUTPUT=$staging/mousehop-windows-x86_64-setup.exe" \
    "-DAPP_VERSION=$(cat "$staging/version.txt")" "$script_dir/windows/installer.nsi"
[[ -s "$staging/mousehop-windows-x86_64-setup.exe" ]] || { echo "error: NSIS produced no installer" >&2; exit 1; }
mv -f "$staging/mousehop-windows-x86_64-setup.exe" "$output"
echo "Done: $output"
