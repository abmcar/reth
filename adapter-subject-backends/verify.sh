#!/usr/bin/env bash
set -euo pipefail

# Configure via environment. DTVM_LIBRARY / DTVM_LIBRARY_SHA256 / EVMC_INCLUDE
# are required; the rest default to sensible in-tree locations.
adapter_root="${ADAPTER_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
build_root="${ADAPTER_BUILD_DIR:-${adapter_root}/build}"
dtvm_library="${DTVM_LIBRARY:?set DTVM_LIBRARY to the EVMC shared library}"
dtvm_sha256="${DTVM_LIBRARY_SHA256:?set DTVM_LIBRARY_SHA256 to its sha256}"
evmc_include="${EVMC_INCLUDE:?set EVMC_INCLUDE to the EVMC headers directory}"

export CARGO_TARGET_DIR="${build_root}/cargo-target"
export DTVM_REQUIRED=1
export DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true
export DTVM_LIBRARY="${dtvm_library}"
export DTVM_LIBRARY_SHA256="${dtvm_sha256}"

mkdir -p "${build_root}"

actual_dtvm_sha256="$(sha256sum "${dtvm_library}" | cut -d' ' -f1)"
test "${actual_dtvm_sha256}" = "${dtvm_sha256}"

cc -std=c11 -Wall -Wextra -Werror \
  -I"${evmc_include}" \
  "${adapter_root}/tests/abi_probe.c" \
  -o "${build_root}/abi_probe"

actual_abi="$("${build_root}/abi_probe")"
expected_abi='size.evmc_address=20
size.evmc_bytes32=32
size.struct evmc_message=184
size.struct evmc_tx_context=256
size.struct evmc_tx_initcode=48
size.struct evmc_result=72
size.struct evmc_host_interface=128
size.struct evmc_vm=56
offset.struct evmc_message.gas=16
offset.struct evmc_message.input_data=64
offset.struct evmc_message.value=80
offset.struct evmc_message.code_address=144
offset.struct evmc_message.code=168
offset.struct evmc_message.code_size=176
offset.struct evmc_tx_context.blob_hashes=224
offset.struct evmc_tx_context.initcodes=240
offset.struct evmc_result.gas_left=8
offset.struct evmc_result.gas_refund=16
offset.struct evmc_result.release=40
offset.struct evmc_result.create_address=48
offset.struct evmc_host_interface.set_transient_storage=120
offset.struct evmc_vm.execute=32
offset.struct evmc_vm.set_option=48'
test "${actual_abi}" = "${expected_abi}"

cd "${adapter_root}"
"${CARGO_HOME}/bin/cargo" fmt -- --check
"${CARGO_HOME}/bin/cargo" check --locked --offline --all-targets
"${CARGO_HOME}/bin/cargo" test --locked --offline -- --nocapture

sha256sum \
  Cargo.toml Cargo.lock src/*.rs tests/*.rs tests/*.c \
  "${dtvm_library}" \
  "${build_root}/abi_probe"
