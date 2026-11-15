# ringscan-ntfs

Fast, platform-independent NTFS MFT reader for RingScan. It reads the Master
File Table of a volume through a `ReadAt` trait (an image file in tests, a
raw `\\.\C:` handle in the engine) and returns one `MftFile` per file or
directory: name, parent record, logical/allocated size, attributes, mtime,
hard links, reparse tag. No dependencies; design notes are in the crate docs
(`src/lib.rs`) and the module docs.

```rust
use ringscan_ntfs::{read_mft, MftProgress};
use std::sync::atomic::AtomicBool;

let image = std::fs::File::open("fixtures/rich-c4k-s512.img")?;
let (volume, files) = read_mft(&image, &MftProgress::new(), &AtomicBool::new(false))?;
// files: sorted by record; root = record 5 (parent = 5); `orphan` marks
// entries whose parent reference is stale; `parent_first_order(&files)`
// gives an insertion order with parents before children.
```

`scan_mft` takes `MftOptions` (parse threads, chunk size, readers) and also returns
`MftStats` (record counters, `$MFT` fragmentation, I/O and timing).

## I/O contract

Every read has a 4096-aligned offset and buffer address and a length that is
a multiple of 4096 (only a read ending exactly at the volume end may be a
multiple of the sector size instead), never crosses the volume end and is at
most 16 MiB. Besides the boot sector and record 0, the bootstrap reads
`$MFT:$BITMAP` (a few hundred KiB; its in-use count sizes the file list up
front, so the list is never held twice; files created before the records are
read can outgrow it, and the list then grows by ~3 % instead of doubling,
counted in `MftStats::size_hint_overflows`). The MFT is read sequentially in 4 MiB chunks from one thread
(`MftOptions::readers` > 1 adds concurrent readers), so a Windows `ReadAt` can
be a plain positional `ReadFile` on a volume handle, `FILE_FLAG_NO_BUFFERING`
included.

## Tests

```bash
sudo scripts/make-ntfs-fixtures.sh        # ~4 min, ~2.3 GB of images in fixtures/ (gitignored)
cargo test -p ringscan-ntfs               # unit + synthetic + fixtures + fuzz
cargo run -p ringscan-ntfs --release --example dump -- fixtures/big-c4k-s512.img --replay 5
```

* `tests/synthetic.rs` builds volumes byte by byte (fragmented `$MFT` with an
  `$ATTRIBUTE_LIST`, unaligned runs, a record spanning two runs, stale
  references, cycles, torn records) and runs without fixtures, also on
  Windows (`--target x86_64-pc-windows-gnu`).
* `tests/fixtures.rs` compares every image from
  `scripts/make-ntfs-fixtures.sh` with ntfs-3g's view of it (paths, inode =
  record, sizes, hard links, streams, reparse tags) and checks that every
  read is aligned. Without fixtures it prints how to generate them and passes.
* `tests/robustness.rs` corrupts real records 1 000 000 times and whole
  volumes a few thousand times; nothing may panic or hang.

The fixture generator needs root, `mkntfs`, `ntfs-3g` and `ntfsinfo`; it
mounts only under `fixtures/.mnt` and always unmounts. The Python half lives
in `scripts/ntfs_fixture.py` (run with `python3 -I`).
