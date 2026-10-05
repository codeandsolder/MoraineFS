# MoraineFS

MoraineFS is an experimental single-node tiered storage layer that presents an ordinary FUSE filesystem while moving data across fast disposable tiers and slower durable/object-backed tiers according to policy.

The current implementation is the consolidated successor to the `io-tierfs` prototype. It has a low-level libfuse C data/namespace path, a Python asynchronous checkpointer, RAM/zram admission helpers, NVMe writeback state, longest-prefix durability policy, and crash-recovery tests. The longer-term storage model is content-addressed and is intended to expose a pragmatic S3-compatible object surface alongside the filesystem mount.

## Status

This repository is the canonical development source. It is still experimental. The existing cold-storage deployment intentionally continues to use its `io-tierfs` runtime paths and services until a separately validated promotion; importing the code here does not replace or restart that deployment.

## Architecture

- **Filesystem surface:** low-level FUSE namespace and policy control, with kernel passthrough for materialized files where possible.
- **Fast tiers:** RAM/tmpfs/zram for hot or regenerable state, then NVMe for durable writeback and cache state.
- **Cold tiers:** local durable storage today; immutable content-addressed objects and optional remote S3-compatible storage are the intended backing model.
- **Policy:** longest-prefix `durable` / `volatile` rules today, evolving toward generic per-tier placement and acknowledgement requirements rather than hard-coded media names.
- **Durability:** foreground acknowledgement and background convergence are separate concerns. Volatile trees may deliberately lose uncheckpointed state.

## Build and checks

Requires libfuse3 development headers, `pkg-config`, GCC, and `uv`.

```sh
uv sync --locked
make check
```

`make check` runs Ruff, Python recovery/scheduler tests, shell syntax checks, Git whitespace checks, and a strict C build with warnings (including conversion warnings) promoted to errors.

## Layout

- `src/` — FUSE core.
- `tools/checkpoint.py` — asynchronous convergence/checkpoint engine.
- `tools/admit.py` — hot-object admission helper.
- `tools/setup-zram.sh` — current zram helper.
- `tests/` — deterministic checkpoint/recovery and settle-window tests.
- `config/policy.example.conf` — current prefix-policy syntax.

Detailed design history, benchmark evidence, migration notes, and operational state live in Notion rather than in the repository.

## License

The FUSE core is derived from libfuse example code. See `LICENSE` and the source-file notices for applicable terms.
