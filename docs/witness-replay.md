# Witness replay harness

`adapter-subject-backends/witness-db/` contains a standalone harness that
re-executes single mainnet blocks from **witness bundles** — self-contained JSON
files carrying the block and every piece of state it touches. State is preloaded
into memory, so unlike the datadir-based paths in
[`mainnet-replay.md`](./mainnet-replay.md), no database I/O occurs inside the
measured region. The two families of numbers are **not comparable** with each
other; see §5 of that document.

Every replay validates gas, receipts root, logs bloom and the post-state root,
and (in differential mode) cross-checks the subject engine against reth's
built-in REVM executor on the same bundle.

---

## 1. Pick the right binary — this determines what you measure

| Binary | VM lifetime | Use for |
|---|---|---|
| `replay-block` | **one VM per invocation** | correctness of a single bundle |
| `replay-batch` | one VM across all bundles and passes | DTVM timing and cache diagnostics |
| `replay-evmone-batch` | one VM across all bundles and passes | evmone diagnostics (see §6) |
| `verify-witness` | — | witness integrity only, no execution |

**Do not time engines by looping `replay-block` over many bundles.** Each
invocation constructs a fresh EVMC VM with an empty code cache. For a
compiling engine (`dtvm-eager` compiles every contract synchronously on first
call) this re-pays full compilation for every block, and the resulting numbers
measure repeated cold compilation, not execution. `replay-batch` exists
precisely to hold one VM — and therefore one compiled-code cache — across the
whole run, and it fails closed (`subjectVmCreateCount must equal 1`) if that
invariant breaks.

Build them from the repository root (also needs `curl` and `jq` for the
capture and extraction steps later):

```bash
cargo build --release --manifest-path adapter-subject-backends/witness-db/Cargo.toml \
    --bin replay-batch --bin replay-block --bin verify-witness
```

`witness-db` is its own workspace root, so the binaries land in
`adapter-subject-backends/witness-db/target/release/`, **not** in the
repository-root `target/`. The command examples below assume that directory is
on `PATH` or spelled out.

---

## 2. Environment variables

The harness binaries read the `RETH_SUBJECT_*` family (the same one
`reth-dtvm` uses):

| Variable | Meaning |
|---|---|
| `RETH_SUBJECT_BACKEND` | `dtvm-eager`, `dtvm-profile-guided`, `evmone-advanced` |
| `RETH_SUBJECT_LIBRARY` | path to the EVMC shared library |
| `RETH_SUBJECT_LIBRARY_SHA256` | expected SHA-256 of that file; startup fails on mismatch |
| `DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION` | must be `true` for the DTVM backends |

The `DTVM_LIBRARY` / `DTVM_LIBRARY_SHA256` names that appear in
`witness-db/README.md` and in test code are consumed **only by `cargo test`**;
the replay binaries ignore them.

---

## 3. Timing protocol (`replay-batch --production-timing`)

```bash
RETH_SUBJECT_BACKEND=dtvm-eager \
RETH_SUBJECT_LIBRARY=/path/to/libdtvmapi.so \
RETH_SUBJECT_LIBRARY_SHA256=<sha256> \
DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true \
  replay-batch --production-timing bundles/*.json > timing.jsonl
```

One long-lived process runs a fixed 12-pass lifecycle over the whole bundle
set:

| Pass | Role | `timingUse` |
|---|---|---|
| `C0` | cold population — every contract is compiled here | `false` |
| `G0` | hot-cache gate | `false` |
| `W0`, `W1` | warm-up | `false` |
| `M0`–`M7` | measured | `true` |

Requirements checked at startup and per record (violations abort the batch):
the backend is `dtvm-eager`; the library's diagnostic metrics ABI is *absent*
(`--production-timing` requires a metrics-OFF build — see §5); exactly one EVMC
VM exists for the whole batch; every block passes the correctness gate.

