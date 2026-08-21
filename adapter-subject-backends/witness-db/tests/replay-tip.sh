#!/usr/bin/env bash
set -euo pipefail

crate_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
temporary_root="$(mktemp -d)"
trap 'rm -rf -- "${temporary_root}"' EXIT

fake_bin="${temporary_root}/bin"
mkdir -- "${fake_bin}"
cp -- "${crate_root}/tests/replay-tip-fake-curl.sh" "${fake_bin}/curl"
chmod +x "${fake_bin}/curl"
fake_replay="${crate_root}/tests/replay-tip-fake-replay-block.sh"
rpc_user="fixture-user"
rpc_password="fixture-password"
rpc_authority="${rpc_user}:${rpc_password}"
rpc_url="http://${rpc_authority}@network-must-not-be-used.invalid"
hash_a="0x$(printf '11%.0s' {1..32})"
hash_b="0x$(printf '22%.0s' {1..32})"
scenario_count=0

last_stdout=""
last_stderr=""
last_state=""
last_log=""
last_replay_log=""
last_output=""
last_status=0

run_tip() {
    local scenario="$1"
    local tag="$2"
    local output_name="$3"
    local max_attempts="${4:-3}"
    last_stdout="${temporary_root}/${scenario}.stdout"
    last_stderr="${temporary_root}/${scenario}.stderr"
    last_state="${temporary_root}/${scenario}.state"
    last_log="${temporary_root}/${scenario}.requests.jsonl"
    last_replay_log="${temporary_root}/${scenario}.replay.log"
    last_output="${temporary_root}/${output_name}"
    printf '%s\n' 0 >"${last_state}"
    : >"${last_log}"
    : >"${last_replay_log}"

    if PATH="${fake_bin}:${PATH}" \
        FAKE_SCENARIO="${scenario}" \
        FAKE_TIP_STATE="${last_state}" \
        FAKE_TIP_LOG="${last_log}" \
        FAKE_REPLAY_LOG="${last_replay_log}" \
        REPLAY_TIP_REPLAY_BLOCK="${fake_replay}" \
        bash "${crate_root}/replay-tip.sh" \
            --tag "${tag}" \
            --max-attempts "${max_attempts}" \
            --output "${last_output}" \
            "${rpc_url}" >"${last_stdout}" 2>"${last_stderr}"; then
        last_status=0
    else
        last_status="$?"
    fi
}

run_diagnostic() {
    local scenario="$1"
    last_stdout="${temporary_root}/${scenario}.stdout"
    last_stderr="${temporary_root}/${scenario}.stderr"
    last_state="${temporary_root}/${scenario}.state"
    last_log="${temporary_root}/${scenario}.requests.jsonl"
    last_replay_log="${temporary_root}/${scenario}.replay.log"
    last_output=""
    printf '%s\n' 0 >"${last_state}"
    : >"${last_log}"
    : >"${last_replay_log}"

    if PATH="${fake_bin}:${PATH}" \
        FAKE_SCENARIO="${scenario}" \
        FAKE_TIP_STATE="${last_state}" \
        FAKE_TIP_LOG="${last_log}" \
        FAKE_REPLAY_LOG="${last_replay_log}" \
        bash "${crate_root}/replay-tip.sh" \
            --diagnose-only \
            --tag finalized \
            "${rpc_url}" >"${last_stdout}" 2>"${last_stderr}"; then
        last_status=0
    else
        last_status="$?"
    fi
}

assert_no_secret() {
    local path
    for path in "$@"; do
        [[ -e "${path}" ]] || continue
        if grep -R -F "${rpc_url}" "${path}" >/dev/null 2>&1; then
            echo "RPC URL leaked into ${path}" >&2
            exit 1
        fi
    done
}

assert_no_number_witness_fallback() {
    jq -s -e \
        'all(.[]; .method != "debug_executionWitness")' \
        "${last_log}" >/dev/null
}

