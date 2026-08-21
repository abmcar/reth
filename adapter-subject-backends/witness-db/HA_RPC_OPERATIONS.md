# Adopt three sources after readiness: 1 Reth loss → 2 canonical votes remain.

## Adopt a three-source deployment for available finalized replay

Adopt a three-source steady-state deployment: a self-hosted Reth primary and
replica for
`debug_executionWitnessByBlockHash(hash, "canonical")`. Add an independent
standard Ethereum RPC for canonical reads. After readiness has proved both Reth
roles, one Reth failure leaves one witness source plus two canonical votes.
This topology cannot start or resume while either Reth is already down.

Cold-start tolerance for any one Reth failure requires three witness-capable
Reth nodes. Add an independent standard provider when the canonical quorum
must also span a separate operational failure domain, for four total sources.

The suite in `reth_rpc_ha.py` keeps network capture outside DTVM replay timing,
freezes a finalized hash, reuses the existing `capture-window.sh` and
`fetch-witness.sh`, verifies every bundle, and publishes the 16-block corpus
only after a full-height canonical recheck. It fails closed when chain identity,
the witness capability, quorum, checksums, replay-to-corpus identity, or
approved replayer identity cannot be proved.

## Standard providers cannot replace a Reth witness source

Reth v2.1.0 is the oldest verified release whose by-hash witness RPC accepts
the optional mode argument. The `"canonical"` value selects witness
construction; it does not assert that the hash is still canonical. The client
must therefore pin and recheck the chain separately. Reth must expose the
`eth` and `debug` namespaces; `rpc_modules` can show a namespace but cannot
prove that the exact method and mode work.

| Source role | Required production capabilities | Suite behavior |
|---|---|---|
| `witness-primary` | `eth_*`, raw header/block, exact by-hash canonical witness | Probed before capture and preferred for debug reads |
| `witness-standby` | Same exact Reth capabilities as primary | Takes over bounded idempotent reads after classified failure |
| `canonical-aux` | Chain ID, genesis, syncing, finalized and numbered blocks | Contributes canonical quorum; never supplies a witness |

Chainstack explicitly disables both execution-witness methods. QuickNode's
published Ethereum Debug OpenRPC and Alchemy's Ethereum mainnet Debug API do
not list the required by-hash canonical method. A provider with a private
extension may be promoted only after the exact capability probe succeeds; a
generic `debug` namespace or standard `eth_getProof` is insufficient.

Official sources:

- [Reth v2.0.0 debug RPC before the mode argument](https://github.com/paradigmxyz/reth/blob/v2.0.0/crates/rpc/rpc-api/src/debug.rs)
- [Reth v2.1.0 debug RPC with the optional mode](https://github.com/paradigmxyz/reth/blob/v2.1.0/crates/rpc/rpc-api/src/debug.rs)
- [Commit introducing the witness mode](https://github.com/paradigmxyz/reth/commit/a05960ab07acae0d933a5a2cd69a11044719841a)
- [Reth v2.1.0 canonical witness mode](https://github.com/paradigmxyz/reth/blob/v2.1.0/crates/trie/common/src/execution_witness.rs)
- [Reth RPC namespace configuration](https://reth.rs/jsonrpc/intro/)
- [Reth pruning and the 10,064-block full-node history window](https://reth.rs/run/storage/pruning/)
- [Ethereum JSON-RPC methods and block tags](https://ethereum.org/developers/docs/apis/json-rpc/)
- [EIP-1898 canonical block identifiers](https://eips.ethereum.org/EIPS/eip-1898)
- [EIP-1186 `eth_getProof`](https://eips.ethereum.org/EIPS/eip-1186)
- [Chainstack disabled methods](https://docs.chainstack.com/docs/limits)
- [QuickNode Ethereum Debug OpenRPC](https://www.quicknode.com/docs/openrpc/ethereum-debug.json)
- [Alchemy Ethereum Debug API](https://www.alchemy.com/docs/chains/debug-api/debug-api-endpoints/debug-trace-block-by-hash)

## The gateway freezes identity before any bundle is published

The secret-free config names environment variables; it never contains endpoint
URLs or authentication headers. `readiness` performs:

1. `eth_chainId`.
2. `eth_getBlockByNumber("0x0", false)` and exact genesis-hash comparison.
3. `eth_syncing == false`.
4. an exact finalized number/hash quorum.
5. raw header, `debug_executionWitnessByBlockHash(hash, "canonical")`, and raw
   block probes on both Reth roles.

Each readiness or canonical-quorum endpoint gets a bounded same-endpoint retry
for 429, 5xx, timeout, or transport failure before its one vote is recorded.
A retry never counts as another vote.

During capture, a loopback gateway is reachable only through a per-process
high-entropy path. It returns the frozen finalized block, executes numbered
block lookups through quorum, and permits only the exact `eth_*` and `debug_*`
methods and parameter shapes required by the existing capture protocol. Debug
calls route only to configured Reth roles; witness requests must be exactly
`[hash, "canonical"]`, raw reads must require the canonical hash, and legacy or
all other calls are rejected. Each connection pool is bounded, each endpoint
has a token bucket, and identical immutable hash requests are coalesced and
stored in a checksummed, private resume cache. Numeric-height responses are
never cached, so the existing full-window recheck still observes a reorg.

`capture-window.sh` remains the capture protocol. It resolves one contiguous
window, enforces parent continuity, invokes one production witness fetch per
hash per whole-window attempt, verifies every raw block and witness, and
rechecks all 16 canonical hashes. The HA wrapper adds `BUNDLE_SHA256SUMS`,
`bundle-checksums.json`, metrics, a capability matrix, and a machine-readable
resume state before atomically renaming the final directory.
Publication uses Linux `renameat2(RENAME_NOREPLACE)`: a target created after
the earlier existence check is preserved and the workflow stops with its
private stage intact. The suite does not emulate no-overwrite with a racy
check followed by replacement.

## Classified failures either recover within bounds or stop the window

| Failure | Automatic action | Fail-closed condition |
|---|---|---|
| 429 | Honor bounded `Retry-After`, then exponential backoff with jitter and failover | Retry budget exhausted |
| 500–599, timeout, transport | Bounded backoff and endpoint rotation | No eligible endpoint succeeds |
| 401/403 | No retry storm | Stop as authentication failure |
| `-32601` | Mark exact method missing | Required witness quorum unavailable |
| `-32602` | Mark method/version incompatible | Required witness quorum unavailable |
| Malformed/oversized response | Reject response and try another eligible endpoint | No valid response |
| Chain/genesis mismatch or syncing | Remove endpoint from readiness | Required quorum unavailable |
| Finalized/hash drift or quorum disagreement | Do not select a winner | Stop the whole window |
| Bundle/checksum/verifier failure | Discard the private capture attempt | Bounded whole-window attempts exhausted |
| DTVM strict replay mismatch | Preserve capture, reject the evidence seal | Any required commitment is false |

`resume-state.json` is updated with write–fsync–rename and records only endpoint
labels, public configuration fingerprint, frozen hashes, phases, checksums and
failure categories. A restart revalidates chain/genesis and the frozen hash on
every available eligible source, then requires the configured quorums. The
persistent cache can reuse completed immutable block/witness responses; a
truncated or checksum-invalid cache entry is ignored and replaced atomically.
The final corpus is never assembled from an existing public partial output. If
the process stops after the final directory rename but before its state update,
resume accepts that directory only after the frozen pin, manifest contract and
every recorded checksum match the pre-rename state; published, replayed and
sealed states are revalidated on later capture invocations. Corpus roots,
manifests, checksum evidence, bundle path components and replay artifacts must
all be regular self-contained files; a symlink in the file or any lexical
ancestor is rejected.

One state-directory lock serializes capture, replay and seal. Repeating `seal`
does not rewrite the pre-seal state or timestamp: it revalidates every sealed
input, the original replayed pre-seal state, the sealed-state checksum and the
existing seal bytes, then returns the same document.

## Capacity trades storage and Reth IO for provider independence

The minimum production footprint is two synced Reth full nodes, but capture
cannot start if either is down. Adding one independent standard provider keeps
canonical quorum after a Reth failure that occurs after readiness. Starting or
resuming after any one Reth failure requires three Reth nodes; adding the
independent provider makes four sources. Run archive Reth when a resume may
exceed the documented 10,064-block full-node history window; otherwise full
nodes are sufficient for latest-finalized capture.

Witness generation is CPU, database-IO and response-size sensitive. Reth does
not publish a universal capacity number, so measure p95 witness latency and
response bytes on the target hardware before raising the default sequential
capture concurrency. Start with one 16-block workflow, an RPC connection pool
of four, eight requests per second and a burst of 16. Alert on readiness loss,
failover count, 429/5xx/timeout rates, quorum disagreement, cache corruption,
capture age and remaining disk.

Self-hosting adds node storage, snapshots, upgrades, monitoring and on-call
work. A managed standard provider reduces canonical-source operations but adds
request cost and an external rate limit; it still does not remove the two Reth
nodes. No provider purchase is required by the suite.

## Run readiness, frozen capture, offline replay and sealing separately

The `dtvm-run-reth-replay` directory in DTVMDotfiles is the sole versioned
source. Restore a working suite from any clean checkout before operating it:

```bash
SKILL_ROOT=/absolute/path/to/dtvm-run-reth-replay
TARGET=/private/path/witness-db-suite
bash "$SKILL_ROOT/scripts/restore-reth-replay-suite.sh" install "$TARGET"
bash "$SKILL_ROOT/scripts/restore-reth-replay-suite.sh" check "$TARGET"
bash "$SKILL_ROOT/scripts/verify-hermetic-suite.sh" "$TARGET"
```

The 30-file checksum manifest covers the HA client, validator, Python and shell
tests, capture/fetch/replay tools, Rust crate and tests, config/schema, and this
guide. Restore requires that exact unique path set, rejects source or target
ancestor symlinks before creating directories, and rejects unlisted files such
as Cargo `build.rs`. Keep Rust build output outside the restored tree. `sync`
updates an existing disposable adapter, and `check` proves byte and mode
equality without writing. Never treat an unversioned experiment directory as
source.

Copy `config/reth-rpc-ha.example.json` to a private operational location. Keep
only environment-variable names in that JSON. Export each URL and optional
header JSON in the named environment variable, then run:

Header names are normalized to lowercase. Case-insensitive duplicates,
framing/hop-by-hop names such as `Host`, `Content-Length`,
`Transfer-Encoding`, and `Connection`, and explicit Authorization combined
with URL userinfo are rejected. Endpoint-origin comparison canonicalizes
socket-compatible legacy numeric IPv4 spellings without resolving ordinary
DNS names.

```bash
PYTHONDONTWRITEBYTECODE=1 python3 reth_rpc_ha.py \
  --config /private/path/reth-rpc-ha.json readiness
```

Start or resume the latest finalized 16-block capture:

```bash
RETH_RPC_HA_CONFIG=/private/path/reth-rpc-ha.json \
CAPTURE_WINDOW_STATE_DIR=/private/path/finalized-16-state \
./../../evidence/dtvm-reth-16block-pr577-20260728/replay/resume-16block.sh
```

Fetch a transaction, full block or production witness without changing the
capture protocol:

```bash
python3 reth_rpc_ha.py --config /private/path/reth-rpc-ha.json \
  fetch --kind transaction --identifier 0xHASH --output /private/path/tx.json
python3 reth_rpc_ha.py --config /private/path/reth-rpc-ha.json \
  fetch --kind block --identifier 0xHASH --output /private/path/block.json
python3 reth_rpc_ha.py --config /private/path/reth-rpc-ha.json \
  fetch --kind witness --identifier 0xBLOCK_HASH --output /private/path/bundle.json
```

Stop all RPC gateway activity before strict replay. The `replay` subcommand
removes configured and conventional RPC/proxy variables from the child
environment, verifies the expected SHA-256 of both `verify-corpus.sh` and the
DTVM library, revalidates the capture-time approved replayer manifest and
binary bytes, and requires the result to prove every per-block differential and
commitment check. The result must repeat the capture manifest SHA-256 and block
count, bind every ordered block number/hash/bundle path/bundle SHA-256, and
repeat the approved replayer realpath and SHA-256. The replayer manifest is
mandatory for HA capture; its approved binary is included in the final
evidence seal. Replay must also state that network timing is excluded from the
performance conclusion. Run `seal` only after replay:

```bash
python3 reth_rpc_ha.py --config /private/path/reth-rpc-ha.json replay \
  --state-dir /private/path/finalized-16-state \
  --verify-corpus-script /absolute/path/verify-corpus.sh \
  --verify-corpus-sha256 EXPECTED_SCRIPT_SHA256 \
  --dtvm-library /absolute/path/libdtvmapi.so \
  --dtvm-library-sha256 EXPECTED_LIBRARY_SHA256 \
  --replay-output /private/path/finalized-16-replay \
  --label candidate
python3 reth_rpc_ha.py --config /private/path/reth-rpc-ha.json seal \
  --state-dir /private/path/finalized-16-state
```

If readiness is unavailable, supply two trusted Reth endpoints with the exact
canonical witness extension. Do not substitute a public standard RPC, buy a
service automatically, place a credential in the config, or use network wall
time as DTVM performance evidence.
