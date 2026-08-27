# Mainnet replay with reth

This document describes how to re-execute historical Ethereum mainnet blocks with
reth, reading state from a real on-disk database. It covers the stock reth paths
first, then the additions in this fork.

It does not prescribe a benchmarking methodology. It documents which mechanisms
exist, what each one measures, and what commonly goes wrong.

---

## 1. Prerequisites

A reth datadir containing the block range you want to replay. Either sync a node
normally, or download a snapshot — reth ships a downloader:

```bash
reth download --chain mainnet --datadir <DATADIR> \
    --manifest-path <manifest.json> --minimal --resumable
```

Snapshot manifests are published at <https://snapshots.reth.rs/>. The `--minimal`
profile keeps full state and headers but prunes history to a trailing window; the
exact retention distances are printed in the download log and stored as prune
checkpoints in the datadir.

Check which range is actually usable before planning a run:

```bash
reth db --datadir <DATADIR> stats
```

Two stage checkpoints matter:

- **`Bodies`** — highest block whose transactions are present locally. You cannot
  replay past this without fetching more bodies.
- **`Execution`** — the height the plain state currently reflects.

---

## 2. Re-executing a block range

Two commands re-execute a range. They differ in which engines they can drive,
so pick based on that first:

| | `stage run execution` | `re-execute` |
|---|---|---|
| Writes to the datadir | yes (`--commit` mandatory) | no |
| Unwinds first | yes, unless `--skip-unwind` | no |
| Accepts `--jit` (revmc) | **no** | **yes** |
| Honours `RETH_SUBJECT_BACKEND` (EVMC) | yes | yes |

`JitArgs` is wired into the `node` and `re-execute` commands only, so **revmc
cannot be enabled on `stage run`**. To replay with revmc, use `re-execute`.

### `reth stage run execution`

Runs the execution stage over an explicit range:

```bash
reth stage run execution \
    --datadir <DATADIR> --chain mainnet \
    --from <FIRST> --to <LAST> --commit
```

Per block this validates gas used, receipts root and logs bloom against the
canonical header, and validates the resulting state against the stored
changesets. It prints `Finished stage stage=Execution time=<seconds>` when done.

Three properties are easy to get wrong:

1. **`--commit` is mandatory for the execution stage.** Its `requires_commit()`
   matches `Headers | Bodies | Execution`, and the command errors out without the
   flag. It writes to the database — run it on a copy of the datadir, never on
   one you want to keep.

2. **It unwinds before it executes.** Absent `--skip-unwind`, the stage is first
   unwound from its checkpoint down to `--from`, then executed forward. For the
   execution stage this unwind is changeset-based and does not touch the trie,
   but its cost grows with unwind distance. A range ending at or near the current
   `Execution` checkpoint unwinds least.

3. **Executing block *N* requires the state as of *N−1*.** That is what the
   unwind establishes. Picking a range far below the current execution height
   means a correspondingly long unwind.

### `reth re-execute`

Re-executes a range and validates against changesets without the staged-sync
unwind. It clamps `--to` to the node's best block — the `Finish` checkpoint —
which can be lower than the `Execution` checkpoint on a datadir whose later
stages have not completed.

### Copying a datadir cheaply

On a filesystem with reflink support (XFS with `reflink=1`, Btrfs), a copy is
near-instant and consumes no space until written:

```bash
cp --archive --reflink=always --sparse=auto <DATADIR> <COPY>
```

Check support with `xfs_info <mountpoint> | grep reflink`. Without reflink this
is a full copy of the entire database.

---

## 3. Choosing the execution engine

### Stock reth: REVM, or the revmc JIT

