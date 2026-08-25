# Finalized tip replay is automatic; moving tips remain reorg-aware

> **Timing engines? Read [`docs/witness-replay.md`](../../docs/witness-replay.md)
> first.** `replay-block` (used throughout this README) is a single-bundle
> correctness tool: every invocation constructs a fresh EVMC VM with an empty
> code cache. Looping it over many bundles makes a compiling engine re-pay full
> compilation per block. Performance measurement goes through `replay-batch`,
> which holds one VM across the whole run and fails closed if that breaks.
> Note also: the `DTVM_LIBRARY` / `DTVM_LIBRARY_SHA256` variables in this
> README are read only by `cargo test`; the replay binaries take
> `RETH_SUBJECT_BACKEND` / `RETH_SUBJECT_LIBRARY` /
> `RETH_SUBJECT_LIBRARY_SHA256`.

This directory is restored output. Its versioned source is the
`dtvm-run-reth-replay` skill in DTVMDotfiles. Use
`scripts/restore-reth-replay-suite.sh sync TARGET` from that skill to update
this copy and `check TARGET` to prove all 30 source files are byte-identical;
do not maintain this working copy directly.

For primary/standby Reth, independent canonical quorum, classified retries,
cross-process resume state and evidence sealing, use `reth_rpc_ha.py` with the
secret-free schema in `config/reth-rpc-ha.schema.json`. The production
architecture and operating commands are in `HA_RPC_OPERATIONS.md`. The
existing scripts below remain the capture and strict-replay protocol behind
that HA layer.

`replay-tip.sh` now captures and replays a Mainnet Reth chain tip without
manually selecting a hash. It defaults to `finalized`, pins the tag's number
and hash, fetches the header, canonical execution witness, and raw block by
that hash, runs strict replay, and then verifies the canonical hash again at
the original height.

Finalized replay can be automated stably under the verified Osaka scope.
`safe` and `latest` are moving views: hash pinning plus a canonical recheck and
bounded retry manages a reorg, but cannot eliminate one. A stale successful
execution is discarded and never published.

## Run the finalized tip directly

The output path must not exist. The command creates a private attempt
directory beside the requested output, publishes `bundle.json`, `replay.json`,
and `result.json` with one atomic directory rename only after the canonical
recheck passes, and refuses to overwrite an existing artifact.

```bash
./replay-tip.sh \
  --output /path/to/finalized-replay \
  http://127.0.0.1:8545
```

`--tag finalized|safe|latest` selects the requested tag. The default is
`finalized`. The default retry limit is three attempts; `--max-attempts` accepts
1–10:

```bash
./replay-tip.sh \
  --tag latest \
  --max-attempts 3 \
  --output /path/to/latest-replay \
  http://127.0.0.1:8545
```

Every attempt captures the tag with `eth_getBlockByNumber(tag, false)`. All
subsequent header, witness, and raw-block requests use the captured hash.
After capture and replay, including a failed capture or replay after the tag
was fixed, the command queries `eth_getBlockByNumber(capturedNumber, false)`.
If the hash changed, the attempt is stale and is discarded. Exhausting the
limit returns `reorg_retry_exhausted` and leaves no output directory.

The command emits one JSON document. Success includes `requestedTag`,
`capturedBlockNumber`, `captureHash`, `recheckHash`, `attemptCount`,
`reorgDetected`, `stale`, the witness method/mode/policy, the full strict
replay report, header commitments, and `postStateRoot`. Failure uses the same
identity and retry fields plus `failureCategory` and `missingCapabilities`.
The RPC URL and credentials are neither printed nor stored in the artifacts.

## Diagnose the local Reth witness source

Production replay is Mainnet-only and requires a local Reth endpoint with the
hash-addressed canonical witness extension. This command checks chain ID, the
selected tag, `debug_getRawHeader`,
`debug_executionWitnessByBlockHash(hash, "canonical")`, and
`debug_getRawBlock` without publishing a bundle:

