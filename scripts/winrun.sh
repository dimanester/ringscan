#!/usr/bin/env bash
# Cargo runner for x86_64-pc-windows-gnu: copies the built .exe to a Windows-side
# scratch dir (%TEMP%\ringscan-dev, so it runs from a real NTFS path rather than
# \\wsl.localhost) and executes it through WSL interop, forwarding args, stdio
# and the exit code. Override the dir with RINGSCAN_WIN_DEV (a WSL path).
set -euo pipefail
exe="$1"; shift
if [[ -z "${RINGSCAN_WIN_DEV:-}" ]]; then
  wintemp="$(cd /mnt/c && cmd.exe /c 'echo %TEMP%' 2>/dev/null | tr -d '\r')"
  RINGSCAN_WIN_DEV="$(wslpath -u "$wintemp")/ringscan-dev"
fi
dir="$RINGSCAN_WIN_DEV/run-$$"
mkdir -p "$dir"
cp "$exe" "$dir/"
cd "$dir"
set +e
"./$(basename "$exe")" "$@"
code=$?
set -e
cd /
rm -rf "$dir" 2>/dev/null || true
exit $code
