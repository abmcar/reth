#!/usr/bin/env bash
set -euo pipefail

request="$(cat)"
method="$(jq -r '.method' <<<"${request}")"
request_id="$(jq -r '.id' <<<"${request}")"
step="$(cat "${FAKE_TIP_STATE}")"
scenario="${FAKE_SCENARIO}"
hash_a="0x$(printf '11%.0s' {1..32})"
hash_b="0x$(printf '22%.0s' {1..32})"
hash_c="0x$(printf '33%.0s' {1..32})"
hash_d="0x$(printf '44%.0s' {1..32})"
handled=false

jq -e '
    .jsonrpc == "2.0" and
    (.id | type == "number") and
    (.method | type == "string") and
    (.params | type == "array")
' <<<"${request}" >/dev/null
jq -c . <<<"${request}" >>"${FAKE_TIP_LOG}"

emit_result() {
    local result="$1"
    jq -cn \
        --argjson id "${request_id}" \
        --argjson result "${result}" \
        '{jsonrpc: "2.0", id: $id, result: $result}'
    handled=true
}

emit_error() {
    local code="$1"
    local message="$2"
    jq -cn \
        --argjson id "${request_id}" \
        --argjson code "${code}" \
        --arg message "${message}" \
        '{jsonrpc: "2.0", id: $id, error: {code: $code, message: $message}}'
    handled=true
}

emit_block() {
    local hash="$1"
    jq -cn \
        --argjson id "${request_id}" \
        --arg hash "${hash}" \
        '{
            jsonrpc: "2.0",
            id: $id,
            result: {hash: $hash, number: "0x10"}
        }'
    handled=true
}

assert_tag_capture() {
    local expected_tag="$1"
    jq -e \
        --arg tag "${expected_tag}" \
        '.params == [$tag, false]' <<<"${request}" >/dev/null
}

assert_height_recheck() {
    jq -e '.params == ["0x10", false]' <<<"${request}" >/dev/null
}

assert_hash_lookup() {
    local hash="$1"
    jq -e \
        --arg hash "${hash}" \
        '.params == [$hash, false]' <<<"${request}" >/dev/null
}

assert_raw_lookup() {
    local hash="$1"
    jq -e \
        --arg hash "${hash}" \
        '.params == [{blockHash: $hash, requireCanonical: true}]' \
        <<<"${request}" >/dev/null
}

assert_witness_lookup() {
    local hash="$1"
    jq -e \
        --arg hash "${hash}" \
        '.params == [$hash, "canonical"]' <<<"${request}" >/dev/null
}

emit_fetch_step() {
    local offset="$1"
    local hash="$2"
    local behavior="${3:-normal}"
    case "${offset}:${method}" in
        0:eth_getBlockByHash)
            assert_hash_lookup "${hash}"
            emit_block "${hash}"
            ;;
        1:debug_getRawHeader)
            assert_raw_lookup "${hash}"
            if [[ "${behavior}" == "raw_header_missing" ]]; then
                emit_error -32601 "method not found"
            else
                emit_result '"0xc0"'
            fi
            ;;
        2:debug_executionWitnessByBlockHash)
            assert_witness_lookup "${hash}"
            if [[ "${behavior}" == "witness_missing" ]]; then
                emit_error -32601 "method not found"
            else
                emit_result '{
                    "state": ["0xc0"],
                    "codes": ["0x"],
                    "keys": [],
                    "headers": []
                }'
            fi
            ;;
        3:debug_getRawBlock)
            assert_raw_lookup "${hash}"
            if [[ "${behavior}" == "raw_block_missing" ]]; then
                emit_error -32601 "method not found"
            elif [[ "${behavior}" == "raw_mismatch" ]]; then
                emit_result '"0xc2"'
            else
                emit_result '"0xc0"'
            fi
            ;;
    esac
}

emit_probe_step() {
    local offset="$1"
    local hash="$2"
    local behavior="${3:-normal}"
    case "${offset}:${method}" in
        0:debug_getRawHeader)
            assert_raw_lookup "${hash}"
            if [[ "${behavior}" == "raw_header_missing" ]]; then
                emit_error -32601 "method not found"
            else
                emit_result '"0xc0"'
            fi
            ;;
        1:debug_executionWitnessByBlockHash)
            assert_witness_lookup "${hash}"
            if [[ "${behavior}" == "witness_missing" ]]; then
                emit_error -32601 "method not found"
            else
                emit_result '{
                    "state": ["0xc0"],
                    "codes": [],
                    "keys": [],
                    "headers": []
                }'
            fi
            ;;
        2:debug_getRawBlock)
            assert_raw_lookup "${hash}"
            if [[ "${behavior}" == "raw_block_missing" ]]; then
                emit_error -32601 "method not found"
            else
                emit_result '"0xc0"'
            fi
            ;;
    esac
}

