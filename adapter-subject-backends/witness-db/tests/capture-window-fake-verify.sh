#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" != 2 || "$1" != "--require-target-block" ]]; then
    echo "unexpected verify invocation" >&2
    exit 2
fi

bundle="$2"
base="$(basename -- "${bundle}")"
number="${base#block-}"
number="${number%%-*}"
hash="$(jq -r '.hash' "${bundle}")"
numeric_hash="${hash#0x}"
numeric_hash="$((16#${numeric_hash: -8}))"
version=a
if (( numeric_hash >= 2000 )); then
    version=b
fi
base_value=1000
if [[ "${version}" == b ]]; then
    base_value=2000
fi
parent="0x$(printf '%064x' "$((base_value + number - 1))")"

printf '%s\n' "${number}" >>"${FAKE_CAPTURE_VERIFY_LOG}"
if [[ "${FAKE_CAPTURE_SCENARIO}" == "verify_fail" && "${number}" == 8 ]]; then
    exit 1
fi
jq -cn \
    --argjson number "${number}" \
    --arg hash "${hash}" \
    --arg parent "${parent}" \
    --argjson raw_bytes "$((100 + number))" \
    '{
        targetBlockNumber: $number,
        targetBlockHash: $hash,
        parentBlockNumber: ($number - 1),
        parentBlockHash: $parent,
        targetBlockBinding: "rawBlock",
        rawBlockBound: true,
        bodyCommitmentsVerified: true,
        targetBlockRawBytes: $raw_bytes,
        targetBlockTransactionCount: 2,
        status: "verified"
    }'
