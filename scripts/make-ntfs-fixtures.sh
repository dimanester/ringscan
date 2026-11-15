#!/usr/bin/env bash
# Builds NTFS test images for ringscan-ntfs with mkntfs + ntfs-3g.
#
#   scripts/make-ntfs-fixtures.sh [name ...]     (default: all images)
#
# Output (gitignored): fixtures/<name>.img and fixtures/<name>.json, the ground
# truth written by crates/ntfs/scripts/ntfs_fixture.py from a fresh read-only
# mount (os.lstat of every path; inode = MFT record number under ntfs-3g) plus
# ntfsinfo facts about fragmentation. Needs root (mkntfs on files, FUSE
# mounts), mkntfs, ntfs-3g, ntfsinfo and python3. Images are sparse files
# (all of them: ~4 GB apparent, ~2.3 GB on disk, ~5 minutes); everything
# mounts at fixtures/.mnt and is always unmounted on exit.
#
# Images (cluster / sector size, profile):
#   rich-c4k-s512    4096 / 512   all features (see ntfs_fixture.py: deep nesting,
#   rich-c512-s512    512 / 512     5000-entry dir, hard links, sparse, compressed,
#   rich-c4k-s4096   4096 / 4096    ADS, Unicode + 255-char names, deleted files,
#   rich-c64k-s4096 65536 / 4096    reparse points, DOS names, a file with ~1200
#                                   fragments and an $ATTRIBUTE_LIST)
#   many-c4k-s512    4096 / 512   47k files, $MFT fragmented into ~1800 runs with
#   many-c512-s512    512 / 512     an $ATTRIBUTE_LIST (fill, delete half, refill)
#   big-c4k-s512     4096 / 512   ~2 GB volume, 150k files, large files far in
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIX="$ROOT/fixtures"
MNT="$FIX/.mnt"
GEN="$ROOT/crates/ntfs/scripts/ntfs_fixture.py"
NTFS_PID=""

ALL=(rich-c4k-s512 rich-c512-s512 rich-c4k-s4096 rich-c64k-s4096 many-c4k-s512 many-c512-s512 big-c4k-s512)

for tool in mkntfs ntfs-3g ntfsinfo python3 fusermount mountpoint; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done
[[ $EUID -eq 0 ]] || { echo "run as root (mkntfs on image files and FUSE mounts)" >&2; exit 1; }

mkdir -p "$FIX" "$MNT"
# A previous run that was killed may have left its mount behind.
if mountpoint -q "$MNT"; then
  fusermount -u "$MNT" 2>/dev/null || umount -l "$MNT"
fi

unmount() {
  if mountpoint -q "$MNT"; then
    fusermount -u "$MNT" 2>/dev/null || umount "$MNT" 2>/dev/null || umount -l "$MNT"
  fi
  # ntfs-3g runs with no_detach in the background: wait until it has
  # flushed and exited, so the image is consistent before anyone reads it.
  if [[ -n "$NTFS_PID" ]]; then
    wait "$NTFS_PID" 2>/dev/null || true
    NTFS_PID=""
  fi
}
trap unmount EXIT
trap 'unmount; exit 130' INT TERM

mount_image() { # image options
  # Its chatter (mount banners, plugin hints) goes to a log next to the images.
  ntfs-3g -o "no_detach,$2" "$1" "$MNT" 2>>"$FIX/ntfs-3g.log" &
  NTFS_PID=$!
  for _ in $(seq 100); do
    mountpoint -q "$MNT" && return 0
    sleep 0.1
  done
  echo "mount of $1 did not appear" >&2
  exit 1
}

build() { # name size cluster sector profile
  local name=$1 size=$2 cluster=$3 sector=$4 profile=$5
  local img="$FIX/$name.img" json="$FIX/$name.json" manifest="$FIX/$name.manifest.json"
  local t0=$SECONDS
  echo "== $name: $size, cluster $cluster, sector $sector, profile $profile"
  rm -f "$img" "$json" "$manifest"
  truncate -s "$size" "$img"
  local log
  if ! log=$(mkntfs -F -Q -c "$cluster" -s "$sector" -L "$name" "$img" 2>&1); then
    echo "$log" >&2
    exit 1
  fi
  mount_image "$img" streams_interface=windows,compression
  # mkntfs uses 1 KiB MFT records, or the sector size if that is larger.
  local record=$(( sector > 1024 ? sector : 1024 ))
  python3 -I "$GEN" populate "$MNT" "$profile" "$cluster" "$record" "$manifest"
  unmount
  mount_image "$img" ro,streams_interface=windows
  python3 -I "$GEN" truth "$MNT" "$manifest" "$json"
  unmount
  python3 -I "$GEN" facts "$img" "$json" mft '$MFT'
  python3 -I - "$manifest" <<'PY' | while IFS= read -r p; do python3 -I "$GEN" facts "$img" "$json" "frag:$p" "/$p" </dev/null; done
import json, sys
for p in json.load(open(sys.argv[1], encoding='utf-8'))['fragmented']:
    print(p)
PY
  rm -f "$manifest"
  echo "   done in $((SECONDS - t0)) s: $(du -h --apparent-size "$img" | cut -f1) apparent, $(du -h "$img" | cut -f1) on disk"
}

names=("$@")
[[ ${#names[@]} -gt 0 ]] || names=("${ALL[@]}")
for n in "${names[@]}"; do
  case "$n" in
    rich-c4k-s512)   build "$n" 128M 4096 512 rich ;;
    rich-c512-s512)  build "$n" 128M 512 512 rich ;;
    rich-c4k-s4096)  build "$n" 160M 4096 4096 rich ;;
    rich-c64k-s4096) build "$n" 640M 65536 4096 rich ;;
    many-c4k-s512)   build "$n" 128M 4096 512 many ;;
    many-c512-s512)  build "$n" 128M 512 512 many ;;
    big-c4k-s512)    build "$n" 2G 4096 512 big ;;
    *) echo "unknown image: $n (known: ${ALL[*]})" >&2; exit 2 ;;
  esac
done
echo "fixtures ready in $FIX"
