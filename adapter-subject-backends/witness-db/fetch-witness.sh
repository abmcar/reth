#!/usr/bin/env bash
set -euo pipefail

policy="production"
if [[ "${1:-}" == "--policy" ]]; then
    if [[ "$#" -lt 2 ]]; then
        echo "missing value for --policy" >&2
        exit 2
    fi
    policy="$2"
    shift 2
fi

if [[ "$#" -lt 3 || "$#" -gt 4 ]]; then
    echo "usage: $0 [--policy production|best-effort] RPC_URL BLOCK_HASH OUTPUT.json [canonical|legacy]" >&2
    exit 2
fi

rpc_url="$1"
block_hash="$2"
output="$3"
mode="${4:-canonical}"

if [[ "${policy}" != "production" && "${policy}" != "best-effort" ]]; then
    echo "witness policy must be production or best-effort" >&2
    exit 2
fi
if [[ ! "${block_hash}" =~ ^0x[0-9a-fA-F]{64}$ ]]; then
    echo "BLOCK_HASH must be a 32-byte 0x-prefixed hash" >&2
    exit 2
fi
if [[ "${mode}" != "canonical" && "${mode}" != "legacy" ]]; then
    echo "witness mode must be canonical or legacy" >&2
    exit 2
fi
if [[ "${policy}" == "production" && "${mode}" != "canonical" ]]; then
    echo "production witness policy requires canonical mode" >&2
    exit 2
fi
if [[ -e "${output}" ]]; then
    echo "refusing to overwrite existing output: ${output}" >&2
    exit 2
fi

temporary_root="$(mktemp -d)"
bundle_tmp=""
cleanup() {
    rm -rf -- "${temporary_root}"
    if [[ -n "${bundle_tmp}" ]]; then
        rm -f -- "${bundle_tmp}"
    fi
}
trap cleanup EXIT

rpc_id=0
last_rpc_id=0
rpc_call() {
    local method="$1"
    local params="$2"
    local response="$3"
    rpc_id=$((rpc_id + 1))
    last_rpc_id="${rpc_id}"
    jq -cn \
        --argjson id "${last_rpc_id}" \
        --arg method "${method}" \
        --argjson params "${params}" \
        '{jsonrpc: "2.0", id: $id, method: $method, params: $params}' |
        curl --fail --silent --show-error \
            --header 'content-type: application/json' \
            --data-binary @- \
            "${rpc_url}" >"${response}"
}

report_invalid_response() {
    local method="$1"
    local response="$2"
    if ! jq -c \
        --arg method "${method}" \
        '{method: $method, jsonrpc, id, error, resultType: (.result | type)}' \
        "${response}" >&2; then
        echo "${method} returned invalid JSON" >&2
    fi
}

validate_raw_result() {
    local method="$1"
    local response="$2"
    local expected_id="$3"
    jq -e \
        --argjson id "${expected_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error == null) and
            has("result") and
            (.result | type == "string") and
            (.result | test("^0x([0-9a-fA-F]{2})+$"))
        ' "${response}" >/dev/null
}

block_response="${temporary_root}/block-by-hash.json"
rpc_call \
    "eth_getBlockByHash" \
    "$(jq -cn --arg hash "${block_hash}" '[$hash, false]')" \
    "${block_response}"
block_response_id="${last_rpc_id}"
jq -e \
    --argjson id "${block_response_id}" \
    --arg hash "${block_hash}" \
    '
        (type == "object") and
        (.jsonrpc == "2.0") and
        (.id == $id) and
        (.error == null) and
        (.result | type == "object") and
        (.result.hash == $hash) and
        (.result.number | type == "string") and
        (.result.number | test("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$"))
    ' "${block_response}" >/dev/null || {
    report_invalid_response "eth_getBlockByHash" "${block_response}"
    exit 1
}
block_number="$(jq -r '.result.number' "${block_response}")"

header_response="${temporary_root}/header.json"
rpc_call \
    "debug_getRawHeader" \
    "$(jq -cn \
        --arg hash "${block_hash}" \
        '[{blockHash: $hash, requireCanonical: true}]'
    )" \
    "${header_response}"
header_response_id="${last_rpc_id}"
validate_raw_result \
    "debug_getRawHeader" \
    "${header_response}" \
    "${header_response_id}" || {
    report_invalid_response "debug_getRawHeader" "${header_response}"
    exit 1
}