if [[ "${step}" == 0 && "${method}" == "eth_chainId" ]]; then
    jq -e '.params == []' <<<"${request}" >/dev/null
    if [[ "${scenario}" == "wrong_chain" ]]; then
        emit_result '"0x2"'
    else
        emit_result '"0x1"'
    fi
fi

case "${scenario}" in
    finalized_success|safe_success|latest_success|raw_mismatch|witness_missing)
        expected_tag="${scenario%%_success}"
        if [[ "${scenario}" == "raw_mismatch" ||
            "${scenario}" == "witness_missing" ]]; then
            expected_tag="latest"
        fi
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "${expected_tag}"
            emit_block "${hash_a}"
        elif [[ "${scenario}" == "witness_missing" &&
            "${step}" == 5 &&
            "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_a}"
        elif ((step >= 2 && step <= 5)); then
            behavior="normal"
            if [[ "${scenario}" == "raw_mismatch" ]]; then
                behavior="raw_mismatch"
            elif [[ "${scenario}" == "witness_missing" ]]; then
                behavior="witness_missing"
            fi
            emit_fetch_step "$((step - 2))" "${hash_a}" "${behavior}"
        elif [[ "${step}" == 6 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_a}"
        fi
        ;;
    reorg_once)
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "latest"
            emit_block "${hash_a}"
        elif ((step >= 2 && step <= 5)); then
            emit_fetch_step "$((step - 2))" "${hash_a}"
        elif [[ "${step}" == 6 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_b}"
        elif [[ "${step}" == 7 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "latest"
            emit_block "${hash_b}"
        elif ((step >= 8 && step <= 11)); then
            emit_fetch_step "$((step - 8))" "${hash_b}"
        elif [[ "${step}" == 12 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_b}"
        fi
        ;;
    reorg_exhausted)
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "latest"
            emit_block "${hash_a}"
        elif ((step >= 2 && step <= 5)); then
            emit_fetch_step "$((step - 2))" "${hash_a}"
        elif [[ "${step}" == 6 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_b}"
        elif [[ "${step}" == 7 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "latest"
            emit_block "${hash_b}"
        elif ((step >= 8 && step <= 11)); then
            emit_fetch_step "$((step - 8))" "${hash_b}"
        elif [[ "${step}" == 12 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_c}"
        elif [[ "${step}" == 13 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "latest"
            emit_block "${hash_c}"
        elif ((step >= 14 && step <= 17)); then
            emit_fetch_step "$((step - 14))" "${hash_c}"
        elif [[ "${step}" == 18 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_height_recheck
            emit_block "${hash_d}"
        fi
        ;;
    malformed_tag)
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "latest"
            printf '%s\n' 'not-json'
            handled=true
        fi
        ;;
    null_tag)
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "safe"
            emit_result 'null'
        fi
        ;;
    tag_not_found)
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "finalized"
            emit_error -32000 "header not found"
        fi
        ;;
    wrong_chain)
        ;;
    diagnostic_success|diagnostic_witness_missing|diagnostic_raw_header_missing|diagnostic_raw_block_missing)
        if [[ "${step}" == 1 && "${method}" == "eth_getBlockByNumber" ]]; then
            assert_tag_capture "finalized"
            emit_block "${hash_a}"
        elif ((step >= 2 && step <= 4)); then
            behavior="normal"
            case "${scenario}" in
                diagnostic_witness_missing) behavior="witness_missing" ;;
                diagnostic_raw_header_missing) behavior="raw_header_missing" ;;
                diagnostic_raw_block_missing) behavior="raw_block_missing" ;;
            esac
            emit_probe_step "$((step - 2))" "${hash_a}" "${behavior}"
        fi
        ;;
esac

if [[ "${handled}" != true ]]; then
    echo "unexpected fake tip curl call ${scenario}:${step}:${method}" >&2
    exit 1
fi

printf '%s\n' "$((step + 1))" >"${FAKE_TIP_STATE}"