**Extracting numbers.** Each output line is one block × one pass. Filter to
`pass.measured == true` and `correctness.passed == true`, then read
`replay.phaseWallTimeNs.rethSubjectExecute` — one whole block through
`BasicBlockExecutor::execute_one`, wall clock (the region also includes
executor construction and state extraction, identically on both arms). Aggregate the eight measured
passes per block (e.g. median). The REVM reference figure for the *same block in
the same record* is `replay.phaseWallTimeNs.rethRevmExecute`, measured at the
same boundary with the same witness database; that pairing is the one
comparison this harness makes apples-to-apples by construction.

Do **not** use `rethSubjectRunExecLoop.wallNs` for cross-engine comparison: it
covers only the frame-execution loop, a strictly smaller region than
`rethSubjectExecute`.

The whole extraction as one `jq` invocation — per block: number, subject
median (ms), reference median (ms) over the eight measured passes:

```bash
jq -rs '
  def med: sort | if length % 2 == 1 then .[length/2|floor]
                  else (.[length/2-1] + .[length/2]) / 2 end;
  map(select(.timingUse and .correctness.passed) | .replay)
  | group_by(.blockNumber)[]
  | [ .[0].blockNumber,
      ([.[].phaseWallTimeNs.rethSubjectExecute] | med / 1e6),
      ([.[].phaseWallTimeNs.rethRevmExecute]    | med / 1e6) ]
  | @tsv' timing.jsonl
```

---

## 4. Diagnostic protocol (`replay-batch`, default mode)

The default (no flag) mode runs the same 12-pass lifecycle against a DTVM build
whose **diagnostic metrics ABI is present** (§5), and snapshots 24 internal
counters around every replay: synchronous JIT compile attempts and wall time,
module-cache lookups/hits/misses/evictions, JIT vs interpreter frame counts,
and fallback classifications.

On measured passes the hot-cache gate requires, among others:

```
synchronousJitCompileAttemptCount == 0     moduleCacheMissCount == 0
synchronousJitCompileWallNs       == 0     moduleCacheEvictionCount == 0
```

i.e. it *proves* that no compilation and no cache churn happened inside any
measured region, rather than assuming it. Diagnostic records carry
`timingUse: false` — the metrics probes themselves cost time, so a strict
comparison uses **two runs**: this one for qualification, a
`--production-timing` run with a metrics-OFF library for the numbers. Production
records carry `requiresSameCellDiagnosticQualification: true` to point at that
pairing. Skipping the diagnostic run leaves the timing run valid but
unqualified: you keep the single-VM guarantee, you lose the zero-compilation
proof.

---

## 5. Building the subject libraries

### DTVM

