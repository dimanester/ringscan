# RingScan

Disk usage visualizer for Windows: the whole drive as a sunburst chart
(concentric rings, outer ring = deeper folder level, arc angle ∝ size).
A modern remake of Steffen Gerlach's *Scanner* 2.13.

Status: under construction — see [SPEC.md](SPEC.md) for scope and architecture.

## Build (from WSL)

```bash
. ~/.cargo/env
cargo build --release --target x86_64-pc-windows-gnu -p ringscan
# -> target/x86_64-pc-windows-gnu/release/ringscan.exe (single portable exe)
```

Requires `rustup target add x86_64-pc-windows-gnu` and `gcc-mingw-w64-x86-64`.

## Test

```bash
cargo test -p ringscan-core -p ringscan-ntfs                    # Linux
cargo test --target x86_64-pc-windows-gnu -p ringscan-win       # runs on Windows via scripts/winrun.sh
```