reth's default engine is REVM. Upstream reth also integrates the
[revmc](https://github.com/paradigmxyz/revmc) JIT compiler, added in
[#23230](https://github.com/paradigmxyz/reth/pull/23230) and made opt-in for
library crates in [#25178](https://github.com/paradigmxyz/reth/pull/25178).

The `jit` cargo feature is in the `reth` binary's default feature set, so release
binaries generally include it. Enable it at runtime — on `node`, or on
`re-execute` for replay:

```bash
# replay a range with revmc enabled
reth re-execute --datadir <COPY> --chain mainnet \
    --from <FIRST> --to <LAST> \
    --jit --jit.hot-threshold 8 --jit.worker-count 16
```

`stage run` does not accept these flags (see the table in §2).

| Flag | Meaning | Default |
|---|---|---|
| `--jit` | Enable JIT compilation of EVM bytecode | off |
| `--jit.hot-threshold` | Observed misses before a bytecode is promoted to JIT | 8 |
| `--jit.worker-count` | JIT compilation worker threads | — |
| `--jit.channel-capacity` | Lookup-event channel capacity; events drop silently when full | 4096 |
| `--jit.max-pending-jobs` | Maximum queued compilation jobs | 2048 |

This is threshold-triggered compilation, not ahead-of-time compilation of
everything: a contract is compiled only after being observed
`--jit.hot-threshold` times, and code executed fewer times than that runs
interpreted. If you need every contract compiled before some measured region
begins, you have to arrange and confirm that yourself.

#### JIT worker stack size

Upstream revmc builds its compilation worker pools without setting a thread
stack size, so workers get the platform default (~2 MiB). LLVM code generation
recurses deeply enough on some mainnet contracts to exceed that, which aborts
the process:

```
thread 'revmc-13' has overflowed its stack
fatal runtime error: stack overflow, aborting
```

This fork's root `Cargo.toml` carries a `[patch]` override pointing at a revmc
branch that sets a 16 MiB stack and changes nothing else, so a build from this
repository already includes the fix:

```toml
[patch."https://github.com/paradigmxyz/revmc"]
revmc = { git = "https://github.com/abmcar/revmc", branch = "jit-worker-stack-size" }
```

The diff is also in
[`patches/revmc-jit-worker-stack-size.patch`](../patches/revmc-jit-worker-stack-size.patch)
if you would rather apply it to your own revmc checkout and point the override
there. Remove the override entirely once the fix lands upstream.

`--jit` measures node-shaped execution: state comes from the database, and
JIT compilation overlaps the measured window. A second, standalone revmc path
— witness input, in-memory state, compilation excluded from the timer — lives
in `adapter-subject-backends/revmc-witness/` and is documented in
`witness-replay.md` §8; the two are not interchangeable and their numbers are
not comparable.

### This fork: EVMC backends

`bin/reth-dtvm/` builds `reth-dtvm`, a node binary identical to the stock one
except that its EVM comes from an EVMC-compatible shared library, selected from
the environment:

```bash
RETH_SUBJECT_BACKEND=dtvm-eager \
RETH_SUBJECT_LIBRARY=/path/to/libdtvmapi.so \
RETH_SUBJECT_LIBRARY_SHA256=<sha256 of that file> \
DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true \
  reth-dtvm stage run execution --datadir <COPY> --chain mainnet \
      --from <FIRST> --to <LAST> --commit
```

| Variable | Values |
|---|---|
| `RETH_SUBJECT_BACKEND` | `dtvm-eager`, `dtvm-profile-guided`, `evmone-advanced` |
| `RETH_SUBJECT_LIBRARY` | path to the EVMC shared library |
| `RETH_SUBJECT_LIBRARY_SHA256` | expected SHA-256; the binary refuses to start on mismatch |

All three are required. The hash check guards against silently running a
different build than intended.

On startup the binary logs which backend it installed:

```
INFO Installing EVMC subject backend as the node execution engine
     backend=DtvmEager library=... sha256=...
```

Absence of that line means the injected factory was not used and execution fell
through to the default engine.

Everything except the EVM is stock reth — same staged sync, same database, same
validation. The crate's default feature set is however **smaller** than the
stock `reth` binary's: besides `jit` (so the revmc JIT is not simultaneously
active), it also omits `gmp`, `reth-revm/portable`, `js-tracer` and the OTLP
features. `gmp` (modexp precompile) and `reth-revm/portable` are
performance-relevant, so when comparing `reth-dtvm` against a stock `reth`
binary on the datadir path, either enable the same features on both builds or
account for the difference.

Build:

```bash
cargo build --release --bin reth-dtvm
```

The EVMC adapter it depends on is documented separately in
[`docs/evmc-bridge.md`](./evmc-bridge.md).

---

## 4. Execution witnesses

`debug_executionWitnessByBlockHash` returns the state a block touches, enabling
stateless re-execution elsewhere:

```bash
curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"debug_executionWitnessByBlockHash","params":["<BLOCK_HASH>"]}' \
  http://127.0.0.1:8545
```

Two operational notes:

- **Long calls can hit the database read-transaction timeout** (default 300 s),
  surfacing as `read transaction has been timed out (-96000)`. Adjust with
  `--db.read-transaction-timeout <SECONDS>` (`0` disables the limit).
- **Cost depends strongly on the block's depth below the head.** At the current
  head the trie state is already in place; for older blocks the node reconstructs
  historical state, and it warns `Attempt to calculate state root for an old
  block might result in OOM`. Measure the depth you actually need rather than
  assuming it is uniform.

---

## 5. Factors that affect measurements

These dominate often enough to be worth stating explicitly, whatever you are
measuring:

- **Page cache.** The same range on the same binary can differ several-fold
  between a cold and warm page cache. Control it — drop caches
  (`echo 3 | sudo tee /proc/sys/vm/drop_caches`) for cold, or take an untimed
  warm-up pass for warm — and record which regime a number came from.

- **Storage.** Execution from a real database is dominated by random reads.
  `iostat -x <dev>` showing `%util` near 100 with queue depth near 1 indicates
  the workload is waiting on the device rather than computing.

- **Timing boundary.** reth exposes several nested regions: the frame execution
  loop, one whole block through the block executor, and the full stage wall clock
  including database I/O and the unwind. They are not interchangeable, and a
  comparison is only meaningful if every arm is measured at the same one.

- **Compilation.** JIT and AOT engines pay a compilation cost that is not
  execution. Whether it falls inside a timed region depends on where the region
  starts and whether compilation has completed by then.

- **Log lines are not a timing source.** `Executed block range` lines are emitted
  on the stage's own schedule and do not correspond one-to-one with work done.
  `Finished stage … time=` is the authoritative figure for a run.

---

## 6. Verifying a run

Treat a run as valid only if it completed without error. The execution stage
validates every block against the canonical header and the stored changesets, so
a clean `Finished stage` with no error output means those checks passed across
the range.

When comparing engines, run the reference engine over the identical range first.
If that does not pass cleanly, the datadir or the range — not the engine under
test — is what needs fixing.
