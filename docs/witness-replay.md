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
| `RETH_SUBJECT_EVMC_OPTIONS` | optional; comma-separated `name=value` EVMC options applied after the mandatory ones, e.g. `code_cache_dir=/var/cache/dtvm,code_cache_mode=rw`. A malformed entry, or one the library rejects, fails startup with an error rather than being silently ignored. Which names are accepted depends entirely on the loaded library — see §5. |

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
`338d123a5d9d4a464d8d0151158447d500a9997a` (`refactor(evm): enforce
prepared-memory helper proof contracts (#598)`). Build prerequisites (LLVM for the multipass JIT, cmake entry points)
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

**Persistent code cache (optional).** The pinned `338d123` tree keeps compiled
code only in process memory, so every process recompiles every contract it
meets. A branch that adds an on-disk cache —
[`abmcar/DTVM@feat/evm-persistent-code-cache`](https://github.com/abmcar/DTVM/tree/feat/evm-persistent-code-cache)
— accepts two extra EVMC options, `code_cache_dir` (a directory) and
`code_cache_mode` (`off`, `ro`, `rw`), passed via
`RETH_SUBJECT_EVMC_OPTIONS` (§2). It needs no additional CMake switch — the
cache sits inside the already-enabled `ZEN_ENABLE_MULTIPASS_JIT` — and the
default mode is `off`, so leaving the options unset reproduces the pinned
tree's behaviour. Option passthrough requires harness commit `b778d43f7` or
later; against the pinned library these option names are rejected and startup
fails.

How much this matters depends entirely on how many processes see the same
contracts. Measured with `replay-block` in **one process per block** — the
worst case for in-memory reuse — over blocks 25625046–25625055, as the mean
`rethSubjectExecute` phase:

| cache | s/block |
|---|---|
| none | 492.3 |
| `rw`, starting empty | 206.9 (399.3 on the first block, 71.3 on the tenth) |
| `ro`, already populated | 3.0 |

Two caveats on the last row. First, roughly all of those 3 s is cache loading
(read, digest check, install), not execution: the same blocks execute in
~95 ms once code is resident in memory, so a populated cache does not buy
hot-execution speed — it buys not compiling. Second, the 492 s baseline is
specific to one-process-per-block. Within a single long-lived process the
in-memory cache already absorbs most of this cost: in a 17-block batch run on
the same corpus, the first block spent 340–520 s compiling and later blocks
50–150 s, without any on-disk cache.

**In-memory module cache bound (optional).** Separately from the on-disk cache,
each EVMC VM keeps compiled modules in an LRU whose capacity is a compile-time
constant (`MaxModuleCacheSize`, 4096 in the pinned tree). A `replay-batch` run
holds one VM across the whole corpus, so a corpus containing more unique
contracts than that bound evicts and recompiles inside the measured passes —
which the §4 hot-cache gate then fails, correctly. The
`feat/evm-persistent-code-cache` branch reads
`DTVM_EVM_MAX_MODULE_CACHE_SIZE` (a plain environment variable, not an EVMC
option) to raise it; an unparseable value is ignored with a warning and the
default stands.

**Do not size this bound by counting unique contracts.** That is the mistake this
document previously recommended, and it cost a full re-measurement. The cache key is
`(code_address, revision, memory-specialisation profile, 8-byte code prefix, code
size)`, so one contract occupies more than one entry whenever it is entered under
more than one memory profile. The 1000-block corpus of §10 has 15,415 unique
contracts, a bound of 16384 looks comfortable, and it evicts: the diagnostic
protocol fails the hot-cache gate at the very first block of `G0` with seven
evictions, seven misses and seven synchronous compilations. The measured effect of
that eviction on the same corpus, same disk cache, same binary and same protocol
was **217.23 ms per block against 87.15 ms** — a 2.49x error, entirely inside the
"execution" figure.

Size it with headroom and then **prove** it: a `--production-timing` run does not
check the hot-cache gate and will report contaminated numbers without complaint.
Only the default (diagnostic) protocol of §4 checks
`moduleCacheEvictionCount == 0` and `synchronousJitCompileAttemptCount == 0`. The
§10 campaign now runs at 131072 and is verified eviction-free that way.

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
`a4a0e47aff903a47a6be133c67ad106c706fe566` (`feat: update evmc (#9)`), built
unmodified with its standard CMake
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

## 7. Capturing a bundle corpus

Capture a window of consecutive blocks **near your node's finalized tip** —
witness generation there is cheap, and engine comparisons within a corpus do
not depend on which window it is. One bundle per block via `fetch-witness.sh`:

```bash
RPC=http://127.0.0.1:8545
mkdir -p bundles
TO=$(curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["finalized",false]}' \
  "$RPC" | jq -r '.result.number' | xargs printf '%d\n')
FROM=$((TO - 99))                       # 100-block window ending at finalized
for n in $(seq "$FROM" "$TO"); do
  hash=$(curl -s -X POST -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["'"$(printf '0x%x' "$n")"'",false]}' \
    "$RPC" | jq -r .result.hash)
  adapter-subject-backends/witness-db/fetch-witness.sh \
    "$RPC" "$hash" "bundles/block-$n-$hash.json"
done
```

**Optional — a pinned reference corpus.**
[`docs/corpus/mainnet-25625000-25625099.tsv`](./corpus/mainnet-25625000-25625099.tsv)
lists one specific 100-block corpus (block number, block hash, bundle
SHA-256). You do **not** need it to evaluate engines. Its purpose is workload
identity: two parties who capture the same pinned blocks can compare numbers
block by block. Those blocks sit well below the current head, so capturing
them is subject to the depth cost described below. The bundle files
themselves total ~1.4 GB and are not stored in git.

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
Depth is a capability question before it is a cost question: serving a witness for
an old block requires the node to reconstruct that block's state, so a pruned node
may be unable to serve this corpus at all, and a full node may only manage it within
its reconstruction range (see `mainnet-replay.md` §4, which also notes the memory
cost). If your node cannot reach these blocks, capture your own window at the
finalized tip instead — engine comparisons are internal to a corpus. Where depth is
merely expensive rather than impossible, raise or disable the database
read-transaction timeout (`--db.read-transaction-timeout 0`) — deep witness
generation can exceed the 300 s default. The finalized-tip loop above avoids
this cost entirely; it only applies when capturing a pinned historical window
such as the reference corpus.

The SHA-256 column identifies the exact bundle bytes used with this corpus; a
bundle re-captured from a different node or reth version can differ byte-wise
while describing the same block — the block hash is the anchor, and
`verify-witness` checks a bundle's internal integrity either way.

---

## 8. revmc: a separate witness path with its own baseline

`adapter-subject-backends/revmc-witness/` is a standalone binary that replays
witness bundles through reth's block executor with the experimental
[revmc](https://github.com/paradigmxyz/revmc) JIT backend. It is **not** an
EVMC backend and is not run through `replay-block`/`replay-batch`; it has its
own CLI, timing protocol, and — importantly — its own dependency baseline.

Two revmc paths exist around this repository, and they measure different
things:

| | in-tree `--jit` (see `mainnet-replay.md` §3) | `revmc-witness` adapter |
|---|---|---|
| Input | datadir | witness bundles |
| State reads in measured window | database (MDBX) | in-memory strict witness DB |
| JIT compilation | overlaps the measured window (background promotion) | excluded: `--lane resident` starts the timer only after every unique witness program is compiled and runtime queues are empty |
| Host reth crates | this repository (v2.4.1 base, revm 41) | upstream `paradigmxyz/reth@70fb52e5fc` (v2.5.0-dev, revm 42), pinned by git in the crate's `Cargo.toml` |

**The revmc pin must carry the worker-stack fix.** revmc spawns JIT codegen
workers without setting a thread stack size, so they get the platform default
(~2 MB), which LLVM instruction selection overflows on the largest mainnet
contracts — the process aborts with `thread 'revmc-NN' has overflowed its
stack` rather than failing the compilation. Over the §10 corpus this killed 38
of 1000 blocks, clustered on the blocks holding the biggest code, so the loss
was biased rather than random. Both the workspace `[patch]` in the root
`Cargo.toml` and this crate's own `Cargo.toml` therefore pin
`abmcar/revmc@42d475f46a480c00fe73a70879e4ea633ee8fbbd` — upstream
`paradigmxyz/revmc@cf68a87f` plus that one commit
([`patches/revmc-jit-worker-stack-size.patch`](../patches/revmc-jit-worker-stack-size.patch)).
Pinned by commit, not by branch: a branch pin stops being reproducible the
moment the branch moves. With the fix in place those 38 failures drop to zero.

The two paths answer different questions and neither is a substitute for the
other: `--jit` answers "how fast does a node run a chain segment with revmc"
(storage included, compilation overlapping), while the adapter answers "how
fast is the revmc execution kernel" (state in memory, compilation excluded).
A number produced by one path is not comparable to a number produced by the
other.

The baseline difference also matters *across* backends: results from this
adapter (and its native-REVM reference leg, revm 42) were produced with
different reth scaffolding inside the measured window than the
`replay-block`/`replay-batch` backends of §1–§6 (this repository, revm 41).
All paths execute the same consensus rules over the same witness input and
every block is gated on pre/post state-root verification, but treat
cross-backend ratios that span the two baselines as carrying an unquantified
host-version component. Build and usage details are in
`adapter-subject-backends/revmc-witness/README.md`.

---

## 9. geth: the native-stateless reference leg

`geth-witness-replay` feeds the same bundles to go-ethereum's own
`core/stateless` execution and emits a report with the same shape as the other
engines (pre/post state root, five commitments, phase timings). It is a
separate binary built from a geth tree, not from this repository.

**It is not checked in as source, on purpose.** The CLI lives under geth's
`cmd/` and derives from GPL code, while this repository is Apache-2.0 + MIT.
It ships as patches instead, the same way the DTVM and revmc changes do, so
the GPL text stays on the GPL side:

| Patch | Applies to | Gives you |
|---|---|---|
| [`patches/geth-witness-replay-cli-stock.patch`](../patches/geth-witness-replay-cli-stock.patch) | stock go-ethereum v1.17.4 | the CLI, no `core/` change at all |
| [`patches/geth-witness-replay-cli-fork.patch`](../patches/geth-witness-replay-cli-fork.patch) | v1.17.4 **plus** the metrics patch below | the CLI, execution-only timing |
| [`patches/geth-execution-metrics.patch`](../patches/geth-execution-metrics.patch) | `core/` of v1.17.4 (3 files, +62 lines) | splits setup / execution / validation |

```bash
git clone https://github.com/ethereum/go-ethereum && cd go-ethereum
git checkout v1.17.4                       # 36a7dc72e96b3f42846be925cfeb2fad18489917
git apply /path/to/reth/patches/geth-execution-metrics.patch          # fork variant only
git apply /path/to/reth/patches/geth-witness-replay-cli-fork.patch
go build -o geth-witness-replay ./cmd/geth-witness-replay             # needs go 1.24.x
./geth-witness-replay -input BUNDLE.json
```

**Which variant you build changes what the number means.** The two CLIs differ
by twelve lines, but not in what they measure:

| | stock | fork |
|---|---|---|
| `core/` changes | none | 3 files, +62 lines |
| timed region | `Setup + Execution + Validation` | **`Execution` only** |
| report `schema` | `geth-witness-replay.report.v1-stock` | `geth-witness-replay.report.v1` |
| read the time from | the single whole-run figure | `phaseWallTimeNs.gethExecute` |

On mainnet block 25625046 the stock caliber reads 131.39 ms against the fork's
91.65 ms — **1.43×**, i.e. setup and validation are about 30% of the stock
figure. That gap is systematic, not noise. The stock variant is the easier
build and is fine on its own terms, but a stock number placed beside the
`rethSubjectExecute` figures of §3 is not comparing the same region. **The
§10 campaign uses the fork variant**; check the `schema` field of any report
before trusting a cross-engine ratio.

The patches were derived from the tree that produced the measured binary; run
`git apply --check` against a fresh v1.17.4 before relying on them.

---

## 10. Running the five-engine comparison over the 1000-block corpus

This is the concrete campaign the sections above serve: DTVM, evmone, geth,
revmc, and REVM over one thousand consecutive mainnet blocks.

### 10.0 Pin this document too

This file pins every dependency by commit and was itself addressed only by a branch,
which is the same failure it warns about. Two commits matter and they are not the
same one:

- **The harness that produced the numbers.** `replay-batch`, `replay-block` and
  `verify-witness` were built from
  `abmcar/reth@62bae4417dbe1d16eee71cf080cd838ecd4f757e`, from a clean tree. That is
  the code that ran; check it out if you want the measurement path byte-for-byte.
- **This document.** Later commits on `evmc-extra-access-fix` correct the text
  without touching the harness — a wrong module-cache sizing rule, a wrong DTVM pin,
  a mixed-source REVM column, and the gaps a reproducibility audit found. Read the
  branch tip, not `62bae441`, or you will read the errors this section exists to
  record.

The split is deliberate: re-running the harness to fix prose would invalidate the
numbers the prose describes.

### 10.1 The corpus

Blocks **25817835–25818834**, 1000 consecutive blocks with no gaps, ~16 GB of
bundles. Measured properties, not estimates: **15,415 unique contracts**,
147.9 MB of novel bytecode, a mean of 361 contracts per block.

[`docs/corpus/mainnet-25817835-25818834.tsv`](./corpus/mainnet-25817835-25818834.tsv)
pins it: block number, block hash, and the SHA-256 of the bundle this campaign used,
one row per block. The block hash is the anchor — a bundle re-captured from a
different node or reth version can differ byte-wise while describing the same block,
and `verify-witness` checks a bundle's internal integrity either way.

Capture it with the §7 loop (`FROM=25817835`, `TO=25818834`). These blocks sit
well below any current head, so the depth cost of §7 applies — raise the read
transaction timeout (`--db.read-transaction-timeout 0`) or capture your own
window near your node's finalized tip instead. **A different window is fine**;
engine comparisons are internal to a corpus. What is not fine is comparing
engines across *different* block sets — see §10.3.

### 10.2 What each leg needs

| Engine | Binary | Version pin | Notes |
|---|---|---|---|
| DTVM (timing) | `replay-batch` | `abmcar/DTVM@8403be3f7f390afe9ea4d5366305ea7b9da24fa0` | `RETH_SUBJECT_BACKEND=dtvm-eager`; needs `DTVM_EVM_MAX_MODULE_CACHE_SIZE=131072` (§5 — do not size this by unique-contract count) and a metrics-OFF build for `--production-timing` |
| evmone | `replay-block` | `DTVMStack/evmone@a4a0e47aff903a47a6be133c67ad106c706fe566` | `RETH_SUBJECT_BACKEND=evmone-advanced`; no batch path (§6) |
| geth | `geth-witness-replay` | go-ethereum v1.17.4 `36a7dc72e`, **fork** variant | §9 |
| revmc | `revmc-witness-adapter` | `abmcar/revmc@42d475f`, `--lane resident` | §8; its own reth baseline |
| DTVM (qualification) | `replay-batch`, default mode | `abmcar/DTVM@f9e25be` on `metrics/evm-phase-metrics-on-8403be3` | the same tree with `patches/dtvm-evmc-phase-metrics.patch` applied and `-DZEN_ENABLE_EVMC_PHASE_METRICS=ON`. The patch is written against `338d123` and does **not** apply to `8403be3`; that branch records the resolution so you do not have to redo it. Needed for §10.4 step 2 |
| REVM 41 | — | in-record | `phaseWallTimeNs.rethRevmExecute` **from the DTVM production-timing records only**. The evmone leg carries the same field, but as a cold fresh-process figure against the DTVM batch's hot 8-pass median; mixing the two puts different measurements in one cell |

Give DTVM its commit, not its branch name. **`8403be3` on
`fix/evm-jit-null-membase`** is the tree the measured library was built from. It is
`feat/evm-persistent-code-cache` at `03b542e` plus one commit, and every part of it
matters here:

| in `8403be3` | what it does | upstream |
|---|---|---|
| persistent code cache | `code_cache_dir` / `code_cache_mode` (§5) | fork only |
| `37b36b1` | `DTVM_EVM_MAX_MODULE_CACHE_SIZE` — without it the bound is a compile-time 4096 and this corpus is unrunnable | fork only |
| `ea8f80d` | gas on an exceptional halt | DTVMStack/DTVM#605 |
| `03b542e` | a null dereference reported as an EVM memory fault, i.e. as a *consensus* status | DTVMStack/DTVM#604 |
| `8403be3` | the JIT reloads its cached memory base, not only the cached size | DTVMStack/DTVM#607 |

**Building `03b542e` instead is not a smaller version of this — it does not run.**
Blocks 25818502 and 25818530 fault in generated code without `8403be3`, and because
`03b542e` reclassifies that fault as an internal error rather than a consensus one,
the replay fails closed instead of silently producing wrong gas. You would get 998
of 1000 blocks and no way to form the full intersection.

The branch keeps moving; the commit does not — the same reason §8 pins revmc by sha.

Two of these run against a different host baseline than the others: revmc (and
its native-REVM reference leg) is built on upstream reth v2.5.0-dev / revm 42,
while the EVMC legs use this repository at v2.4.1 / revm 41. §8 explains why
that gap is unquantified; do not silently fold it into an engine ratio.

The DTVM leg is the expensive one. With no code cache it is roughly 6.4 hours
over the good blocks; with a fully populated on-disk cache the twelve passes take
about 80 minutes. At the *default* L1 bound of 4096 it degrades to hundreds of
hours from eviction thrash, which is why §5's environment variable is not
optional here — and §5 explains why 16384 is also not enough despite the corpus
having only 15,415 unique contracts.

### 10.2b Repetitions and extraction, per leg

`replay-batch` records carry `timingUse` and a `correctness` wrapper; a bare
`replay-block` or `geth-witness-replay` report carries neither, so §3's jq recipe
does not apply to those legs. What the published table used:

| leg | invocation | reps/block | keep a sample when | field | aggregate |
|---|---|---|---|---|---|
| DTVM | `replay-batch --production-timing`, one process, whole corpus | 8 (`M0`–`M7`) | `timingUse && correctness.passed` | `replay.phaseWallTimeNs.rethSubjectExecute` | median over the 8 |
| REVM 41 | same records | 8 | same | `replay.phaseWallTimeNs.rethRevmExecute` | median over the 8 |
| evmone | `replay-block`, fresh process each time | 8 | `postStateRootVerified` | `replay.phaseWallTimeNs.rethSubjectExecute` | median over the 8 |
| geth | `geth-witness-replay -input`, fresh process each time | 8 | `postStateRootVerified` | `phaseWallTimeNs.gethExecute` | median over the 8 |
| revmc | `revmc-witness-adapter --lane resident`, chunks of 100 bundles | 1 | `allBlocksMatch` on the chunk | `blocks[].measuredElapsedNs` | single value |
| REVM 42 | same revmc invocation | 1 | same | `blocks[].revmReferenceElapsedNs` | single value |

No repetitions are discarded: for the batch legs the discard is the protocol's own
(`C0`, `G0`, `W0`, `W1` are not `timingUse`), and the per-process legs have no warm-up
to discard because each invocation starts cold by construction. revmc is measured once
per block because its `resident` lane already excludes compilation by starting the
timer after its JIT queues drain, so a repetition would measure the same warm state.

For a bare report the gate is the boolean itself:

```bash
jq -r 'select(.postStateRootVerified) | [.blockNumber, .phaseWallTimeNs.gethExecute] | @tsv'
```

`docs/results/five-engine-1000.tsv` is the result of applying exactly this table.

### 10.3 Use the intersection, not each engine's own subset

Not every engine replays every block. Comparing engines on the blocks each one
happens to survive means comparing different question sets, and the resulting
ratios mean nothing. **Take the set of blocks that all engines pass, and run
every engine on exactly that set.**

A full 1000×4 correctness scan (4000 independent replays, no sampling) found
46 blocks failing at least one engine, an intersection of 954. Every one of
those failures has since been root-caused, and all but two are fixed:

| Defect | Blocks | Cause | State |
|---|---|---|---|
| Cold-account probe order | 5 | evmone-family engines probe a call target before charging the 9000 transfer cost; revm charges first and skips the cold load when the frame cannot pay. The frame runs out of gas at the charge, so revm never touches the account. Upstream evmone v0.23 already reorders this. | fixed in the adapter: three read-only probes answer self-consistently instead of consuming a witness proof |
| Over-strict access gate | 3 | the gate required the subject's DB access sequence to equal the reference's item for item, rejecting the legitimate case where the subject reads *less* | fixed: subject accepted as an order-preserving subsequence, withdrawal tail compared as a set. Applies to `witness-db` and, separately, to `revmc-witness`, which carries its own copy of the gate |
| revmc worker stack overflow | 38 | §8 | fixed by the commit pin |
| DTVM halt gas rule | 2 | three sites overwrote the gas the halt path had just set with `instance.getGas()`, which is not authoritative after a halt | fixed upstream: DTVMStack/DTVM#605 |
| DTVM JIT null dereference | 2 | EVM memory is allocated lazily, so a frame starts with `MemoryBase == nullptr`. When a frame's first growth happens inside one of fifteen runtime helpers rather than through `expandMemoryIR`, the helper allocates and sets the instance's base, the JIT reloads only its cached *size*, and the entry-time null survives in the cached base. The reloaded size then keeps the expansion branch — the only other refresh site — untaken. Depth 0 on a fresh instance qualifies; no nested call is required. | fixed upstream: DTVMStack/DTVM#607 |

All 46 are closed. A full 1000-block rescan under the fixed libraries returns
**1000 OK, 0 failures**, so the four-engine intersection is now the entire
corpus: **1000 / 1000**, up from 954. Run the campaign on all 1000 blocks.

This matters operationally as well as statistically: `replay-batch` holds one
process across the whole batch, so a single failing block aborts the entire
DTVM leg rather than skipping it. A corpus that is not fully clean cannot be
timed in one pass at all.

### 10.4 Order of operations

1. Scan for correctness first, one engine at a time, and build the
   intersection. Do not skip this because a previous run's block list is
   lying around — engine or adapter changes move the set.
2. Run the §4 diagnostic pass with a metrics-ON DTVM build to *prove* no
   compilation happened inside the measured region.
3. Run the §3 `--production-timing` pass with the metrics-OFF build for the
   numbers, on the intersection only.
4. Extract with the `jq` recipe in §3, pairing each subject median against the
   `rethRevmExecute` median from the same records.

**Run every measured invocation alone.** Nothing in any output records what else
was on the machine, and the temptation is strong: the per-process legs (evmone, geth)
are one invocation per bundle, so `xargs -P$(nproc)` over a thousand bundles is the
obvious move, and running two engine legs at once would halve a long night. Do not.
Concurrency inflates the measured phase, and — this is the part that matters — it
does so by **different amounts for different engines**, so it moves the cross-engine
ratios this campaign exists to report, not just the absolute numbers. Calibrated on
60 blocks, three repetitions, against a serial baseline:

| parallelism | geth | evmone |
|---|---|---|
| 8 concurrent | +2.9% (p90 +7.5%) | +4.8% (p90 +8.8%) |
| 16 concurrent | +10.7% (p90 +18.2%) | +11.6% (p90 +27.8%) |

A 56-core machine does not absorb this; the effect is there at 8 concurrent
processes on 56 cores. The published table was produced with the DTVM leg alone at
parallelism 1 and the other three at 4, and each leg wrote its own
`legs/<leg>.env.json` recording parallelism, start time and load average — because
that boundary is invisible in the JSON otherwise. Parallelism is fine for the
*correctness* scan of step 1, which is not a measurement.

Steps 2 and 3 are two different runs against two different libraries by
design; skipping step 2 leaves the timing valid but unqualified.

**Do not treat step 2 as optional.** "Unqualified" sounds like a missing formality;
it is not. `--production-timing` checks the single-VM invariant and the correctness
gate, and nothing else. It does not check that the measured passes were free of
compilation and cache eviction, so a run whose module cache is too small produces
numbers that look ordinary and are wrong by a factor. That is not hypothetical: the
first version of §10.5's DTVM column was contaminated exactly this way, and only the
diagnostic protocol surfaced it.

### 10.5 Results

One thousand blocks, four engines, full intersection, nothing dropped. Per-block
medians over each engine's repetitions.

| engine | boundary | median | mean | p90 | vs DTVM (per-block median) |
|---|---|---:|---:|---:|---:|
| revmc | adapter resident timer | 55.51 ms | 62.73 | 109.89 | 0.63x |
| REVM 41 | `rethRevmExecute` | 61.96 ms | 69.71 | 123.22 | 0.71x |
| REVM 42 | `revmReferenceElapsedNs` | 66.79 ms | 76.87 | 135.79 | 0.76x |
| **DTVM** | `rethSubjectExecute` | **87.15 ms** | 94.99 | 160.86 | 1.00x |
| DTVM, module only | `jitActiveWallNs` net of lookups | 73.30 ms | — | — | 0.84x |
| geth | `gethExecute` | 221.81 ms | 1366.83 | 4340.46 | 2.52x |
| evmone | `rethSubjectExecute` | 239.02 ms | 256.74 | 434.21 | 2.70x |

The per-block table is checked in as
[`docs/results/five-engine-1000.tsv`](./results/five-engine-1000.tsv) — one row per
block, one column per engine, so any claim here can be recomputed rather than taken
on trust.

The `dtvm_module_ms` column and the "module only" row peel the DTVM figure down to
the compiled module itself, in four steps that are subtractive because they come from
one run: `rethSubjectExecute` 87.36 → `rethSubjectRunExecLoop` 80.41 (−6.95, reth's
executor construction and state extraction) → `topLevelExecuteWallNs` 76.66 (−3.75,
entering DTVM) → `jitActiveWallNs` net 73.30 (−3.37, the module-cache lookup and its
full-bytecode `memcmp`). Those come from the diagnostic build, which on the same
blocks runs 0.49% slower than the production build (p10–p90 1.0000–1.0105) — the
bias runs against DTVM, not for it.

**Do not divide 73.30 by REVM.** Of the three available ratios only the middle one is
symmetric: wide/wide 1.41x, tight/tight **1.45x**, net/tight 1.32x. The last is
DTVM-favourable because the module lookup was removed from one side and REVM has no
module cache to remove anything from — its equivalent per-contract analysis is inside
its execution and cannot be separated. 73.30 answers "how fast is the compiled module",
not "how much faster is DTVM than REVM"; for that, use 1.45x.

Read the per-block ratio spread, not just the median. evmone's is 2.22x–3.44x
(p10–p90) and geth's is 2.11x–40.04x; a single number hides that geth is bimodal
(290 of the 1000 blocks exceed two seconds while the median is 221 ms, and those
290 carry 90% of geth's total time).

The two REVM legs are the same engine on different host baselines — revm 41 inside
this repository, revm 42 inside the revmc adapter's upstream reth. Their per-block
absolute difference has a median of 3.2% and a p90 of 23.9%, which is the honest
size of the "unquantified host-version component" this document used to warn about
without measuring: fine to cross for an aggregate, not fine to cross for one block.

**What this table is not.** The boundaries in column two are not the same region.
DTVM and evmone are measured through `rethSubjectExecute`, which includes reth-side
executor construction and state extraction; geth's `gethExecute` is geth's own
execution phase and never pays that; revmc's timer starts after its JIT queues
drain. The EVMC legs therefore carry a harness cost the other two do not — 8.5%
for DTVM, measured by comparing against `rethSubjectRunExecLoop` (87.15 vs 80.09 ms). Deeper still, DTVM's figure contains every host callback crossing the
EVMC bridge and, on this corpus, about 2,100 module-cache lookups per block each
running a full-bytecode `memcmp` under the mandatory strict validation. Read the
DTVM number as *DTVM through the EVMC bridge with strict validation*, not as a
measurement of its code generation.

### 10.6 What the DTVM number contains

Read `rethSubjectExecute` and you get the wide boundary: executor construction,
`execute_one`, and state extraction. `replay-batch` — and only `replay-batch`,
since `execution_metrics::enable()` is called from the batch session
constructor — also fills `rethSubjectRunExecLoop.wallNs`, the sum over each
top-level execution of revm's frame-execution loop. On this corpus the gap is
small: 87.15 ms wide against 80.09 ms tight for DTVM, 61.96 against 55.52 for
REVM — 8.5% and 10.2% respectively. The fixed overhead is the same in absolute
terms on both arms, so it is a larger *fraction* of REVM's smaller number, which
means the tight boundary makes DTVM look slightly **worse**, not better: 1.44x
against 1.41x.

Neither boundary isolates code generation. Both include every host callback
made from inside the loop, and for the EVMC backends every one of those crosses
the bridge. A metrics-ON build measured, on this corpus, roughly **2,100 module-cache
lookups per block** against 190 top-level executions — the lookup is per call
*frame*, and the L0 inline cache is disabled in the source. Each of those
lookups runs a full bytecode `memcmp`, because
`DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true` is mandatory here and the relaxed
mode compares only a 256-byte head and tail. Nested frames do not reopen the
timing window, so all of that sits inside the DTVM figure.

State it as what it is: **DTVM through the EVMC bridge with strict cache
validation, against native REVM.** Not codegen against interpreter.
