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
| `RETH_SUBJECT_BACKEND` | `dtvm-eager`, `dtvm-profile-guided`, `evmone-advanced`, `evmone-baseline` |
| `RETH_SUBJECT_LIBRARY` | path to the EVMC shared library |
| `RETH_SUBJECT_LIBRARY_SHA256` | expected SHA-256 of that file; startup fails on mismatch |
| `DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION` | must be `true` for the DTVM backends |
| `RETH_SUBJECT_EVMC_OPTIONS` | optional; comma-separated `name=value` EVMC options applied after the mandatory ones, e.g. `code_cache_dir=/var/cache/dtvm,code_cache_mode=rw`. A malformed entry, or one the library rejects, fails startup with an error rather than being silently ignored. Which names are accepted depends entirely on the loaded library — see §5. Because these are applied *after* the mandatory options, they can override one: `mode=interpreter` turns a `dtvm-eager` leg into a DTVM interpreter leg with no code change (§11.1). Note the record still reports `backend=dtvm-eager`, so the true mode has to be recorded out of band. |

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
default stands. Count your corpus first and set the bound above it — the
1000-block corpus of §10 holds 15,415 unique contracts and is run at 16384.

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
setup; the artefact is `libevmone.so`. One library, two legs:

| Backend | EVMC option set | What it selects |
|---|---|---|
| `evmone-advanced` | `("advanced", "")` | the Advanced interpreter |
| `evmone-baseline` | *none* | the Baseline interpreter |

**Baseline is evmone's default, and has been since 0.9.0 (2022-08-30).**
`lib/evmone/vm.cpp` installs `baseline::execute` in the VM constructor;
`advanced` is an opt-in that overwrites it. Upstream's own release notes give
Baseline as 18% faster than Advanced with "over 8x smaller code analysis cost",
which is why they made the switch. On this corpus the gap is larger still —
§11.2. An `evmone-advanced` number is therefore a measurement of the
**non-default** interpreter and should say so.

---

## 6. Backend coverage of the batch paths

| Backend | `replay-batch` | Notes |
|---|---|---|
| `dtvm-eager` | yes | both modes of §3/§4 |
| `dtvm-profile-guided` | **no** | only `replay-block`; each invocation starts a fresh VM with an empty profile window, so per-bundle runs never accumulate enough heat to trigger JIT — numbers reflect near-pure interpretation |
| `evmone-baseline` | **no** | `replay-block` only. `replay-evmone-batch` rejects it by design: that path exists to read the instrumented Advanced diagnostic ABI, which has no Baseline counterpart. Timing legs use `replay-block`, so nothing is lost. |
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
for blocks well below the head, raise or disable the database
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

### 10.1 The corpus

Blocks **25817835–25818834**, 1000 consecutive blocks with no gaps, ~16 GB of
bundles. Measured properties, not estimates: **15,415 unique contracts**,
147.9 MB of novel bytecode, a mean of 361 contracts per block.

Capture it with the §7 loop (`FROM=25817835`, `TO=25818834`). These blocks sit
well below any current head, so the depth cost of §7 applies — raise the read
transaction timeout (`--db.read-transaction-timeout 0`) or capture your own
window near your node's finalized tip instead. **A different window is fine**;
engine comparisons are internal to a corpus. What is not fine is comparing
engines across *different* block sets — see §10.3.

### 10.2 What each leg needs

| Engine | Binary | Version pin | Notes |
|---|---|---|---|
| DTVM | `replay-batch` | `abmcar/DTVM@03b542e6b765685795dee2d4a8a3efcba91d0e2a` | `RETH_SUBJECT_BACKEND=dtvm-eager`; needs `DTVM_EVM_MAX_MODULE_CACHE_SIZE=16384` (§5) and a metrics-OFF build for `--production-timing` |
| evmone (advanced) | `replay-block` | `DTVMStack/evmone` v0.18.0, sha256 `1316fad3aac3ee21…` | `RETH_SUBJECT_BACKEND=evmone-advanced`; no batch path (§6). The **non-default** interpreter — see §5 and §11.2 |
| evmone (baseline) | `replay-block` | same library | `RETH_SUBJECT_BACKEND=evmone-baseline`; needs the adapter at `abmcar/reth@evmone-baseline-backend` (`f1bc29bfa`). evmone's default mode; §11.2 |
| DTVM (interpreter) | `replay-batch` | the same commit and library as the DTVM timing leg | append `mode=interpreter` to `RETH_SUBJECT_EVMC_OPTIONS`. Everything else identical to the DTVM leg — that is the point of the leg. §11.1 |
| geth | `geth-witness-replay` | go-ethereum v1.17.4 `36a7dc72e`, **fork** variant | §9 |
| revmc | `revmc-witness-adapter` | `abmcar/revmc@42d475f`, `--lane resident` | §8; its own reth baseline |
| REVM | — | in-record | `phaseWallTimeNs.rethRevmExecute`, from the same DTVM/evmone records (§3) |

