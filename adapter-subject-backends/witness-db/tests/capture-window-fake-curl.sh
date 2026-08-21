#!/usr/bin/env bash
set -euo pipefail

request="$(cat)"
request_id="$(jq -r '.id' <<<"${request}")"
method="$(jq -r '.method' <<<"${request}")"
selector="$(jq -r '.params[0] // ""' <<<"${request}")"
step="$(cat "${FAKE_CAPTURE_STATE}")"
scenario="${FAKE_CAPTURE_SCENARIO}"

jq -c . <<<"${request}" >>"${FAKE_CAPTURE_RPC_LOG}"

hash_for() {
    local version="$1"
    local height="$2"
    local base=1000
    if [[ "${version}" == "b" ]]; then
        base=2000
    fi
    printf '0x%064x' "$((base + height))"
}

emit_result() {
    jq -cn \
        --argjson id "${request_id}" \
        --argjson result "$1" \
        '{jsonrpc: "2.0", id: $id, result: $result}'
}

emit_block() {
    local height="$1"
    local version="$2"
    local hash parent
    hash="$(hash_for "${version}" "${height}")"
    parent="$(hash_for "${version}" "$((height - 1))")"
    if [[ "${scenario}" == "parent_mismatch" && "${height}" == 8 ]]; then
        parent="0x$(printf 'ff%.0s' {1..32})"
    fi
    jq -cn \
        --argjson id "${request_id}" \
        --arg number "$(printf '0x%x' "${height}")" \
        --arg hash "${hash}" \
        --arg parent "${parent}" \
        '{
            jsonrpc: "2.0",
            id: $id,
            result: {
                number: $number,
                hash: $hash,
                parentHash: $parent,
                transactions: [
                    "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                ],
                gasUsed: "0x5208",
                blobGasUsed: "0x20000"
            }
        }'
}

if [[ "${method}" == "eth_chainId" ]]; then
    jq -e '.params == []' <<<"${request}" >/dev/null
    if [[ "${scenario}" == "wrong_chain" ]]; then
        emit_result '"0x2"'
    else
        emit_result '"0x1"'
    fi
    exit 0
fi

if [[ "${method}" != "eth_getBlockByNumber" ]]; then
    echo "unexpected method: ${method}" >&2
    exit 1
fi
printf '%s\n' "$((step + 1))" >"${FAKE_CAPTURE_STATE}"

if [[ "${selector}" == "finalized" ]]; then
    case "${scenario}" in
        reorg_retry)
            if (( step >= 18 )); then
                emit_block 16 b
            else
                emit_block 16 a
            fi
            ;;
        *)
            emit_block 16 a
            ;;
    esac
    exit 0
fi

height="$((selector))"
if [[ "${scenario}" == "gap" && "${height}" == 5 ]]; then
    emit_result null
    exit 0
fi

case "${scenario}" in
    reorg_retry)
        if (( step == 17 )); then
            # First attempt's first canonical recheck observes a new hash.
            emit_block 1 b
        elif (( step >= 18 )); then
            emit_block "${height}" b
        else
            emit_block "${height}" a
        fi
        ;;
    retry_exhausted)
        # Each attempt consumes 18 calls: pin, 16 headers, first recheck.
        if (( step % 18 == 17 )); then
            emit_block "${height}" b
        else
            emit_block "${height}" a
        fi
        ;;
    *)
        emit_block "${height}" a
        ;;
esac
