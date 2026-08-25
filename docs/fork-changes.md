# What this fork changes

A record of everything this fork adds on top of upstream reth, so that the
delta is auditable without diffing the whole tree.

Base commit: `1c2942abc6d3b78a7656acdaa985bdac03408a26`
(`perf(engine): txpool prewarming (#26378)`, 2026-07-23, reth 2.4.1).

---

## Changes to existing files

Exactly one file:

**`Cargo.toml`** — three edits. Registering the new binary crate, excluding the
adapter crates from the workspace (each is its own workspace root), and
overriding revmc with a branch that fixes a JIT worker stack size:

```diff
 [workspace]
 members = [
+    "bin/reth-dtvm/",
     "bin/reth-bb/",
 ...
-exclude = ["docs/cli"]
+exclude = ["docs/cli", "adapter-subject-backends", "patched"]

+[patch."https://github.com/paradigmxyz/revmc"]
+revmc = { git = "https://github.com/abmcar/revmc", branch = "jit-worker-stack-size" }
```

No reth source file is modified. The execution stack, staged sync, database
layer and validation logic are upstream as-is.

---

## Added

| Path | What it is |
|---|---|
| `bin/reth-dtvm/` | Node binary whose EVM comes from an EVMC shared library instead of the built-in engine. Mirrors the stock `reth` binary otherwise. |
| `adapter-subject-backends/` | The EVMC bridge — see [`evmc-bridge.md`](./evmc-bridge.md). Includes the witness replay harness (`witness-db/`) — see [`witness-replay.md`](./witness-replay.md). |
| `patched/revm-handler` | Path dependency of the adapter core. |
| `patches/revmc-jit-worker-stack-size.patch` | The diff behind the `[patch]` override above, for reference or for applying to your own revmc checkout. |
| `patches/dtvm-evmc-phase-metrics.patch` | Optional DTVM patch (against `DTVMStack/DTVM@338d123`) adding the diagnostic phase-metrics ABI the batch harness's qualification mode reads. Not needed for timing runs. See [`witness-replay.md`](./witness-replay.md) §4–§5. |
| `docs/corpus/mainnet-25625000-25625099.tsv` | Manifest of a 100-block mainnet witness corpus: block number, block hash, bundle SHA-256. |
| `docs/mainnet-replay.md` | How to replay mainnet blocks with reth. |
| `docs/evmc-bridge.md` | The bridge layer. |
| `docs/witness-replay.md` | The witness replay harness: binary selection, timing/diagnostic protocols, subject-library builds, corpus. |
| `docs/fork-changes.md` | This file. |

`bin/reth-dtvm/` is 2 files (~300 lines). It defines a node type whose
components builder substitutes an EVMC-backed executor for the Ethereum one, and
drives it via `Cli::run_with_components`. This matters because reth's CLI derives
the EVM from the *node type*, so passing a factory to the launch closure alone is
not sufficient — subcommands such as `stage run` and `re-execute` resolve their
EVM through the node type and would otherwise silently use the default engine.

### Adapter revision

The bridge validates `keccak256(code) == code_hash` before handing code across
the EVMC boundary. The adapter here carries that validation in its memoized
form: code travels as refcounted `Bytes`, and validation results are cached per
thread keyed by account address (invalidated on any change of hash, pointer, or
length). An earlier adapter revision recomputed the hash over a full copy of the
bytecode on every code-related callback; numbers taken through that revision
include that extra per-callback cost. See `evmc-bridge.md` §6.

---

## The revmc patch

revmc's compilation worker pools do not set a thread stack size, so workers get
the platform default (~2 MiB). LLVM code generation on some mainnet contracts
recurses deeply enough to exceed that, which aborts the process rather than
failing the compilation:

```
thread 'revmc-13' has overflowed its stack
fatal runtime error: stack overflow, aborting
```

The fix sets a 16 MiB stack on both worker pools (in-process and
out-of-process) — 9 lines across 2 files, no other behaviour change. 16 MiB
matches sizes used by other LLVM-backed compiler worker pools.

It lives on
[`abmcar/revmc@jit-worker-stack-size`](https://github.com/abmcar/revmc/tree/jit-worker-stack-size),
branched from `cf68a87f` (the commit reth pins), and the root `Cargo.toml`
`[patch]` override points there — so a build from this repository picks it up
with no extra steps.

If you would rather carry it yourself, `patches/revmc-jit-worker-stack-size.patch`
is the same diff:

```bash
git -C /path/to/revmc apply /path/to/patches/revmc-jit-worker-stack-size.patch
```

and then repoint the override at your checkout. Drop the override entirely once
the fix is upstream.

---

## Building

```bash
# stock reth, unchanged
cargo build --release --bin reth

# EVMC-backed node
cargo build --release --bin reth-dtvm
```

`reth-dtvm` requires `RETH_SUBJECT_BACKEND`, `RETH_SUBJECT_LIBRARY` and
`RETH_SUBJECT_LIBRARY_SHA256` at runtime; it refuses to start otherwise, and
refuses if the library's hash does not match. See
[`mainnet-replay.md`](./mainnet-replay.md#3-choosing-the-execution-engine).