assert_success() {
    local scenario="$1"
    local tag="$2"
    local expected_hash="$3"
    local expected_attempts="$4"
    local expected_calls="$5"
    local expected_reorg="$6"
    run_tip "${scenario}" "${tag}" "${scenario}-output" 3
    if [[ "${last_status}" != 0 ]]; then
        echo "${scenario} failed unexpectedly" >&2
        cat "${last_stdout}" >&2
        cat "${last_stderr}" >&2
        exit 1
    fi
    jq -e \
        --arg tag "${tag}" \
        --arg hash "${expected_hash}" \
        --argjson attempts "${expected_attempts}" \
        --argjson reorg "${expected_reorg}" \
        '
            .status == "success" and
            .success == true and
            .failureCategory == null and
            .requestedTag == $tag and
            .chainId == "0x1" and
            .capturedBlockNumber == 16 and
            .capturedBlockNumberHex == "0x10" and
            .capturedBlockHash == $hash and
            .captureHash == $hash and
            .recheckHash == $hash and
            .attemptCount == $attempts and
            .reorgDetected == $reorg and
            .stale == false and
            .witness == {
                method: "debug_executionWitnessByBlockHash",
                mode: "canonical",
                policy: "production"
            } and
            .replayCommitments.differentialMatch == true and
            .replayCommitments.rawBound == true and
            .replayCommitments.postStateRootVerified == true and
            (.postStateRoot | test("^0x[0-9a-f]{64}$"))
        ' "${last_stdout}" >/dev/null
    jq -e \
        --arg hash "${expected_hash}" \
        '.targetBlockHash == $hash' \
        "${last_output}/bundle.json" >/dev/null
    cmp -s "${last_stdout}" "${last_output}/result.json"
    jq -e \
        --arg hash "${expected_hash}" \
        '.blockHash == $hash and .postStateRootVerified == true' \
        "${last_output}/replay.json" >/dev/null
    test "$(cat "${last_state}")" = "${expected_calls}"
    assert_no_number_witness_fallback
    assert_no_secret \
        "${last_stdout}" \
        "${last_stderr}" \
        "${last_log}" \
        "${last_output}"
    scenario_count=$((scenario_count + 1))
}

assert_failure() {
    local scenario="$1"
    local tag="$2"
    local category="$3"
    local expected_calls="$4"
    local max_attempts="${5:-3}"
    run_tip "${scenario}" "${tag}" "${scenario}-output" "${max_attempts}"
    if [[ "${last_status}" == 0 ]]; then
        echo "${scenario} unexpectedly succeeded" >&2
        exit 1
    fi
    jq -e \
        --arg tag "${tag}" \
        --arg category "${category}" \
        '
            .status == "failure" and
            .success == false and
            .requestedTag == $tag and
            .failureCategory == $category and
            .replay == null
        ' "${last_stdout}" >/dev/null
    test "$(cat "${last_state}")" = "${expected_calls}"
    if [[ -e "${last_output}" ]]; then
        echo "${scenario} left a published artifact" >&2
        exit 1
    fi
    output_base="$(basename -- "${last_output}")"
    if find "${temporary_root}" \
        -maxdepth 1 \
        -name ".${output_base}.attempt-*" \
        -print \
        -quit | grep -q .; then
        echo "${scenario} left a temporary attempt directory" >&2
        exit 1
    fi
    assert_no_number_witness_fallback
    assert_no_secret "${last_stdout}" "${last_stderr}" "${last_log}"
    scenario_count=$((scenario_count + 1))
}

assert_success "finalized_success" "finalized" "${hash_a}" 1 7 false
assert_success "safe_success" "safe" "${hash_a}" 1 7 false
assert_success "latest_success" "latest" "${hash_a}" 1 7 false
assert_success "reorg_once" "latest" "${hash_b}" 2 13 true
jq -e \
    --arg first_hash "${hash_a}" \
    --arg second_hash "${hash_b}" \
    '
        .attempts == [
            {
                attempt: 1,
                capturedNumber: "0x10",
                captureHash: $first_hash,
                recheckHash: $second_hash,
                outcome: "stale",
                stale: true,
                failureCategory: null
            },
            {
                attempt: 2,
                capturedNumber: "0x10",
                captureHash: $second_hash,
                recheckHash: $second_hash,
                outcome: "success",
                stale: false,
                failureCategory: null
            }
        ] and
        .staleAttemptCount == 1
    ' "${last_stdout}" >/dev/null
