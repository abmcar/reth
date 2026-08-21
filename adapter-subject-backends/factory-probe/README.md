# Reth factory compile probe — not DTVM execution

This is a compile-only API probe against the pinned Reth checkout at
`../../src/reth`. It proves that the current `alloy_evm::EvmFactory` GAT shape
can be supplied to `EthEvmConfig::new_with_evm_factory`.

It does **not** execute DTVM. `DtvmEvm` delegates every execution method to the
standard `alloy_evm::eth::EthEvm`. Passing these tests is not transaction,
block, witness, state-root, gas, log, EIP-7702, system-call, or DTVM correctness
evidence.

The probe intentionally compiles both the no-inspector and generic-inspector
factory shapes. It makes no claim that an EVMC backend can preserve Reth
inspector callbacks.

Pinned inputs:

- Reth commit: `1c2942abc6d3b78a7656acdaa985bdac03408a26`
- Reth tree: `15168a4a1c04e27a93b7a86baadd3f30722514fe`
- Reth `Cargo.lock` SHA-256:
  `9dd1ae47a32ef0c8d789f294115cee44395ca39e2989d8b6cd07a0b25257064d`
- `alloy-evm`: `0.37.1`
- `revm`: `41.0.0`
- Rust/Cargo: `1.95.0`

Run the reproducible verifier from this directory. It selects the
experiment-local toolchain and target directory, requires the pinned Reth
commit/tree and lock hash, checks that this probe's registry lock entries are
an exact subset of Reth's lock entries, and then runs the compile checks and
tests offline:

```sh
./verify.sh
```

The verifier does not load `libdtvmapi.so` or execute DTVM.
