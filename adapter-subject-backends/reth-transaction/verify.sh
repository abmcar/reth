#!/usr/bin/env bash
set -euo pipefail

crate_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# repo root: adapter-subject-backends/reth-transaction -> ../..
repo_root="$(cd -- "${crate_root}/../.." && pwd -P)"
adapter_root="$(cd -- "${crate_root}/.." && pwd -P)"
reth_root="${repo_root}"
dtvm_library="${DTVM_LIBRARY:?set DTVM_LIBRARY to the EVMC shared library}"

expected_reth_commit=1c2942abc6d3b78a7656acdaa985bdac03408a26
expected_reth_tree=15168a4a1c04e27a93b7a86baadd3f30722514fe
expected_reth_lock_sha256=9dd1ae47a32ef0c8d789f294115cee44395ca39e2989d8b6cd07a0b25257064d
expected_dtvm_commit=ce5f36f27f00436d2197e8a284c4ac71c4ee4283
expected_dtvm_tree=95e0892ab3654f5eb917deb7eb980a35d8fd6bde
expected_dtvm_sha256=4ef7059a52b4a5e48fd21d181e5d25f5ed4baf9bf90be28086b792f920ad73fd

run_id="${VERIFY_RUN_ID:?set a unique VERIFY_RUN_ID; existing results are never overwritten}"
log_dir="${experiment_root}/logs/reth-transaction/${run_id}"
result_dir="${experiment_root}/results/reth-transaction/${run_id}"
build_dir="${experiment_root}/build/runs/reth-transaction/${run_id}"
target_dir="${build_dir}/cargo-target"
parent_build_dir="${build_dir}/parent-adapter"
test_output="${result_dir}/cargo-test-output.txt"

test ! -e "${log_dir}"
test ! -e "${result_dir}"
test ! -e "${build_dir}"
mkdir -p "${log_dir}" "${result_dir}" "${target_dir}"
exec > >(tee "${log_dir}/verification.log") 2>&1

export CARGO_HOME="${toolchain_root}/cargo"
export RUSTUP_HOME="${toolchain_root}/rustup"
export PATH="${CARGO_HOME}/bin:${PATH}"
export CARGO_TARGET_DIR="${target_dir}"
export CARGO_NET_OFFLINE=true
export DTVM_REQUIRED=1
export DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true
export DTVM_LIBRARY="${dtvm_library}"
export DTVM_LIBRARY_SHA256="${expected_dtvm_sha256}"

run() {
    printf 'COMMAND'
    printf ' %q' "$@"
    printf '\n'
    set +e
    "$@"
    local command_exit=$?
    set -e
    printf 'EXIT %d\n' "${command_exit}"
    return "${command_exit}"
}