```bash
./replay-tip.sh \
  --diagnose-only \
  --tag finalized \
  http://127.0.0.1:8545
```

The readiness JSON reports `status: "ready"` only when all capabilities
succeed. Otherwise `missingCapabilities` names the unavailable or malformed
method. This diagnostic generates a real canonical witness and can therefore
take comparable time to capture.

## Production requires the canonical hash API

`fetch-witness.sh` now defaults to `--policy production`. Production accepts
only `debug_executionWitnessByBlockHash(hash, "canonical")`; it does not fall
back to a number-based witness:

```bash
./fetch-witness.sh \
  --policy production \
  http://127.0.0.1:8545 \
  0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  /path/to/bundle.json \
  canonical
```

An explicit `--policy best-effort` retains compatibility with
`debug_executionWitness(number)` and performs a height/hash recheck before
writing. That path is not the production tip-replay policy. A public endpoint
may also omit all debug methods.

Standard `eth_getProof` responses must not be described as a complete
execution witness. They may omit opaque siblings or preimages needed to
authenticate a post-state deletion collapse. The adapter never fills missing
state with zero or guesses those trie nodes.

## A full raw block binds the witness bundle

```json
{
  "targetHeader": "0x...",
  "targetBlockHash": "0x...",
  "targetBlock": "0x...",
  "witness": {
    "state": ["0x..."],
    "codes": ["0x..."],
    "keys": ["0x..."],
    "headers": ["0x..."]
  },
  "accessManifest": {
    "accounts": ["0x..."],
    "storage": [
      {
        "address": "0x...",
        "slot": "0x..."
      }
    ]
  }
}
```

`targetBlock` remains optional for backward-compatible header-only imports.
Strict replay requires it. When present, the importer requires exact canonical
RLP with no trailing bytes, an embedded header equal to `targetHeader`, and a
decoded hash equal to `targetBlockHash`. It also binds the transaction, ommers,
and withdrawals roots to the embedded header.

The witness `keys` field is auxiliary preimage data, not an access allowlist.
Each account-qualified query must resolve a complete inclusion or exclusion
proof. `accessManifest` is optional and only preloads paths through the same
proof-checked database.

The helper validates all responses, writes through a temporary file, installs
the bundle atomically, and refuses to overwrite an existing file. Strict
replay still verifies the raw block and witness together; a structurally valid
but mismatched capture cannot become a successful result.

Require the full raw-block binding before replay:

```bash
cargo run --locked --offline --bin verify-witness -- \
  --require-target-block /path/to/bundle.json
```

The verifier reports the raw binding and body commitments, byte and
transaction counts, target and parent identities, and verified pre-state root.
Header-only bundles remain readable without `--require-target-block`, but the
replay CLI rejects them.

## Run strict replay from a sealed Linux memfd

Strict replay requires Linux `memfd_create`, file sealing, and a mounted
`/proc/self/fd`. Supply the DTVM library path, its independently computed
SHA-256, and strict address-cache validation:

```bash
DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true \
DTVM_LIBRARY=/absolute/path/libdtvmapi.so \
DTVM_LIBRARY_SHA256=4ef7059a52b4a5e48fd21d181e5d25f5ed4baf9bf90be28086b792f920ad73fd \
cargo run --locked --offline --bin replay-block -- /path/to/bundle.json
```

The CLI copies the library into a memfd while hashing those exact bytes. It
checks the supplied hash, applies write, grow, shrink, and seal seals, and
loads DTVM through `/proc/self/fd/<fd>`. Replacing the source path cannot
change the bytes loaded for that run.

Before execution, the CLI validates the raw body, validates the header against
the witness-provided parent, performs consensus preflight, and recovers every
sender. It runs the complete block through two independent `WitnessDb` and
`BasicBlockExecutor` instances.

Success JSON includes `postStateRoot`, `postStateRootVerified: true`, and true
flags for:

