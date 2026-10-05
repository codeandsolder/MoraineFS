# MoraineFS

MoraineFS is an experimental single-node tiered filesystem. It presents a normal FUSE filesystem while placing data across fast disposable tiers and slower durable/object-backed tiers according to policy.

This is an early prototype. **There is no compatibility promise for command lines, service names, private paths, or on-disk metadata.** We keep semantics that have been validated and freely replace prototype plumbing when a better design becomes clear.

## Current architecture

- **Foreground path:** a small low-level libfuse C core, kept close to libfuse so passthrough and kernel-facing behavior remain easy to audit.
- **Control plane:** Rust 1.99 for checkpoint/convergence scheduling, crash and rename recovery, hot-object admission, and range-cache garbage collection.
- **Fast data:** RAM/zram for tiny hot objects and NVMe overlays/writeback.
- **Durable data:** a local durable tier today, with content-addressed and S3-compatible backing intended later.
- **Policy:** prefix-based durable/volatile behavior today; the policy model can evolve independently of storage backends.

The database benchmark is intentionally still the decision point for tiny-object and metadata persistence. The Rust control plane therefore depends on narrow interfaces (`MetadataStore`, `NamespaceJournal`, and `MicroStore`) rather than on a particular database. The current directory/file-backed implementations are disposable prototype adapters, not a stable storage format.

The foreground C path still reads/writes the prototype file-backed overlay/generation/journal layout directly. That is the remaining backend-coupled seam, intentionally left provisional until the benchmark chooses the metadata/tiny-object backend; it is not an interface to preserve.

## What is considered stable enough to keep

The useful part of the old prototype is its behavior: asynchronous convergence, generation validation, batching/settling, crash recovery, rename transaction semantics, admission/eviction policy, and range-cache cleanup. Those semantics live in Rust and are covered by deterministic tests.

No legacy runtime identity or compatibility layer is carried forward; the repository describes the current prototype only.

## Build and checks

Requires Rust 1.99, libfuse >= 3.17.2, `pkg-config`, and GCC.

```sh
make check
```

`make check` runs rustfmt, strict Clippy, Rust tests, shell/Git checks, the C warnings-as-errors build, and release builds of the Rust binaries.

## Layout

- `src/morainefs.c`, `src/passthrough_helpers.h` — low-level FUSE foreground/data path.
- `src/checkpoint.rs` — asynchronous convergence, scheduling, durability barriers, and recovery semantics.
- `src/admission.rs` — tiny/hot-object admission and eviction semantics.
- `src/store.rs` — metadata interface plus disposable file-backed adapter.
- `src/journal.rs` — namespace transaction interface plus disposable file-backed adapter.
- `src/range_gc.rs` — stale per-process range-cache cleanup.
- `src/bin/` — small process entry points around those libraries.
- `tools/setup-zram.sh` — current zram development helper.
- `config/policy.example.conf` — current policy syntax.

Detailed exploration and benchmark history belong in Notion, not in the source tree.

## License

The FUSE core is derived from libfuse example code. See `LICENSE` and the source-file notices for applicable terms.
