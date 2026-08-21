#!/usr/bin/env bash
set -euo pipefail

request="$(cat)"
method="$(jq -r '.method' <<<"${request}")"
request_id="$(jq -r '.id' <<<"${request}")"
step="$(cat "${FAKE_CURL_STATE}")"
scenario="${FAKE_SCENARIO:-current}"
header_hash_14="0x$(printf '22%.0s' {1..32})"
header_hash_15="0x$(printf '33%.0s' {1..32})"
wrong_hash="0x$(printf '44%.0s' {1..32})"

jq -e \
    --argjson id "$((step + 1))" \
    --arg method "${method}" \
    '
        .jsonrpc == "2.0" and
        .id == $id and
        .method == $method and
        (.params | type == "array")
    ' <<<"${request}" >/dev/null

emit_result() {
    local result="$1"
    jq -cn \
        --argjson id "${request_id}" \
        --argjson result "${result}" \
        '{jsonrpc: "2.0", id: $id, result: $result}'
}

emit_error() {
    local code="$1"
    local message="$2"
    jq -cn \
        --argjson id "${request_id}" \
        --argjson code "${code}" \
        --arg message "${message}" \
        '{jsonrpc: "2.0", id: $id, error: {code: $code, message: $message}}'
}

assert_hash_lookup() {
    jq -e \
        '.params == [env.FAKE_BLOCK_HASH, false]' \
        <<<"${request}" >/dev/null
}

assert_target_raw_request() {
    jq -e '
        .params == [{
            blockHash: env.FAKE_BLOCK_HASH,
            requireCanonical: true
        }]
    ' <<<"${request}" >/dev/null
}

case "${scenario}:${step}:${method}" in
    current:0:eth_getBlockByHash | \
    odd_raw:0:eth_getBlockByHash | \
    fallback_success:0:eth_getBlockByHash | \
    fallback_wrong_recheck_hash:0:eth_getBlockByHash | \
    fallback_malformed_headers:0:eth_getBlockByHash | \
    noncompat_error:0:eth_getBlockByHash)
        assert_hash_lookup
        jq -cn \
            --argjson id "${request_id}" \
            --arg hash "${FAKE_BLOCK_HASH}" \
            '{
                jsonrpc: "2.0",
                id: $id,
                result: {hash: $hash, number: "0x10"}
            }'
        ;;

    current:1:debug_getRawHeader | \
    odd_raw:1:debug_getRawHeader | \
    fallback_success:1:debug_getRawHeader | \
    fallback_wrong_recheck_hash:1:debug_getRawHeader | \
    fallback_malformed_headers:1:debug_getRawHeader | \
    noncompat_error:1:debug_getRawHeader)
        assert_target_raw_request
        emit_result '"0xc0"'
        ;;

    current:2:debug_executionWitnessByBlockHash | \
    odd_raw:2:debug_executionWitnessByBlockHash)
        jq -e \
            '.params == [env.FAKE_BLOCK_HASH, "canonical"]' \
            <<<"${request}" >/dev/null
        emit_result '{
            "state": ["0xc0"],
            "codes": ["0x"],
            "keys": null,
            "headers": ["0xc1", "0xc2"]
        }'
        ;;

    current:3:debug_getRawBlock)
        assert_target_raw_request
        emit_result '"0xc0"'
        ;;

    odd_raw:3:debug_getRawBlock)
        assert_target_raw_request
        emit_result '"0x0"'
        ;;

    fallback_success:2:debug_executionWitnessByBlockHash | \
    fallback_malformed_headers:2:debug_executionWitnessByBlockHash)
        jq -e \
            '.params == [env.FAKE_BLOCK_HASH, "canonical"]' \
            <<<"${request}" >/dev/null
        emit_error -32601 "method not found"
        ;;

    fallback_wrong_recheck_hash:2:debug_executionWitnessByBlockHash)
        jq -e \
            '.params == [env.FAKE_BLOCK_HASH, "canonical"]' \
            <<<"${request}" >/dev/null
        emit_error -32602 "invalid params"
        ;;

    noncompat_error:2:debug_executionWitnessByBlockHash)
        jq -e \
            '.params == [env.FAKE_BLOCK_HASH, "canonical"]' \
            <<<"${request}" >/dev/null
        emit_error -32000 "execution failed"
        ;;

    fallback_success:3:debug_executionWitness)
        jq -e '.params == ["0x10"]' <<<"${request}" >/dev/null
        jq -cn \
            --argjson id "${request_id}" \
            --arg old_hash "${header_hash_14}" \
            --arg parent_hash "${header_hash_15}" \
            '{
                jsonrpc: "2.0",
                id: $id,
                result: {
                    state: ["0xc0"],
                    codes: [],
                    headers: [
                        {number: "0xf", hash: $parent_hash},
                        {number: "0xe", hash: $old_hash}
                    ]
                }
            }'
        ;;

    fallback_wrong_recheck_hash:3:debug_executionWitness)
        jq -e '.params == ["0x10"]' <<<"${request}" >/dev/null
        emit_result '{"state":["0xc0"],"codes":[],"headers":[]}'
        ;;

    fallback_malformed_headers:3:debug_executionWitness)
        jq -e '.params == ["0x10"]' <<<"${request}" >/dev/null
        jq -cn \
            --argjson id "${request_id}" \
            --arg hash "${header_hash_15}" \
            '{
                jsonrpc: "2.0",
                id: $id,
                result: {
                    state: ["0xc0"],
                    codes: [],
                    keys: [],
                    headers: [
                        "0xc1",
                        {number: "0xf", hash: $hash}
                    ]
                }
            }'
        ;;

    fallback_success:4:eth_getBlockByNumber | \
    fallback_malformed_headers:4:eth_getBlockByNumber)
        jq -e '.params == ["0x10", false]' <<<"${request}" >/dev/null
        jq -cn \
            --argjson id "${request_id}" \
            --arg hash "${FAKE_BLOCK_HASH}" \
            '{
                jsonrpc: "2.0",
                id: $id,
                result: {hash: $hash, number: "0x10"}
            }'
        ;;

    fallback_wrong_recheck_hash:4:eth_getBlockByNumber)
        jq -e '.params == ["0x10", false]' <<<"${request}" >/dev/null
        jq -cn \
            --argjson id "${request_id}" \
            --arg hash "${wrong_hash}" \
            '{
                jsonrpc: "2.0",
                id: $id,
                result: {hash: $hash, number: "0x10"}
            }'
        ;;

    fallback_success:5:debug_getRawHeader)
        jq -e \
            --arg hash "${header_hash_14}" \
            '.params == [{blockHash: $hash, requireCanonical: true}]' \
            <<<"${request}" >/dev/null
        emit_result '"0xc2"'
        ;;

    fallback_success:6:debug_getRawHeader)
        jq -e \
            --arg hash "${header_hash_15}" \
            '.params == [{blockHash: $hash, requireCanonical: true}]' \
            <<<"${request}" >/dev/null
        emit_result '"0xc3"'
        ;;

    fallback_success:7:debug_getRawBlock)
        assert_target_raw_request
        emit_result '"0xc0"'
        ;;

    *)
        echo "unexpected fake curl call ${scenario}:${step}:${method}" >&2
        exit 1
        ;;
esac

printf '%s\n' "$((step + 1))" >"${FAKE_CURL_STATE}"