witness_response="${temporary_root}/witness.json"
rpc_call \
    "debug_executionWitnessByBlockHash" \
    "$(jq -cn \
        --arg hash "${block_hash}" \
        --arg mode "${mode}" \
        '[$hash, $mode]'
    )" \
    "${witness_response}"
witness_response_id="${last_rpc_id}"
witness_method="debug_executionWitnessByBlockHash"
used_fallback=false

if jq -e '.error == null' "${witness_response}" >/dev/null; then
    jq -e \
        --argjson id "${witness_response_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            has("result") and
            (.result | type == "object")
        ' "${witness_response}" >/dev/null || {
        report_invalid_response "${witness_method}" "${witness_response}"
        exit 1
    }
else
    jq -e \
        --argjson id "${witness_response_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (has("result") | not) and
            (.error | type == "object") and
            (.error.message | type == "string") and
            (.error.code as $code |
                ($code | type == "number") and
                ($code | floor == $code))
        ' "${witness_response}" >/dev/null || {
        report_invalid_response "${witness_method}" "${witness_response}"
        exit 1
    }
    if ! jq -e '.error.code == -32601 or .error.code == -32602' \
        "${witness_response}" >/dev/null; then
        jq -c \
            '{method: "debug_executionWitnessByBlockHash", error}' \
            "${witness_response}" >&2
        exit 1
    fi
    if [[ "${policy}" == "production" ]]; then
        jq -cn \
            '{
                failureCategory: "capability_missing",
                missingCapabilities: ["debug_executionWitnessByBlockHash"],
                witnessMethod: "debug_executionWitnessByBlockHash",
                witnessMode: "canonical",
                witnessPolicy: "production"
            }' >&2
        exit 1
    fi

    rpc_call \
        "debug_executionWitness" \
        "$(jq -cn --arg number "${block_number}" '[$number]')" \
        "${witness_response}"
    witness_response_id="${last_rpc_id}"
    witness_method="debug_executionWitness"
    used_fallback=true
    jq -e \
        --argjson id "${witness_response_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error == null) and
            has("result") and
            (.result | type == "object")
        ' "${witness_response}" >/dev/null || {
        report_invalid_response "${witness_method}" "${witness_response}"
        exit 1
    }

    recheck_response="${temporary_root}/block-by-number.json"
    rpc_call \
        "eth_getBlockByNumber" \
        "$(jq -cn --arg number "${block_number}" '[$number, false]')" \
        "${recheck_response}"
    recheck_response_id="${last_rpc_id}"
    jq -e \
        --argjson id "${recheck_response_id}" \
        --arg hash "${block_hash}" \
        --arg number "${block_number}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error == null) and
            (.result | type == "object") and
            (.result.hash == $hash) and
            (.result.number == $number)
        ' "${recheck_response}" >/dev/null || {
        report_invalid_response "eth_getBlockByNumber" "${recheck_response}"
        exit 1
    }
fi

jq -e '
    (.result.state | type == "array") and
    all(.result.state[]; type == "string" and test("^0x([0-9a-fA-F]{2})+$")) and
    (.result.codes | type == "array") and
    all(.result.codes[]; type == "string" and test("^0x([0-9a-fA-F]{2})*$")) and
    (
        ((.result | has("keys")) | not) or
        (.result.keys == null) or
        (
            (.result.keys | type == "array") and
            all(.result.keys[]; type == "string" and test("^0x([0-9a-fA-F]{2})+$"))
        )
    ) and
    (.result.headers | type == "array")
' "${witness_response}" >/dev/null || {
    report_invalid_response "${witness_method}" "${witness_response}"
    exit 1
}

normalized_witness_base="${temporary_root}/normalized-witness-base.json"
jq '
    .result |
    {
        state,
        codes,
        keys: (if (.keys == null) then [] else .keys end),
        headers
    }
' "${witness_response}" >"${normalized_witness_base}"

normalized_headers="${temporary_root}/normalized-headers.json"
if jq -e 'all(.headers[]; type == "string")' \
    "${normalized_witness_base}" >/dev/null; then
    jq -e \
        'all(.headers[]; test("^0x([0-9a-fA-F]{2})+$"))' \
        "${normalized_witness_base}" >/dev/null || {
        report_invalid_response "${witness_method}" "${witness_response}"
        exit 1
    }
    jq '.headers' "${normalized_witness_base}" >"${normalized_headers}"
