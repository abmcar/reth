#!/usr/bin/env bash
set -euo pipefail

crate_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
temporary_root="$(mktemp -d)"
trap 'rm -rf -- "${temporary_root}"' EXIT

fake_bin="${temporary_root}/bin"
mkdir -- "${fake_bin}"
cp -- "${crate_root}/tests/fake-curl.sh" "${fake_bin}/curl"
chmod +x "${fake_bin}/curl"

block_hash="0x$(printf '11%.0s' {1..32})"
rpc_url="http://network-must-not-be-used.invalid"

run_fetch() {
    local scenario="$1"
    local output="$2"
    local state="$3"
    local policy="${4:-production}"
    printf '%s\n' 0 >"${state}"
    PATH="${fake_bin}:${PATH}" \
    FAKE_BLOCK_HASH="${block_hash}" \
    FAKE_CURL_STATE="${state}" \
    FAKE_SCENARIO="${scenario}" \
        bash "${crate_root}/fetch-witness.sh" \
            --policy "${policy}" \
            "${rpc_url}" \
            "${block_hash}" \
            "${output}"
}

expect_fetch_failure() {
    local scenario="$1"
    local output="$2"
    local expected_calls="$3"
    local policy="${4:-production}"
    local state="${temporary_root}/${scenario}.state"
    local error="${temporary_root}/${scenario}.stderr"
    if run_fetch "${scenario}" "${output}" "${state}" "${policy}" \
        >"${temporary_root}/${scenario}.stdout" 2>"${error}"; then
        echo "fetch-witness unexpectedly accepted ${scenario}" >&2
        exit 1
    fi
    if [[ -e "${output}" ]]; then
        echo "fetch-witness left output after ${scenario} failed" >&2
        exit 1
    fi
    if [[ "$(cat "${state}")" != "${expected_calls}" ]]; then
        echo "${scenario} made an unexpected number of RPC calls" >&2
        cat "${error}" >&2
        exit 1
    fi
}

current_output="${temporary_root}/current.json"
current_state="${temporary_root}/current.state"
current_status="$(
    run_fetch "current" "${current_output}" "${current_state}" production
)"
jq -e \
    --arg hash "${block_hash}" \
    '
        .status == "captured" and
        .blockHash == $hash and
        .witnessMethod == "debug_executionWitnessByBlockHash" and
        .witnessMode == "canonical" and
        .witnessPolicy == "production" and
        .usedFallback == false
    ' <<<"${current_status}" >/dev/null
jq -e \
    --arg hash "${block_hash}" \
    '
        .targetHeader == "0xc0" and
        .targetBlockHash == $hash and
        .targetBlock == "0xc0" and
        .witness == {
            state: ["0xc0"],
            codes: ["0x"],
            keys: [],
            headers: ["0xc1", "0xc2"]
        }
    ' "${current_output}" >/dev/null
test "$(cat "${current_state}")" = 4
if grep -F "${rpc_url}" "${current_output}" >/dev/null; then
    echo "fetch-witness persisted the RPC URL" >&2
    exit 1
fi

fallback_output="${temporary_root}/fallback.json"
fallback_state="${temporary_root}/fallback.state"
fallback_status="$(
    run_fetch \
        "fallback_success" \
        "${fallback_output}" \
        "${fallback_state}" \
        best-effort
)"
jq -e \
    --arg hash "${block_hash}" \
    '
        .status == "captured" and
        .blockHash == $hash and
        .witnessMethod == "debug_executionWitness" and
        .witnessMode == "canonical" and
        .witnessPolicy == "best-effort" and
        .usedFallback == true
    ' <<<"${fallback_status}" >/dev/null
jq -e \
    --arg hash "${block_hash}" \
    '
        .targetHeader == "0xc0" and
        .targetBlockHash == $hash and
        .targetBlock == "0xc0" and
        .witness == {
            state: ["0xc0"],
            codes: [],
            keys: [],
            headers: ["0xc2", "0xc3"]
        }
    ' "${fallback_output}" >/dev/null
test "$(cat "${fallback_state}")" = 8
if grep -F "${rpc_url}" "${fallback_output}" >/dev/null; then
    echo "fallback fetch persisted the RPC URL" >&2
    exit 1
fi

expect_fetch_failure \
    "fallback_success" \
    "${temporary_root}/production-missing.json" \
    3 \
    production

overwrite_state="${temporary_root}/overwrite.state"
printf '%s\n' 0 >"${overwrite_state}"
if PATH="${fake_bin}:${PATH}" \
    FAKE_BLOCK_HASH="${block_hash}" \
    FAKE_CURL_STATE="${overwrite_state}" \
    FAKE_SCENARIO="current" \
        bash "${crate_root}/fetch-witness.sh" \
            "${rpc_url}" \
            "${block_hash}" \
            "${current_output}" >/dev/null 2>&1; then
    echo "fetch-witness unexpectedly overwrote an existing bundle" >&2
    exit 1
fi
test "$(cat "${overwrite_state}")" = 0

expect_fetch_failure \
    "odd_raw" \
    "${temporary_root}/odd-raw.json" \
    4
expect_fetch_failure \
    "fallback_wrong_recheck_hash" \
    "${temporary_root}/wrong-recheck.json" \
    5 \
    best-effort
expect_fetch_failure \
    "noncompat_error" \
    "${temporary_root}/noncompat.json" \
    3
expect_fetch_failure \
    "fallback_malformed_headers" \
    "${temporary_root}/malformed-headers.json" \
    5 \
    best-effort