test "$(wc -l <"${last_replay_log}")" = 2

assert_failure "reorg_exhausted" "latest" "reorg_retry_exhausted" 19 3
jq -e \
    '.attemptCount == 3 and
     .reorgDetected == true and
     .staleAttemptCount == 3 and
     .stale == true and
     all(.attempts[]; .outcome == "stale" and .stale == true)' \
    "${last_stdout}" >/dev/null

assert_failure "witness_missing" "latest" "capability_missing" 6
jq -e \
    '.missingCapabilities == ["debug_executionWitnessByBlockHash"]' \
    "${last_stdout}" >/dev/null
test ! -s "${last_replay_log}"

assert_failure "malformed_tag" "latest" "malformed_rpc_response" 2
assert_failure "null_tag" "safe" "tag_not_found" 2
assert_failure "tag_not_found" "finalized" "tag_not_found" 2
assert_failure "raw_mismatch" "latest" "strict_replay_failed" 7
test "$(wc -l <"${last_replay_log}")" = 1

existing_output="${temporary_root}/existing-output"
mkdir -- "${existing_output}"
printf '%s\n' "do-not-overwrite" >"${existing_output}/sentinel"
sentinel_sha="$(sha256sum "${existing_output}/sentinel" | awk '{print $1}')"
run_tip "latest_success" "latest" "existing-output" 3
test "${last_status}" != 0
jq -e \
    '.status == "failure" and
     .success == false and
     .failureCategory == "artifact_exists" and
     .attemptCount == 0' "${last_stdout}" >/dev/null
test "$(cat "${last_state}")" = 0
test "$(sha256sum "${existing_output}/sentinel" | awk '{print $1}')" = "${sentinel_sha}"
assert_no_secret "${last_stdout}" "${last_stderr}" "${existing_output}"
scenario_count=$((scenario_count + 1))

run_diagnostic "diagnostic_success"
test "${last_status}" = 0
test "$(cat "${last_state}")" = 5
jq -e \
    '.status == "ready" and
     .success == true and
     .ready == true and
     .chainId == "0x1" and
     .requestedTag == "finalized" and
     .capabilities.debugExecutionWitnessByBlockHashCanonical == true and
     .capabilities.debugGetRawHeaderByHashCanonical == true and
     .capabilities.debugGetRawBlockByHashCanonical == true' \
    "${last_stdout}" >/dev/null
assert_no_number_witness_fallback
assert_no_secret "${last_stdout}" "${last_stderr}" "${last_log}"
scenario_count=$((scenario_count + 1))

for diagnostic_case in \
    "diagnostic_witness_missing:debug_executionWitnessByBlockHash:4" \
    "diagnostic_raw_header_missing:debug_getRawHeader:3" \
    "diagnostic_raw_block_missing:debug_getRawBlock:5"; do
    IFS=':' read -r diagnostic_scenario capability expected_calls \
        <<<"${diagnostic_case}"
    run_diagnostic "${diagnostic_scenario}"
    test "${last_status}" != 0
    test "$(cat "${last_state}")" = "${expected_calls}"
    jq -e \
        --arg capability "${capability}" \
        '
            .status == "failure" and
            .success == false and
            .failureCategory == "capability_missing" and
            .missingCapabilities == [$capability]
        ' "${last_stdout}" >/dev/null
    assert_no_number_witness_fallback
    assert_no_secret "${last_stdout}" "${last_stderr}" "${last_log}"
    scenario_count=$((scenario_count + 1))
done

run_tip "wrong_chain" "finalized" "wrong-chain-output" 3
test "${last_status}" != 0
test "$(cat "${last_state}")" = 1
jq -e \
    '.status == "failure" and
     .success == false and
     .failureCategory == "chain_id_mismatch" and
     .missingCapabilities == ["mainnet"]' "${last_stdout}" >/dev/null
test ! -e "${last_output}"
assert_no_secret "${last_stdout}" "${last_stderr}" "${last_log}"
scenario_count=$((scenario_count + 1))

test "${scenario_count}" = 16
printf 'replay-tip hermetic scenarios: %s passed\n' "${scenario_count}"
