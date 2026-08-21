#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 1 ]]; then
    exit 2
fi

bundle="$1"
block_hash="$(jq -r '.targetBlockHash' "${bundle}")"
raw_block="$(jq -r '.targetBlock' "${bundle}")"
printf '%s\n' "${block_hash}" >>"${FAKE_REPLAY_LOG}"

if [[ "${raw_block}" == "0xc2" ]]; then
    echo "strict importer rejected raw/witness mismatch" >&2
    exit 1
fi

root="0x$(printf 'aa%.0s' {1..32})"
jq -cn \
    --arg block_hash "${block_hash}" \
    --arg root "${root}" \
    '{
        differentialMatch: true,
        rawBound: true,
        preExecutionCommitments: true,
        postExecutionCommitments: {
            gasUsed: true,
            receiptsRoot: true,
            logsBloom: true,
            requestsHash: true,
            blobGasUsed: true
        },
        preStateRoot: $root,
        preStateRootVerified: true,
        postStateRoot: $root,
        postStateRootVerified: true,
        blockNumber: 16,
        blockHash: $block_hash,
        rawBlockBytes: 1,
        transactionCount: 0,
        receiptCount: 0,
        gasUsed: 0,
        blobGasUsed: 0
    }'
