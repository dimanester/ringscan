#!/usr/bin/env bash
# Fails unless the given Windows exe imports only DLLs that ship with
# Windows 10/11, i.e. the portable exe needs nothing next to it (no
# libgcc_s_seh-1.dll, libwinpthread-1.dll, vcruntime140.dll, ...).
#
#   scripts/check-exe-imports.sh target/x86_64-pc-windows-gnu/release/ringscan.exe
#
# A new dependency that imports another system DLL (or an ext-ms-win-*
# API set) needs it added to the list below, after checking that it is
# part of every Windows 10/11 edition.
set -euo pipefail

exe="${1:?usage: check-exe-imports.sh FILE.exe}"
objdump="${OBJDUMP:-x86_64-w64-mingw32-objdump}"

system_dlls=(
  advapi32.dll bcrypt.dll bcryptprimitives.dll combase.dll comctl32.dll
  comdlg32.dll crypt32.dll d3d11.dll d3d12.dll dwmapi.dll dwrite.dll dxgi.dll
  gdi32.dll imm32.dll kernel32.dll kernelbase.dll msvcrt.dll ntdll.dll
  ole32.dll oleaut32.dll opengl32.dll propsys.dll secur32.dll setupapi.dll
  shcore.dll shell32.dll shlwapi.dll uiautomationcore.dll user32.dll
  userenv.dll uxtheme.dll version.dll winmm.dll ws2_32.dll
)

imports="$("$objdump" -p "$exe" | awk '/DLL Name:/ { print tolower($3) }' | sort -u)"
if [[ -z "$imports" ]]; then
  echo "error: no imports found in $exe (is it a PE file?)" >&2
  exit 1
fi

bad=0
while read -r dll; do
  ok=0
  # api-ms-win-* API sets are resolved by the loader to system DLLs.
  # ext-ms-win-* sets are optional and missing on some Windows editions,
  # so they are not accepted here; review and list one explicitly if needed.
  [[ "$dll" == api-ms-win-* ]] && ok=1
  for s in "${system_dlls[@]}"; do
    [[ "$dll" == "$s" ]] && ok=1
  done
  if ((ok)); then
    echo "  ok   $dll"
  else
    echo "  BAD  $dll (not a known Windows system DLL)"
    bad=1
  fi
done <<<"$imports"

if ((bad)); then
  echo "error: $exe imports DLLs that are not part of Windows" >&2
  exit 1
fi
echo "$exe imports only system DLLs"
