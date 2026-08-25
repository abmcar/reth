# EVMC bridge adapter

This document describes `adapter-subject-backends/`, the layer that lets an
EVMC-compatible shared library act as reth's execution engine. It is separate
from [`mainnet-replay.md`](./mainnet-replay.md), which covers replay operation
and does not require any of this.

You only need this if you are running an EVM that ships as a C ABI shared
library. Engines written in Rust — REVM, and revmc — plug into reth directly and
do not cross this boundary.

---

## 1. Why a bridge exists at all

reth's execution stack is Rust: the EVM receives state through revm's `Database`
trait, ordinary monomorphised calls with no marshalling.

An EVMC engine is a shared library with a C ABI. Something has to sit between the
two: expose reth's state to the library through EVMC's host-callback interface,
translate values across the boundary, and map the library's frame results back
into revm types. That layer is what this directory contains.

The cost of the boundary is not the same as the cost of the engine behind it.
Anything measured through this adapter includes both, so it is worth accounting
for the two separately when interpreting numbers.

---

## 2. Layout

```
adapter-subject-backends/
├── src/
│   ├── ffi.rs            EVMC ABI 12 type definitions and C-side plumbing
│   ├── host.rs           host callbacks: account, storage, code, logs, nested calls
│   ├── vm.rs             library loading, VM lifetime, backend selection
│   └── lib.rs
├── reth-transaction/     reth integration
│   ├── reth_evm.rs       implements alloy-evm `EvmFactory` / `Evm` for reth
│   ├── journal_host.rs   HostBackend backed by revm's journaled state
│   ├── strict_db.rs      Database impl that records the access sequence
│   └── lib.rs
├── witness-db/           standalone witness-driven replay tool — see docs/witness-replay.md
├── revm-reference/       reference execution path
├── factory-probe/        checks the factory satisfies reth's trait bounds
└── verify.sh
```

`patched/revm-handler` is a path dependency of the adapter core and must be
present for it to build.

### Frame coverage

From the adapter core's own documentation:

> The core executes Osaka top-level CALL and CREATE-initcode frames, including
> delegated-code CALLs. Its client backend owns state and child-frame lifecycle
> semantics; the Reth backend implements CALL, STATICCALL, DELEGATECALL,
> CALLCODE, CREATE, CREATE2, and SELFDESTRUCT. EOFCREATE, unknown revisions or
> flags, and capabilities omitted by another backend remain explicitly
> fail-closed.

Fail-closed means an unsupported case raises an error rather than falling back to
a different execution path.

---

## 3. How it attaches to reth

`reth_evm.rs` implements alloy-evm's `EvmFactory` for the EVMC-backed engine.
That is the same extension point reth uses for its own engines, so a node can be
built with this factory in place of the default one — which is what
`bin/reth-dtvm/` does.

Two consequences worth knowing:

- The factory must satisfy reth's trait bounds, including `Send + Sync + Unpin`,
  even though the VM handle behind it is not itself thread-portable. The factory
  keeps VM instances in thread-local storage and hands out per-thread handles;
  `factory-probe/` exists to check the bounds still hold at compile time.

- reth constructs an EVM per block. A factory that loads the shared library on
  every construction pays that cost per block; one that caches pays it once per
  thread. `vm_create_count()` on the factory reports how many VM instances were
  actually created, which is the direct way to tell which is happening.

---

## 4. Selecting a backend

Three environment variables, all required:

| Variable | Values |
|---|---|
| `RETH_SUBJECT_BACKEND` | `dtvm-eager`, `dtvm-profile-guided`, `evmone-advanced` |
| `RETH_SUBJECT_LIBRARY` | path to the EVMC shared library |
| `RETH_SUBJECT_LIBRARY_SHA256` | expected SHA-256 of that file |

The hash is verified before the library is loaded and startup is refused on
mismatch. `DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true` is additionally required
by the DTVM backends.

---

## 5. Access-sequence recording

`strict_db.rs` implements revm's `Database` and records every access as a typed
event — `Basic`, `Code`, `Storage`, `StorageByAccountId`, `BlockHash` — in order.

Because two engines executing the same block correctly must issue the same
sequence of state reads, comparing recorded sequences detects divergence that
gas and state-root checks alone can miss, and localises it to a specific access
rather than to the block as a whole.

One legitimate exception is documented in the code: withdrawal balance credits at
the end of a block apply to a *set* of recipients, and their relative order can
differ between engines without any consensus-relevant divergence.

---

## 6. Cost characteristics of the boundary

If you are measuring an engine through this adapter, the boundary contributes in
two distinct ways, and they behave differently:

- **Per-crossing cost** — argument marshalling, the C call itself, error-path
  setup. Roughly constant per crossing, so its total scales with crossing count.
  Crossing counts are workload-dependent and can reach tens of thousands per
  mainnet block.

- **Work performed inside a callback** — whatever the host does before returning.
  This is not bounded by the crossing count and can dominate it. Code-related
  callbacks (`get_code_size`, `copy_code`, `get_code_hash`) are the ones to look
  at first, since contract bytecode is large and these callbacks fire roughly
  once per nested call.

  The adapter validates `keccak256(code) == code_hash` before handing code
  across the boundary. Code is handled as refcounted `Bytes` (no copy per
  callback), and the validation result is memoized per thread, keyed by account
  address and invalidated on any change of hash, pointer, or length — so the
  keccak recompute happens once per contract rather than once per callback. An
  earlier revision of this adapter recomputed the hash over a fresh copy of the
  full bytecode on every code-related callback; that cost scales with
  bytecode size × callback count and is attributable to the boundary, not the
  engine. If you are comparing numbers produced by different adapter revisions,
  check which behavior was in place.

Both are attributable: instrumenting the adapter to count crossings by kind, and
microbenchmarking a single crossing in isolation, together bound how much of an
engine's measured time is the boundary rather than the engine.

---

## 7. Building

The adapter crates are excluded from the reth workspace (`exclude` in the root
`Cargo.toml`) because each is its own workspace root. They are consumed by path
dependency, so `cargo build --release --bin reth-dtvm` from the repository root
builds them as part of that binary's dependency graph.

`adapter-subject-backends/verify.sh` runs the adapter's own checks.