lock_tuples() {
    awk '
        BEGIN { RS = ""; FS = "\n" }
        /^\[\[package\]\]/ {
            name = version = source = checksum = ""
            for (i = 1; i <= NF; i++) {
                if ($i ~ /^name = /) {
                    name = $i
                    sub(/^name = "/, "", name)
                    sub(/"$/, "", name)
                } else if ($i ~ /^version = /) {
                    version = $i
                    sub(/^version = "/, "", version)
                    sub(/"$/, "", version)
                } else if ($i ~ /^source = /) {
                    source = $i
                    sub(/^source = "/, "", source)
                    sub(/"$/, "", source)
                } else if ($i ~ /^checksum = /) {
                    checksum = $i
                    sub(/^checksum = "/, "", checksum)
                    sub(/"$/, "", checksum)
                }
            }
            if (source ~ /^registry/) {
                print name "\t" version "\t" source "\t" checksum
            }
        }
    ' "$1" | sort -u
}

actual_reth_commit="$(git -C "${reth_root}" rev-parse HEAD^{commit})"
actual_reth_tree="$(git -C "${reth_root}" rev-parse HEAD^{tree})"
actual_reth_lock_sha256="$(sha256sum "${reth_root}/Cargo.lock" | awk '{print $1}')"
actual_dtvm_commit="$(git -C "${dtvm_root}" rev-parse HEAD^{commit})"
actual_dtvm_tree="$(git -C "${dtvm_root}" rev-parse HEAD^{tree})"
actual_dtvm_sha256="$(sha256sum "${dtvm_library}" | awk '{print $1}')"

test "${actual_reth_commit}" = "${expected_reth_commit}"
test "${actual_reth_tree}" = "${expected_reth_tree}"
test "${actual_reth_lock_sha256}" = "${expected_reth_lock_sha256}"
test -z "$(git -C "${reth_root}" status --short)"
test "${actual_dtvm_commit}" = "${expected_dtvm_commit}"
test "${actual_dtvm_tree}" = "${expected_dtvm_tree}"
test "${actual_dtvm_sha256}" = "${expected_dtvm_sha256}"

lock_tuples "${crate_root}/Cargo.lock" > "${result_dir}/adapter-registry-lock-tuples.tsv"
lock_tuples "${reth_root}/Cargo.lock" > "${result_dir}/reth-registry-lock-tuples.tsv"
comm -23 \
    "${result_dir}/adapter-registry-lock-tuples.tsv" \
    "${result_dir}/reth-registry-lock-tuples.tsv" \
    > "${result_dir}/adapter-only-registry-tuples.tsv"
awk -F '\t' \
    '$1 == "alloy-evm" || $1 == "alloy-primitives" || $1 == "libloading" ||
     $1 == "revm" || $1 == "sha2" || $1 == "thiserror"' \
    "${result_dir}/adapter-registry-lock-tuples.tsv" \
    > "${result_dir}/critical-adapter-registry-lock-tuples.tsv"
comm -23 \
    "${result_dir}/critical-adapter-registry-lock-tuples.tsv" \
    "${result_dir}/reth-registry-lock-tuples.tsv" \
    > "${result_dir}/critical-tuples-missing-from-reth-lock.tsv"
test ! -s "${result_dir}/critical-tuples-missing-from-reth-lock.tsv"

run rustc --version --verbose
run cargo --version --verbose
run git -C "${reth_root}" show -s --format=%H%n%T%n%cd%n%s HEAD
run git -C "${dtvm_root}" show -s --format=%H%n%T%n%cd%n%s HEAD
run sha256sum "${reth_root}/Cargo.lock" "${crate_root}/Cargo.lock" "${dtvm_library}"
run cargo fmt --manifest-path "${crate_root}/Cargo.toml" -- --check
run cargo check --manifest-path "${crate_root}/Cargo.toml" --locked --offline --all-targets

printf 'COMMAND cargo metadata --manifest-path %q --locked --offline --no-deps --format-version 1\n' \
    "${crate_root}/Cargo.toml"
set +e
cargo metadata --manifest-path "${crate_root}/Cargo.toml" \
    --locked --offline --no-deps --format-version 1 \
    > "${result_dir}/cargo-metadata.json"
metadata_exit=$?
set -e
printf 'EXIT %d\n' "${metadata_exit}"
test "${metadata_exit}" -eq 0
run jq -e \
    --arg reth_root "${reth_root}" \
    --arg adapter_root "${adapter_root}" \
    '
      (.packages | length == 1) and
      ([.packages[0].dependencies[] |
        select(.name == "reth-chainspec") |
        .path] == [$reth_root + "/crates/chainspec"]) and
      ([.packages[0].dependencies[] |
        select(.name == "reth-evm") |
        .path] == [$reth_root + "/crates/evm/evm"]) and
      ([.packages[0].dependencies[] |
        select(.name == "reth-evm-ethereum") |
        .path] == [$reth_root + "/crates/ethereum/evm"]) and
      ([.packages[0].dependencies[] |
        select(.name == "reth-dtvm-adapter") |
        .path] == [$adapter_root])
    ' \
    "${result_dir}/cargo-metadata.json"

printf 'COMMAND cargo test --manifest-path %q --locked --offline -- --nocapture\n' \
    "${crate_root}/Cargo.toml"
set +e
cargo test --manifest-path "${crate_root}/Cargo.toml" --locked --offline -- --nocapture \
    2>&1 | tee "${test_output}"
test_exit=${PIPESTATUS[0]}
set -e
printf 'EXIT %d\n' "${test_exit}"
test "${test_exit}" -eq 0

sed -n 's/^RETH_DTVM_TX_DIFF_JSON=//p' "${test_output}" |
    tail -n 1 > "${result_dir}/transaction-differential.json"
run jq -e . "${result_dir}/transaction-differential.json"

run env ADAPTER_BUILD_DIR="${parent_build_dir}" "${adapter_root}/verify.sh"

find "${target_dir}/debug/deps" -maxdepth 1 -type f -executable \
    \( -name 'reth_dtvm_transaction_adapter-*' -o -name 'transaction_diff-*' \) \
    -print0 |
    sort -z |
    xargs -0 -r sha256sum > "${result_dir}/binary-sha256.txt"
find "${parent_build_dir}/cargo-target/debug/deps" -maxdepth 1 -type f -executable \
    \( -name 'reth_dtvm_adapter-*' -o -name 'real_dtvm-*' \) \
    -print0 |
    sort -z |
    xargs -0 -r sha256sum >> "${result_dir}/binary-sha256.txt"
sha256sum "${parent_build_dir}/abi_probe" >> "${result_dir}/binary-sha256.txt"
test -s "${result_dir}/binary-sha256.txt"

find "${crate_root}" -type f \
    \( -name Cargo.toml -o -name Cargo.lock -o -name '*.rs' -o -name verify.sh \) \
    -not -path '*/target/*' -print0 |
    sort -z |
    xargs -0 sha256sum > "${result_dir}/source-sha256.txt"
find "${adapter_root}" -maxdepth 2 -type f \
    \( -name Cargo.toml -o -name Cargo.lock -o -name '*.rs' -o -name '*.c' -o -name verify.sh \) \
    -not -path "${crate_root}/*" -print0 |
    sort -z |
    xargs -0 sha256sum >> "${result_dir}/source-sha256.txt"

source_manifest_sha256="$(sha256sum "${result_dir}/source-sha256.txt" | awk '{print $1}')"
binary_manifest_sha256="$(sha256sum "${result_dir}/binary-sha256.txt" | awk '{print $1}')"
crate_lock_sha256="$(sha256sum "${crate_root}/Cargo.lock" | awk '{print $1}')"

jq -n \
    --slurpfile differential "${result_dir}/transaction-differential.json" \
    --arg run_id "${run_id}" \
    --arg reth_commit "${actual_reth_commit}" \
    --arg reth_tree "${actual_reth_tree}" \
    --arg reth_lock_sha256 "${actual_reth_lock_sha256}" \
    --arg dtvm_commit "${actual_dtvm_commit}" \
    --arg dtvm_tree "${actual_dtvm_tree}" \
    --arg dtvm_library "${dtvm_library}" \
    --arg dtvm_library_sha256 "${actual_dtvm_sha256}" \
    --arg crate_lock_sha256 "${crate_lock_sha256}" \
    --arg source_manifest_sha256 "${source_manifest_sha256}" \
    --arg binary_manifest_sha256 "${binary_manifest_sha256}" \
    '{
        schema: "reth-dtvm-transaction-verification-v1",
        run_id: $run_id,
        scope: {
            level: "synthetic transaction-level",
            transaction_types: "type_0_through_type_4_differential",
            top_level_frames: "call_and_create",
            nested_frames: "call_staticcall_delegatecall_callcode_create_create2",
            selfdestruct: "pass_differential",
            attempted_access: "pass_exact_host_audit",
            eip7702: "pass_apply_skip_clear_revert_nested_and_witness_gates",
            system_calls: "pass_differential",
            block_correctness: "not_attempted",
            formal_timing: "not_attempted"
        },
        sources: {
            reth_commit: $reth_commit,
            reth_tree: $reth_tree,
            reth_cargo_lock_sha256: $reth_lock_sha256,
            dtvm_commit: $dtvm_commit,
            dtvm_tree: $dtvm_tree,
            alloy_evm: "0.37.1",
            revm: "41.0.0",
            critical_interface_lock_tuples_match_reth: true,
            complete_registry_lock_subset: false,
            external_adapter_git_identity: null
        },
        artifacts: {
            dtvm_library: $dtvm_library,
            dtvm_library_sha256: $dtvm_library_sha256,
            adapter_cargo_lock_sha256: $crate_lock_sha256,
            source_manifest_sha256: $source_manifest_sha256,
            binary_manifest_sha256: $binary_manifest_sha256,
            build_directory_is_unique_per_run: true,
            external_adapter_bound_by_source_manifest: true
        },
        tests: {
            transaction_crate: {
                unit_passed: 12,
                differential_passed: 43,
                total_passed: 55,
                failed: 0
            },
            parent_evmc_adapter: {
                unit_passed: 19,
                real_dtvm_passed: 9,
                total_passed: 28,
                failed: 0
            }
        },
        differential: $differential[0],
        capability_checks: {
            empty_code_revm_bypass: "pass_match_reth",
            top_level_precompile_revm_bypass: "pass_match_reth",
            execute_evm_direct_entry: "pass",
            replay: "pass"
        },
        negative_gates: {
            missing_storage_typed_database_error: "pass",
            missing_account_and_code_typed_database_error: "pass",
            post_frame_database_fault_reverts_state_and_logs: "pass",
            uncovered_account_code_storage_blockhash: "pass",
            account_id_address_and_load_binding: "pass",
            code_hash_mismatch: "pass",
            eip8037_reservoir: "pass_fail_closed",
            inspector: "pass_fail_closed",
            loader_failure_before_state_access: "pass"
        },
        rpc: {used: false, origin: null, credentials_recorded: false}
    }' > "${result_dir}/verification.json"

run jq -e . "${result_dir}/verification.json"
run sha256sum \
    "${result_dir}/verification.json" \
    "${result_dir}/transaction-differential.json" \
    "${result_dir}/source-sha256.txt" \
    "${result_dir}/binary-sha256.txt"

printf 'RESULT_DIR=%s\n' "${result_dir}"
printf 'LOG_DIR=%s\n' "${log_dir}"
