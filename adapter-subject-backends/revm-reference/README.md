# Independent REVM Osaka reference

This package runs one standalone REVM baseline for the
`STORAGE_LOG_RETURN` fixture copied from `../tests/real_dtvm.rs`.

It uses the standard `alloy_evm::eth::EthEvmFactory`, REVM 41.0.0, and
`SpecId::OSAKA`. The transaction has empty calldata and value, uses sender
`0x2222222222222222222222222222222222222222`, calls
`0x1111111111111111111111111111111111111111`, and starts recipient slot 0 at
zero. Its gas limit is 1,021,000: subtracting the 21,000 intrinsic gas leaves
exactly 1,000,000 gas for the first frame.

This is only an independent REVM baseline:

- It does not load or execute DTVM.
- It does not read DTVM results.
- It does not claim a differential PASS.
- It does not establish block correctness.

`AccessListInspector` is used only for its documented EIP-2930 access-list
candidate set. Its output is not a complete ordered witness-access audit; that
stronger access claim is explicitly marked blocked in the JSON.

Run the fixed, offline verifier:

```sh
./verify.sh
```

