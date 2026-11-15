#!/usr/bin/env bash
# Prints the release notes for one version: its section of CHANGELOG.md
# (without the heading) followed by download and verification notes.
#
#   scripts/release-notes.sh 0.1.0 > notes.md
#
# Fails when CHANGELOG.md has no "## [VERSION]" section or the section is
# empty, so a release cannot go out without notes. Used by
# .github/workflows/release.yml.
set -euo pipefail

version="${1:?usage: release-notes.sh VERSION}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
changelog="$root/CHANGELOG.md"

section="$(
  awk -v want="## [$version]" '
    # A version heading looks like "## [0.1.0] - 2026-11-15".
    index($0, "## [") == 1 {
      if (found) exit
      if (index($0, want) == 1) { found = 1; next }
    }
    # Link reference definitions at the end of the file are not notes.
    found && /^\[[^]]+\]: / { next }
    found { print }
  ' "$changelog"
)"

# Trim leading and trailing blank lines.
section="$(printf '%s\n' "$section" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')"

if [[ -z "${section//[[:space:]]/}" ]]; then
  echo "error: CHANGELOG.md has no notes for version $version" >&2
  exit 1
fi

printf '%s\n' "$section"
cat <<EOF

### Download and verify

Download \`RingScan.exe\` below: a single portable exe for Windows 10 and 11
(x64), no installer. It is not code-signed, so SmartScreen may say
"Windows protected your PC": choose **More info** > **Run anyway**.

To check that the file is the one built by GitHub Actions from this
repository, compare its SHA-256 with \`RingScan.exe.sha256\`
(PowerShell: \`Get-FileHash .\\RingScan.exe\`) or verify its build
provenance with the GitHub CLI:

\`\`\`
gh attestation verify RingScan.exe -R dimanester/ringscan
\`\`\`
EOF