- `differentialMatch`, `rawBound`, and `preExecutionCommitments`;
- gas used, receipts root, logs bloom, requests hash, and blob gas;
- pre-state-root and post-state-root verification.

## Sparse proofs verify real post-state transitions

Stock Reth and real DTVM compare `BlockExecutionResult`, strict access order,
and semantic `BundleState`. Each side applies its state changes to its own
sparse trie using canonical account and storage batches. Both roots must equal
the target header's `stateRoot`.

Terminal extension divergence can prove exclusion without inventing an empty
account. Proof-backed partial-MPT updates can insert new leaves. A deletion
that would collapse an opaque sibling remains fail closed because the missing
node could change the root.

Reth's `MainnetHandler`, `JournaledState`, and `BasicBlockExecutor` remain the
production host for block rules, system calls, state, and settlement. DTVM
executes EVM bytecode only. The host covers all four call modes, Reth precompiles, nested
`CREATE`/`CREATE2`, EIP-6780 `SELFDESTRUCT`, top-level type 0–3 creation,
type-4/EIP-7702 execution, and canonical EIP-2935, EIP-4788, EIP-7002, and
EIP-7251 system predeploys. Nested one-hop chained EIP-7702 delegation has
direct differential coverage. Witness and unsupported-capability errors
retain structured access counts, transaction indexes, and reasons.

## Three finalized blocks, 138 Rust tests, and 24 shell scenarios are green

| Block | Transactions / receipts | Raw bytes | Gas used | Blob gas used |
|---:|---:|---:|---:|---:|
| 25,625,638 | 125 / 125 | 46,420 | 7,693,091 | 524,288 |
| 25,628,784 | 159 / 159 | 122,814 | 25,479,657 | 131,072 |
| 25,629,035 | 163 / 163 | 194,324 | 58,115,303 | 0 |

All three blocks match their pre- and post-state roots and every checked header
commitment. Block 25,629,035 contains multiple type-4 transactions and a
top-level creation. The older create-first bundle is not counted as an adapter
failure because its later segment lacks Geth witness data and stock Reth also
fails on it.

`witness-db` independently passes 55 of 55 Rust tests. Together with 28 adapter and
55 transaction tests (12 unit and 43 real-DTVM integration tests), the full
external stack passes 138 of 138 with zero failed or ignored tests and zero
compiler warnings. Eight hermetic `fetch-witness` scenarios and 16 hermetic
tip-replay/readiness scenarios additionally pass with fake curl/RPC and fake
replay orchestration. They cover all three tags, one reorg followed by success,
retry exhaustion, missing canonical APIs, malformed/null/not-found tags,
raw/witness rejection, exact capability diagnostics, credential non-leakage,
and artifact non-overwrite/cleanup.

The HA layer adds 19 hermetic Python scenarios for primary failure, standby
failover, exact capability loss, chain/finality mismatch, reorg and quorum
disagreement, per-endpoint vote retry/exhaustion, 429 `Retry-After`, 5xx,
timeout, malformed responses, corrupt
partial-cache recovery, cross-process resume, loopback health/readiness/metrics,
gateway authorization/method/readiness isolation, numeric-origin/header
normalization, atomic no-overwrite publication races, published/replay evidence
tamper rejection, corpus/replayer identity binding, ancestor-symlink rejection,
workflow serialization, idempotent evidence sealing, offline replay isolation,
and credential redaction.

The finalized block evidence manifest is:

```text
build/evidence/finalized-replay-2026-07-28/manifest.json
```

Inspector remains unsupported. Future EIP-8037 reservoir execution, block
access list headers, and slot-number headers fail closed. No EOF or
`EOFCREATE` claim is made. A future fork may still require an adapter update,
and a continuously reorganizing `safe` or `latest` height can exhaust the
retry limit. This README belongs to the external adapter; it is not tracked by
DTVM Git.