Give DTVM its commit, not its branch name. `03b542e` is the head of
`feat/evm-persistent-code-cache` at the time of writing and is the tree the
measured library was built from; it carries the persistent code cache, the
module-cache bound of §5, and two engine fixes that landed upstream as
DTVMStack/DTVM#605 (gas on an exceptional halt) and #604 (a null dereference
misreported as an EVM memory fault). The branch keeps moving; the commit does
not — the same reason §8 pins revmc by sha.

Two of these run against a different host baseline than the others: revmc (and
its native-REVM reference leg) is built on upstream reth v2.5.0-dev / revm 42,
while the EVMC legs use this repository at v2.4.1 / revm 41. §8 explains why
that gap is unquantified; do not silently fold it into an engine ratio.

The DTVM leg is the expensive one. With no code cache it is roughly 6.4 hours
over the good blocks; with a fully populated on-disk cache it is about 0.3
hours. At the *default* L1 bound of 4096 — below this corpus's 15,415 unique
contracts — it degrades to hundreds of hours from eviction thrash, which is
why §5's environment variable is not optional here.

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
| DTVM JIT null dereference | **2** | generated code stores through a null memory base in a nested frame; `mode=interpreter` passes both blocks completely, so it is JIT-only | **open** — 25818502 and 25818530 |

After the fixes, rechecking every block that had ever failed gives DTVM 2
failures (the open defect), evmone 0, geth 0, revmc 0 — an intersection of
**998 / 1000**. Until the JIT defect is closed, run the campaign on those 998
blocks and say so; `replay-batch` holds one process across the whole batch, so
a single failing block aborts the entire DTVM leg rather than skipping it.

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

Steps 2 and 3 are two different runs against two different libraries by
design; skipping step 2 leaves the timing valid but unqualified.

---

## 11. Two mode legs: DTVM interpreter and evmone baseline

Added 2026-08-30 on the same 1000-block corpus. Neither is a new engine. Both
hold the harness fixed and vary exactly one thing, which is what makes them
readable where a cross-engine ratio is not.

### 11.0 Provenance groups — read this before quoting any number

The legs were not all measured under the same conditions, and the difference is
large enough to swamp what is being measured. Every column carries a group tag:

| Group | What | Conditions |
|---|---|---|
| **S** | the sealed campaign (§10) | 2026-08-29, four legs running concurrently at P=12 each, `bin-c3b-campaign/` |
| **I** | the DTVM interpreter leg | 2026-08-30, P=1, idle box, the **same binary and library** as the group-S DTVM leg |
| **N** | the evmone pair | 2026-08-30, P=12, idle box, `bin-baselines/replay-block`, both modes back to back |

**Compare within a group.** Across groups, only where a control licenses it —
see §11.3. In particular a group-N evmone number must not be substituted into
the §10 table: §11.4 measures that error at 16%.

### 11.1 DTVM interpreter vs its own multipass JIT

`mode=interpreter` appended to `RETH_SUBJECT_EVMC_OPTIONS`. No code change: the
extra options are applied after the mandatory ones, so this overrides the
`mode=multipass` that the `dtvm-eager` backend sets, and DTVM's `set_option`
accepts it (`src/vm/dt_evmc_vm.cpp:339`).

**Verify the switch took effect; do not assume it.** A first 5-block probe put
the interpreter at 1.03x the JIT, which is not credible on its face — the
obvious suspect being that `code_cache_mode=ro` was feeding precompiled machine
code regardless of mode. Source says otherwise (`dt_evmc_vm.cpp:856` branches
into `executeInterpreterFastPath` before the JIT path, and the code cache is
consulted only on the JIT side), and the decisive check is empirical: same
binary, same library, same 5 blocks, **cache removed**, only `mode=` differing —
the interpreter produced all 60 records in 22.5 s while multipass had emitted 2
records after 9.5 minutes and was still compiling.

Result, 1000 blocks, 8 measured passes:

| Leg | Median | Mean | p90 |
|---|---|---|---|
| DTVM multipass JIT (group S) | 87.15 ms | 94.99 | 160.86 |
| DTVM interpreter (group I) | 96.63 ms | 105.19 | 178.87 |

Per-block **1.106x** [p10 1.072, p90 1.145].

**The multipass JIT buys about 10% over DTVM's own interpreter on real mainnet
blocks.** Two things this does *not* say:

- It is a **system-level** ratio, not a code-generation ratio. Both legs pay the
  same L1 lookup and the same strict full-bytecode `validateCodeMatch` memcmp —
  `executeInterpreterFastPath` calls `findModuleCached` too
  (`dt_evmc_vm.cpp:719`). An equal *additive* cost cancels in the **difference**,
  not the ratio: the ~9.5 ms gap is what codegen buys, while the shared cost
  dilutes the ratio. Using the lookup-subtracted JIT figure (73.30 ms) puts the
  codegen-only ratio nearer 1.13x — an estimate from a different build, not a
  measurement of this leg.
- It says nothing about DTVM's distance from REVM. DTVM's interpreter at
  96.63 ms is still 1.54x REVM's interpreter at 61.96 ms, so that gap is not a
  codegen story either.

### 11.2 evmone baseline vs advanced

Both modes, same binary, back to back, 8 reps:

| Leg | Median | Mean | p90 |
|---|---|---|---|
| evmone advanced (group N) | 199.56 ms | 214.57 | 353.32 |
| evmone baseline (group N) | 123.61 ms | 132.69 | 223.44 |

Per-block **0.614x** [p10 0.563, p90 0.683] — Baseline is **1.63x faster**, a
38% reduction. Upstream measured 18% on their own suite when they made Baseline
the default in 0.9.0. This corpus roughly doubles that, and the reason is
measurable rather than assumed.

Neither path caches analysis across calls; both call `analyze()` at the top of
every `execute()` (`advanced_execution.cpp:26`, `baseline_execution.cpp:309`).
Per call on N bytes of code, Advanced reserves `(N+2)x16 B` of `Instruction`
plus `(N+1)x32 B` of `intx::uint256` push values — about `48N` — against
Baseline's `~1.13N` (a code copy, padding, and a JUMPDEST bitset).

Correlating Advanced's per-block excess over Baseline (median 76.13 ms):

| Predictor | raw r | partial r |
|---|---|---|
| witness bundle size (code/state volume) | +0.867 | **+0.612** (controlling for REVM time) |
| REVM time (real execution work) | +0.780 | **+0.105** (controlling for volume) |

Remove execution work and the excess still tracks volume; remove volume and
execution work explains almost nothing. **Advanced pays per byte analysed, not
per instruction executed** — the wrong trade for mainnet blocks, where thousands
of frames each analyse a multi-KB contract and then execute a small slice of it.

### 11.3 What licenses the one cross-group comparison

Every `replay-block` and `replay-batch` record carries `rethRevmExecute`, a REVM
41 reference leg run on the same block in the same process. It is the control:
the same code, on the same blocks, in both the group-S and group-I runs.

It agrees to **1.015x per block** [p10 1.007, p90 1.026]. That agreement — not
an assumption about the machine — is what allows attributing 87.15 -> 96.63 ms
to `mode=` rather than to run conditions. **Apply the same test before any other
cross-group pairing.** The group-N REVM figure (67.56 ms) does *not* agree with
group S, because a fresh process per block is a different caliber from a hot
8-pass batch median; that is the §10.2 caliber split, and it is why group N is
quoted only against itself.

### 11.4 The drift that makes §11.0 a rule rather than a preference

Same corpus, same evmone library, same mode. Only the harness binary and what
else was on the machine differ:

| | Median |
|---|---|
| advanced, sealed campaign (old binary, alongside 3 other legs) | 239.02 ms |
| advanced, this binary (idle box, alone) | 199.56 ms |
| | **0.842x per block** |

**A 16% drift — comparable to the entire mode effect being measured.** Applying
a mode ratio to the sealed cell without re-running the control would have
produced a number whose dominant error term was the environment, not the mode.
That is why §11.2 re-runs advanced instead of reusing 239.02.

The transferable quantity is the **ratio**, measured with everything else held
constant. Projecting it: `239.02 x 0.614 = 146.8 ms` is what the §10 evmone cell
would read had it used Baseline, putting DTVM at **1.68x** rather than 2.74x
(band 1.54x-1.87x from the p10/p90 ratios). Measured directly on an idle box
instead, DTVM 87.15 vs Baseline 123.61 is **1.42x**. Say which reading you mean.

### 11.5 Reproducing

Scripts, sealed outputs, the consolidated per-block table (`all-engines.tsv`)
and the mode-switch evidence live outside this repository, in
`dtvm-1000block-work/campaign-baselines/`. The adapter change is one enum
variant on `abmcar/reth@evmone-baseline-backend` (`f1bc29bfa`), based on
`62bae4417` — the exact commit the campaign binaries were built from.

Correctness across all three legs: the interpreter leg passed 12000/12000
records; each evmone leg passed 8000/8000 on both `postStateRootVerified` and
`differentialMatch`. No block was excluded from any leg.
