# MoraineFS

MoraineFS is an experimental single-node tiered filesystem. It presents a normal FUSE filesystem while placing data across fast disposable tiers and slower durable/object-backed tiers according to policy.

This is an early prototype. **There is no compatibility promise for command lines, service names, private paths, or on-disk metadata.** We keep semantics that have been validated and freely replace prototype plumbing when a better design becomes clear.

## Current architecture

- **Foreground path:** Rust 1.99 using `fuser`, including Linux kernel FUSE passthrough for ordinary file I/O. Source-tree traversal is capability-contained with `openat2`/`RESOLVE_BENEATH` rather than ambient pathname access.
- **Control plane:** Rust for checkpoint/convergence scheduling, crash and rename recovery, hot-object admission, and range-cache garbage collection.
- **Fast data:** RAM/zram for tiny hot objects and NVMe overlays/writeback.
- **Durable data:** a local durable tier today, with content-addressed and S3-compatible backing intended later.
- **Policy:** longest-prefix durable/volatile behavior today; the policy model is independent of storage backends.

The database benchmark is intentionally still the decision point for tiny-object and metadata persistence. Filesystem semantics depend on narrow Rust interfaces (`MetadataStore`, `NamespaceJournal`, and `MicroStore`) rather than on a particular database. The current directory/file-backed implementations are disposable prototype adapters, not a stable storage format.

The useful part of the old prototype is its behavior: asynchronous convergence, generation validation, batching/settling, crash recovery, rename transaction semantics, admission/eviction policy, and kernel passthrough. Those semantics are now implemented in Rust. No legacy runtime identity or compatibility layer is carried forward.

## Build and checks

Requires Rust 1.99. Building MoraineFS does **not** require libfuse development headers or a C compiler.

The `morainefs` foreground process runs as root on Linux because kernel FUSE passthrough backing registration is privileged. It mounts with `allow_other` and preserves the requesting FUSE uid/gid (including setgid-directory group inheritance) when creating filesystem objects.

```sh
make check
```

`make check` runs rustfmt, strict Clippy, Rust tests, shell/Git checks, and release builds of every binary.

## Binaries

- `morainefs` — foreground FUSE filesystem.
- `moraine-checkpoint` — asynchronous convergence/checkpoint worker.
- `moraine-admit` — hot/tiny-object admission worker.
- `moraine-range-cache-gc` — stale per-process range-cache cleanup.

The current process split is a prototype implementation detail; the backend traits are the architectural boundary that matters while the metadata/tiny-item benchmark is unresolved.

## Layout

- `src/foreground.rs` — FUSE namespace/data path, passthrough handle management, writeback selection, and foreground transaction semantics.
- `src/foreground/source.rs` — capability-rooted source filesystem access using `openat2`/`RESOLVE_BENEATH`.
- `src/foreground/inode.rs` — synthetic inode identity plus lookup/open/directory lifetime accounting.
- `src/checkpoint.rs` — asynchronous convergence, scheduling, durability barriers, and recovery semantics.
- `src/admission.rs` — tiny/hot-object admission and eviction semantics plus the `MicroStore` interface.
- `src/store.rs` — `MetadataStore` plus the disposable file-backed adapter.
- `src/journal.rs` — `NamespaceJournal` plus the disposable file-backed adapter.
- `src/policy.rs` — tier policy parsing and longest-prefix selection.
- `src/range_gc.rs` — stale per-process range-cache cleanup.
- `src/bin/` — process entry points around the Rust library.
- `tools/setup-zram.sh` — current zram development helper.
- `config/policy.example.conf` — current policy syntax.

Detailed exploration and benchmark history belong in Notion, not in the source tree.

## License

GPL-2.0-only; see `LICENSE`.
