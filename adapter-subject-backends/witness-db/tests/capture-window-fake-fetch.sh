#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" != 6 || "$1" != "--policy" || "$2" != "production" || "$6" != "canonical" ]]; then
    echo "unexpected fetch invocation" >&2
    exit 2
fi

rpc_url="$3"
block_hash="${4,,}"
output="$5"
base="$(basename -- "${output}")"
number="${base#block-}"
number="${number%%-*}"

printf '%s\t%s\n' "${number}" "${block_hash}" >>"${FAKE_CAPTURE_FETCH_LOG}"
if [[ "${FAKE_CAPTURE_SCENARIO}" == "fetch_fail" && "${number}" == 8 ]]; then
    exit 1
fi
[[ ! -e "${output}" ]]
jq -cn \
    --argjson number "${number}" \
    --arg hash "${block_hash}" \
    '{fakeBundle: true, number: $number, hash: $hash}' >"${output}"
jq -cn \
    --arg block_number "$(printf '0x%x' "${number}")" \
    --arg block_hash "${block_hash}" \
    '{
        status: "captured",
        blockNumber: $block_number,
        blockHash: $block_hash,
        witnessMethod: "debug_executionWitnessByBlockHash",
        witnessMode: "canonical",
        witnessPolicy: "production",
        usedFallback: false
    }'

# Keep the credential-bearing argument scoped to this process and never print it.
: "${rpc_url:?}"
