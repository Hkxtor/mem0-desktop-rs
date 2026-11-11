# mem0-desktop (Rust)

Mem0 as a single portable desktop app: one executable, one data folder, no Docker, no
external database, no account. The dashboard and the REST API are served from the same
process on loopback.

- **Single binary** (~40 MB; ships as a ~19 MB tar.gz): Axum HTTP backend, the memory
  pipeline on a byte-verified Rust port of mem0's prompts (`Hkxtor/mem0-rs`, branch
  `fidelity-fixes`), vector storage on sqlite-vec (native metadata filters, FTS5 hybrid
  search, no HNSW index), and a Tauri 2 shell that embeds the dashboard unchanged.
- **Same API contract**: 47/47 golden routes, 10/10 prompt parity against the Python SDK.
- **Local-only by default**: binds 127.0.0.1 with auth disabled; bind a public interface
  only deliberately.

## Layout

```
server-rs/          Rust workspace (axum server + Tauri shell + sqlite-vec store)
  crates/app/       HTTP API, auth, store glue, golden/contract tests
  crates/desktop/   Tauri shell (window, tray, single-instance lock)
  crates/vector-sqlitevec/  VectorStore over sqlite-vec + FTS5
  scripts/build_desktop_rs.sh   packager -> dist/mem0-desktop-<os>-<arch>-rs.tar.gz
server/dashboard/   Next.js dashboard source (exported and embedded into the binary)
.github/workflows/  desktop-build-rs.yml (3-platform artifacts), rust-perf.yml (perf gate)
```

## Build

Requires Rust (stable), Node 22 + pnpm 10.

```bash
cd server/dashboard && pnpm install --frozen-lockfile && pnpm build && cd ../..
bash server-rs/scripts/build_desktop_rs.sh        # -> dist/*.tar.gz
```

## Run

```bash
./mem0-desktop            # GUI (Tauri window + tray)
./mem0-desktop --headless # HTTP only, for servers/CI
```

Data lives in `./data/` next to the binary (override with `MEM0_DATA_DIR`).

## Test

```bash
cd server-rs && cargo test --workspace
```

## Credit

The memory pipeline, prompts and the dashboard are derived from
[mem0](https://github.com/mem0ai/mem0) (Apache-2.0). The Rust port of that pipeline lives in
[Hkxtor/mem0-rs](https://github.com/Hkxtor/mem0-rs), pinned by revision in
`server-rs/Cargo.toml`. This repository is the desktop packaging on top of both.

Licensed under the Apache License 2.0 — see [LICENSE](LICENSE).