elif jq -e 'all(.headers[]; type == "object")' \
    "${normalized_witness_base}" >/dev/null; then
    sorted_header_objects="${temporary_root}/sorted-header-objects.json"
    jq -e '
        all(.headers[];
            (.number | type == "string") and
            (.number | test("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$")) and
            (.hash | type == "string") and
            (.hash | test("^0x[0-9a-fA-F]{64}$"))
        )
    ' "${normalized_witness_base}" >/dev/null || {
        report_invalid_response "${witness_method}" "${witness_response}"
        exit 1
    }
    jq '
        .headers |
        sort_by(
            (.number | ascii_downcase | ltrimstr("0x")) as $digits |
            [($digits | length), $digits]
        )
    ' "${normalized_witness_base}" >"${sorted_header_objects}"
    jq -e '
        group_by(.number | ascii_downcase) |
        all(.[]; length == 1)
    ' "${sorted_header_objects}" >/dev/null || {
        echo "${witness_method} returned duplicate header numbers" >&2
        exit 1
    }

    printf '%s\n' '[]' >"${normalized_headers}"
    header_index=0
    while IFS=$'\t' read -r header_number header_hash; do
        raw_header_response="${temporary_root}/witness-header-${header_index}.json"
        rpc_call \
            "debug_getRawHeader" \
            "$(jq -cn \
                --arg hash "${header_hash}" \
                '[{blockHash: $hash, requireCanonical: true}]'
            )" \
            "${raw_header_response}"
        raw_header_response_id="${last_rpc_id}"
        validate_raw_result \
            "debug_getRawHeader" \
            "${raw_header_response}" \
            "${raw_header_response_id}" || {
            echo "invalid raw witness header ${header_number} (${header_hash})" >&2
            report_invalid_response "debug_getRawHeader" "${raw_header_response}"
            exit 1
        }
        jq \
            --arg raw "$(jq -r '.result' "${raw_header_response}")" \
            '. + [$raw]' \
            "${normalized_headers}" >"${normalized_headers}.next"
        mv -- "${normalized_headers}.next" "${normalized_headers}"
        header_index=$((header_index + 1))
    done < <(jq -r '.[] | [.number, .hash] | @tsv' "${sorted_header_objects}")
else
    echo "${witness_method} returned mixed or malformed headers" >&2
    exit 1
fi

normalized_witness="${temporary_root}/normalized-witness.json"
jq \
    --slurpfile headers "${normalized_headers}" \
    '.headers = $headers[0]' \
    "${normalized_witness_base}" >"${normalized_witness}"

raw_block_response="${temporary_root}/raw-block.json"
rpc_call \
    "debug_getRawBlock" \
    "$(jq -cn \
        --arg hash "${block_hash}" \
        '[{blockHash: $hash, requireCanonical: true}]'
    )" \
    "${raw_block_response}"
raw_block_response_id="${last_rpc_id}"
validate_raw_result \
    "debug_getRawBlock" \
    "${raw_block_response}" \
    "${raw_block_response_id}" || {
    report_invalid_response "debug_getRawBlock" "${raw_block_response}"
    exit 1
}

output_directory="$(dirname -- "${output}")"
output_name="$(basename -- "${output}")"
if [[ ! -d "${output_directory}" ]]; then
    echo "output directory does not exist: ${output_directory}" >&2
    exit 2
fi
bundle_tmp="$(mktemp -- "${output_directory}/.${output_name}.tmp.XXXXXX")"

jq -n \
    --arg block_hash "${block_hash}" \
    --slurpfile header "${header_response}" \
    --slurpfile witness "${normalized_witness}" \
    --slurpfile raw_block "${raw_block_response}" \
    '{
        targetHeader: $header[0].result,
        targetBlockHash: $block_hash,
        targetBlock: $raw_block[0].result,
        witness: $witness[0]
    }' >"${bundle_tmp}"

mv -n -- "${bundle_tmp}" "${output}"
if [[ -e "${bundle_tmp}" ]]; then
    echo "refusing to overwrite existing output: ${output}" >&2
    exit 2
fi
bundle_tmp=""

jq -cn \
    --arg block_number "${block_number}" \
    --arg block_hash "${block_hash}" \
    --arg witness_method "${witness_method}" \
    --arg witness_mode "${mode}" \
    --arg witness_policy "${policy}" \
    --argjson used_fallback "${used_fallback}" \
    '{
        status: "captured",
        blockNumber: $block_number,
        blockHash: $block_hash,
        witnessMethod: $witness_method,
        witnessMode: $witness_mode,
        witnessPolicy: $witness_policy,
        usedFallback: $used_fallback
    }'
