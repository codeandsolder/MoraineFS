# MoraineFS

MoraineFS is an experimental single-node tiered storage layer that presents an ordinary FUSE filesystem while moving data across fast disposable tiers and slower durable/object-backed tiers according to policy.

The current implementation is the consolidated successor to the `io-tierfs` prototype. The foreground filesystem/data path remains a small low-level libfuse C core, while the asynchronous control plane is Rust: checkpoint/convergence, crash recovery, hot-object admission, and stale range-cache collection. The longer-term storage model is content-addressed and is intended to expose a pragmatic S3-compatible object surface alongside the filesystem mount.

## Status

This repository is the canonical development source and is still experimental. The existing cold-storage deployment intentionally continues to use its `io-tierfs` runtime paths and services until a separately validated promotion; changing this repository does not replace or restart that deployment.

The Rust control plane deliberately does **not** commit us to a tiny-object or metadata database yet. Checkpoint generation state is behind `MetadataStore`, and hot micro-object storage is behind `MicroStore`. File/directory-backed implementations preserve the current prototype layout while the database benchmark determines the permanent backend.

## Architecture

- **Filesystem surface:** low-level FUSE namespace and policy control, with kernel passthrough for materialized files where possible.
- **Control plane:** Rust 1.99 scheduler, checkpoint/convergence engine, namespace recovery, admission worker, and range-cache GC.
- **Fast tiers:** RAM/tmpfs/zram for hot or regenerable state, then NVMe for durable writeback and cache state.
- **Cold tiers:** local durable storage today; immutable content-addressed objects and optional remote S3-compatible storage are the intended backing model.
- **Policy:** longest-prefix `durable` / `volatile` rules today, evolving toward generic per-tier placement and acknowledgement requirements rather than hard-coded media names.
- **Durability:** foreground acknowledgement and background convergence are separate concerns. Volatile trees may deliberately lose uncheckpointed state.
- **Backend seams:** metadata and micro-object persistence are traits so the database benchmark can select an implementation without changing checkpoint or admission semantics.

## Build and checks

Requires Rust 1.99, libfuse >= 3.17.2 (passthrough support), `pkg-config`, and GCC. The checked-in `rust-toolchain.toml` selects the Rust toolchain and components. CI also builds against the current libfuse release rather than Ubuntu's older packaged copy.

```sh
make check
```

`make check` runs rustfmt, strict Clippy gates, the Rust recovery/admission/GC test suite, shell syntax checks, Git whitespace checks, the C warnings-as-errors build, and release builds of the Rust daemons.

## Layout

- `src/morainefs.c` and `src/passthrough_helpers.h` — low-level FUSE data/namespace path.
- `src/checkpoint.rs` — asynchronous convergence, batching, durability barriers, and crash recovery.
- `src/admission.rs` — hot micro-object admission and eviction policy.
- `src/store.rs` — checkpoint metadata backend abstraction and current file-backed adapter.
- `src/journal.rs` — namespace intent abstraction and current file-backed adapter.
- `src/range_gc.rs` — stale per-process range-cache collection.
- `src/bin/` — `moraine-checkpoint`, `moraine-admit`, and `moraine-range-cache-gc` entry points.
- `tools/setup-zram.sh` — current zram helper.
- `config/policy.example.conf` — current prefix-policy syntax.

Detailed design history, benchmark evidence, migration notes, and operational state live in Notion rather than in the repository.

## License

The FUSE core is derived from libfuse example code. See `LICENSE` and the source-file notices for applicable terms.
