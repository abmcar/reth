#!/usr/bin/env bash
set -euo pipefail

crate_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
experiment_root="$(cd -- "${crate_root}/../.." && pwd -P)"
toolchain_root="${experiment_root}/build/toolchain"
default_target_root="${experiment_root}/build/witness-db-target"

export CARGO_HOME="${CARGO_HOME:-${toolchain_root}/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-${toolchain_root}/rustup}"
export PATH="${CARGO_HOME}/bin:${PATH}"
export CARGO_NET_OFFLINE="${CARGO_NET_OFFLINE:-true}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${default_target_root}}"
export DTVM_REQUIRED=1
export DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true
export DTVM_LIBRARY="${DTVM_LIBRARY:?DTVM_LIBRARY is required}"
export DTVM_LIBRARY_SHA256="${DTVM_LIBRARY_SHA256:?DTVM_LIBRARY_SHA256 is required}"
export DTVM_REPLAY_STATE_VALIDATOR="${DTVM_REPLAY_STATE_VALIDATOR:-${crate_root}/tools/validate-replay-state.py}"
export PYTHONDONTWRITEBYTECODE=1

test -f "${DTVM_REPLAY_STATE_VALIDATOR}"
test "$(sha256sum "${DTVM_LIBRARY}" | awk '{print $1}')" = \
    "${DTVM_LIBRARY_SHA256}"
bash -n "${crate_root}/fetch-witness.sh"
bash -n "${crate_root}/capture-window.sh"
bash -n "${crate_root}/replay-tip.sh"
bash -n "${crate_root}/tests/capture-window-fake-curl.sh"
bash -n "${crate_root}/tests/capture-window-fake-fetch.sh"
bash -n "${crate_root}/tests/capture-window-fake-verify.sh"
bash -n "${crate_root}/tests/capture-window.sh"
bash -n "${crate_root}/tests/fake-curl.sh"
bash -n "${crate_root}/tests/fetch-witness.sh"
bash -n "${crate_root}/tests/replay-tip-fake-curl.sh"
bash -n "${crate_root}/tests/replay-tip-fake-replay-block.sh"
bash -n "${crate_root}/tests/replay-tip.sh"
python3 "${crate_root}/tests/reth_rpc_ha_test.py"
bash "${crate_root}/tests/fetch-witness.sh"
bash "${crate_root}/tests/capture-window.sh"
bash "${crate_root}/tests/replay-tip.sh"
cargo fmt --manifest-path "${crate_root}/Cargo.toml" -- --check
cargo check --manifest-path "${crate_root}/Cargo.toml" --locked --offline --all-targets
cargo test --manifest-path "${crate_root}/Cargo.toml" --locked --offline
