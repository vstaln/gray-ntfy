#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-ntfy.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-ntfy" ;;
    *) rel="x86_64-apple-darwin/gray-ntfy" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-ntfy" ;;
    *) rel="x86_64-unknown-linux-musl/gray-ntfy" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-ntfy.exe" "$here/target/release/gray-ntfy" "$here/target/debug/gray-ntfy.exe" "$here/target/debug/gray-ntfy"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-ntfy binary not found in $here" >&2
exit 1