Pinned source: [`DTVMStack/DTVM`](https://github.com/DTVMStack/DTVM) at
`338d123` (`refactor(evm): enforce prepared-memory helper proof contracts
(#598)`). Build prerequisites (LLVM for the multipass JIT, cmake entry points)
are DTVM's own — follow its README at that commit. Configuration snapshot of
the build used with this harness (GCC 12, `Release`):

```
ZEN_ENABLE_EVM=ON                ZEN_ENABLE_MULTIPASS_JIT=ON
ZEN_ENABLE_LIBEVM=ON             ZEN_ENABLE_VIRTUAL_STACK=ON
ZEN_ENABLE_CPU_EXCEPTION=ON      ZEN_ENABLE_JIT_PRECOMPILE_FALLBACK=ON
ZEN_ENABLE_BUILTIN_LIBC=ON       ZEN_ENABLE_BUILTIN_WASI=ON
```

An unpatched `338d123` tree has no diagnostic metrics ABI at all, which is
exactly what `--production-timing` requires. (The
`ZEN_ENABLE_EVMC_PHASE_METRICS` option itself only exists after applying the
diagnostic patch below.)

The artefact is `lib/libdtvmapi.so`; hash it with `sha256sum` for
`RETH_SUBJECT_LIBRARY_SHA256`.

For the **diagnostic** build, apply
[`patches/dtvm-evmc-phase-metrics.patch`](../patches/dtvm-evmc-phase-metrics.patch)
on top of `338d123` — or check out the same commit pre-applied as
[`abmcar/DTVM@evmc-phase-metrics`](https://github.com/abmcar/DTVM/tree/evmc-phase-metrics)
— and configure with `-DZEN_ENABLE_EVMC_PHASE_METRICS=ON`.
The patch adds the exported counter ABI
(`dtvm_get_evmc_phase_metrics` / `dtvm_reset_evmc_phase_metrics`, struct v2)
plus build switches whose defaults leave codegen behavior identical to the
unpatched tree.

### evmone

Pinned source: [`DTVMStack/evmone`](https://github.com/DTVMStack/evmone) at
`a4a0e47` (`feat: update evmc (#9)`), built unmodified with its standard CMake
setup; the artefact is `libevmone.so`. Select it with
`RETH_SUBJECT_BACKEND=evmone-advanced`.

---

## 6. Backend coverage of the batch paths

| Backend | `replay-batch` | Notes |
|---|---|---|
| `dtvm-eager` | yes | both modes of §3/§4 |
| `dtvm-profile-guided` | **no** | only `replay-block`; each invocation starts a fresh VM with an empty profile window, so per-bundle runs never accumulate enough heat to trigger JIT — numbers reflect near-pure interpretation |
| `evmone-advanced` | **no** (`replay-evmone-batch` only) | `replay-evmone-batch` refuses to start unless the library exports `evmone_get_advanced_diagnostic_metrics`, an instrumentation symbol that upstream evmone does not provide and that this repository does not ship. Per-bundle `replay-block` runs remain available; evmone's per-call code analysis has no persistent cross-process cache, so fresh-process runs do not change what is measured. |

---

## 7. Bundle corpus

[`docs/corpus/mainnet-25625000-25625099.tsv`](./corpus/mainnet-25625000-25625099.tsv)
lists a 100-block mainnet corpus (block number, block hash, bundle SHA-256).
The bundle files themselves total ~1.4 GB and are not stored in git.

To capture blocks, use `fetch-witness.sh` against a node that can serve
witnesses for them — one bundle per block:

```bash
RPC=http://127.0.0.1:8545
mkdir -p bundles
for n in $(seq 25625000 25625099); do   # or any window your node can serve
  hash=$(curl -s -X POST -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["'"$(printf '0x%x' "$n")"'",false]}' \
    "$RPC" | jq -r .result.hash)
  adapter-subject-backends/witness-db/fetch-witness.sh \
    "$RPC" "$hash" "bundles/block-$n-$hash.json"
done
```

(`capture-window.sh` is the original experiment's sealed-provenance pipeline;
it refuses to run without frozen repository-identity manifests specific to
that environment and is not needed for reproduction.)

Node-side requirements: the reth endpoint must expose the `debug` RPC
namespace (`--http.api` including `debug`) — the script calls
`debug_getRawHeader`, `debug_getRawBlock` and
`debug_executionWitnessByBlockHash`, the last with the two-argument
`canonical` form. Older reth releases reject the second argument; the script
then fails loudly with `capability_missing` rather than falling back. A node
built from this repository (reth 2.4.1 base) serves it; if your node is
older, upgrade it or build `reth` from here. Witness generation cost rises steeply
with the block's depth below the node's head (see `mainnet-replay.md` §4);
for blocks well below the head, raise or disable the database
read-transaction timeout (`--db.read-transaction-timeout 0`) — deep witness
generation can exceed the 300 s default. Pointing the loop at a window just
below the current finalized block instead avoids the depth cost entirely —
engine comparisons within a corpus do not depend on which window it is.

The SHA-256 column identifies the exact bundle bytes used with this corpus; a
bundle re-captured from a different node or reth version can differ byte-wise
while describing the same block — the block hash is the anchor, and
`verify-witness` checks a bundle's internal integrity either way.
