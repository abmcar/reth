# revmc witness adapter

A standalone binary that replays mainnet witness bundles through reth's block
executor with the experimental revmc JIT backend, plus a native-REVM reference
leg for differential correctness and reference timing.

This is one of **two distinct revmc paths** related to this repository, and they
do not measure the same thing. See `docs/witness-replay.md` for the full
comparison; in short:

- The in-tree `--jit` node flag (documented in `docs/mainnet-replay.md`) runs
  against a datadir: execution timing includes database reads, and JIT
  compilation overlaps the measured window.
- This adapter takes witness bundles as input, executes against an in-memory
  strict witness DB (no disk reads inside the measured window), and in its
  `resident` lane starts the timer only after every unique witness program has
  been JIT-compiled and the runtime queues are empty (compile-excluded, pure
  execution).

## Dependency baseline (why this crate does not use the in-tree reth crates)

This crate pins, by git revision, the exact dependency set it was written and
measured against:

- reth: `paradigmxyz/reth@70fb52e5fc7e6fb799937005ac294c8fffba5a61`
  (upstream main, v2.5.0-dev, revm `=42.0.1`, alloy-evm `=0.38.0`)
- revmc: `paradigmxyz/revmc@cf68a87f627299a9c49bcc333a8a317c9b312a3d`

The rest of this repository is based on reth v2.4.1 (revm 41, alloy-evm 0.37),
which is type-incompatible with the above. The two baselines execute the same
consensus rules over the same witness input, and every block is gated by
pre/post state-root verification, but the reth scaffolding inside the measured
window is not the same code between this adapter and the repository's other
backends. Treat cross-backend ratios accordingly.

`src/main.rs` hardcodes the same two commits as `RETH_COMMIT` / `REVMC_COMMIT`
and stamps them into every report, so a report is self-describing about what
produced it.

## Provenance

The shipped sources are byte-identical to the checkout that built the
measured binary:

| file | sha256 |
|---|---|
| `src/main.rs` | `b115375658045a12faf90dc85185ff30a9deeb38c26a13fe4ad8bd26d2b461e9` |
| `src/witness.rs` | `59eb1a2810d9dd11247207f136d3813c7946567ed212cd529b931d19558e81d5` |
| `src/strict_db.rs` | `660b8149e5f1acdd300a9038f490093480ab43eabd4c281ec703c9f8ba230c34` |

Only the dependency *source* fields of `Cargo.toml` differ from that checkout
(path dependencies became the git pins above); the committed `Cargo.lock`
resolves to the same 456 packages with identical versions and checksums.

The build emits a few `dead_code` warnings. They are expected: the sources are
kept byte-for-byte as measured, so silencing them would break the hashes above.
`cargo build --release --locked` and `cargo test --release --locked` (41 tests)
pass as committed.

## Lanes

- `--lane resident` — per-block pure-execution timing. All unique witness
  programs (including dynamic CREATE initcode discovered by one unmeasured
  discovery replay) are compiled and resident before the timer starts; the
  measured executor is created fresh after the resident gate so no
  pre-resident cache miss can leak into the window.
- `--lane full-lifecycle` — one wall-clock span across compile + execute, no
  per-block timing.
- `--lane correctness-smoke` — no timer; resident gate, then fresh-executor
  differential correctness against the native-REVM reference (single bundle).

Every lane runs the native-REVM reference leg first; its per-block `Instant`
span is plain interpreter execution with no compile phase and is reported
alongside the subject numbers.

## Build

Requires rustc 1.95 and an LLVM release for revmc's static linking; the
measured binary was built with rustc 1.95.0 against the official LLVM 22.1.8
Linux x64 release archive. Network access is needed on first build to fetch
the pinned git dependencies.

Point the build at that LLVM explicitly; `llvm-sys` will not find it otherwise and
the first build fails in its build script:

```bash
export LLVM_SYS_221_PREFIX=/path/to/LLVM-22.1.8-Linux-X64
cargo build --release
```


```sh
cargo build --release
./target/release/revmc-witness-adapter --lane resident \
  --bundle witness-25625000.json --bundle witness-25625001.json ...
```

Bundles must be passed in ascending canonical block-number order. The report
is a single JSON document on stdout. In the `resident` lane the per-block
subject timing is `blocks[].measuredElapsedNs` and the native-REVM reference
timing is `blocks[].revmReferenceElapsedNs`; `measurementBoundary` states in
prose where the timer starts and stops, and the per-block boolean gates
(`preStateRootVerified`, `subjectPostStateRootVerified`, …) must all be true
for a block's timing to be usable.
