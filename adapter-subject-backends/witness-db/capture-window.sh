#!/usr/bin/env bash
set -euo pipefail

script_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
experiment_root="$(cd -- "${script_root}/../.." && pwd -P)"

fetch_witness="${CAPTURE_WINDOW_FETCH_WITNESS:-${script_root}/fetch-witness.sh}"
verify_witness="${CAPTURE_WINDOW_VERIFY_WITNESS:-${experiment_root}/build/witness-db-target/debug/verify-witness}"
replayer_manifest="${CAPTURE_WINDOW_REPLAYER_MANIFEST:-}"
dtvm_identity_manifest="${CAPTURE_WINDOW_DTVM_IDENTITY_MANIFEST:-}"
reth_repository="${CAPTURE_WINDOW_RETH_REPOSITORY:-${experiment_root}/src/reth}"
requested_tag="finalized"
count=16
max_attempts=3
output=""
rpc_url="${RETH_RPC_URL:-}"

usage() {
    echo "usage: $0 [--tag finalized|safe|latest] [--count N] [--max-attempts N] --output DIRECTORY --dtvm-identity-manifest FILE [--replayer-manifest FILE] [RPC_URL]" >&2
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --tag)
            [[ "$#" -ge 2 ]] || { usage; exit 2; }
            requested_tag="$2"
            shift 2
            ;;
        --count)
            [[ "$#" -ge 2 ]] || { usage; exit 2; }
            count="$2"
            shift 2
            ;;
        --max-attempts)
            [[ "$#" -ge 2 ]] || { usage; exit 2; }
            max_attempts="$2"
            shift 2
            ;;
        --output)
            [[ "$#" -ge 2 ]] || { usage; exit 2; }
            output="$2"
            shift 2
            ;;
        --replayer-manifest)
            [[ "$#" -ge 2 ]] || { usage; exit 2; }
            replayer_manifest="$2"
            shift 2
            ;;
        --dtvm-identity-manifest)
            [[ "$#" -ge 2 ]] || { usage; exit 2; }
            dtvm_identity_manifest="$2"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        --*)
            usage
            exit 2
            ;;
        *)
            if [[ -n "${rpc_url}" ]]; then
                usage
                exit 2
            fi
            rpc_url="$1"
            shift
            ;;
    esac
done

emit_failure() {
    local category="$1"
    local detail="$2"
    local attempts="${3:-0}"
    local last_category="${4:-null}"
    jq -cn \
        --arg tag "${requested_tag}" \
        --arg category "${category}" \
        --arg detail "${detail}" \
        --arg count_text "${count}" \
        --arg max_attempts_text "${max_attempts}" \
        --argjson attempts "${attempts}" \
        --arg last_category "${last_category}" \
        '{
            schema: "reth-dtvm.atomic-capture-window.v1",
            status: "failure",
            success: false,
            failureCategory: $category,
            detail: $detail,
            requestedTag: $tag,
            requestedCount: ($count_text | tonumber? // $count_text),
            maxAttempts: ($max_attempts_text | tonumber? // $max_attempts_text),
            attemptCount: $attempts,
            lastAttemptFailure:
                (if $last_category == "null" then null else $last_category end),
            rpcUrlRecorded: false,
            outputPublished: false
        }'
}

if [[ "${requested_tag}" != "finalized" &&
      "${requested_tag}" != "safe" &&
      "${requested_tag}" != "latest" ]]; then
    emit_failure "invalid_arguments" "tag_must_be_finalized_safe_or_latest"
    exit 2
fi
if [[ ! "${count}" =~ ^[1-9][0-9]*$ ]] || (( count > 256 )); then
    emit_failure "invalid_arguments" "count_must_be_between_1_and_256"
    exit 2
fi
if [[ ! "${max_attempts}" =~ ^[1-9][0-9]*$ ]] || (( max_attempts > 10 )); then
    emit_failure "invalid_arguments" "max_attempts_must_be_between_1_and_10"
    exit 2
fi
if [[ -z "${output}" ]]; then
    emit_failure "invalid_arguments" "output_is_required"
    exit 2
fi
if [[ -z "${rpc_url}" ]]; then
    emit_failure "trusted_rpc_unavailable" "set_RETH_RPC_URL_or_pass_RPC_URL"
    exit 78
fi
if [[ -e "${output}" ]]; then
    emit_failure "output_exists" "refusing_to_overwrite_output"
    exit 2
fi
if [[ ! -x "${fetch_witness}" ]]; then
    emit_failure "capture_tool_unavailable" "fetch_witness_is_not_executable"
    exit 2
fi
if [[ ! -x "${verify_witness}" ]]; then
    emit_failure "capture_tool_unavailable" "verify_witness_is_not_executable"
    exit 2
fi
if [[ -z "${dtvm_identity_manifest}" || ! -f "${dtvm_identity_manifest}" ]]; then
    emit_failure "identity_unavailable" "frozen_DTVM_identity_manifest_is_required"
    exit 2
fi
if ! jq -e '
    (type == "object") and
    (.status == "frozen") and
    (.repository.canonical_name | type == "string") and
    (.refs["refs/heads/main"] | test("^[0-9a-f]{40}$")) and
    (.refs["refs/pull/577/head"] | test("^[0-9a-f]{40}$")) and
    (.refs["refs/pull/579/head"] | test("^[0-9a-f]{40}$")) and
    (.trees | type == "object") and
    all(.trees[]; type == "string" and test("^[0-9a-f]{40}$"))
' "${dtvm_identity_manifest}" >/dev/null 2>&1; then
    emit_failure "identity_unavailable" "DTVM_identity_manifest_is_not_a_frozen_epoch"
    exit 2
fi
if [[ -n "${replayer_manifest}" && ! -f "${replayer_manifest}" ]]; then
    emit_failure "identity_unavailable" "replayer_manifest_does_not_exist"
    exit 2
fi
if [[ ! -d "${reth_repository}" ]]; then
    emit_failure "source_identity_unavailable" "vendored_Reth_repository_is_missing"
    exit 2
fi
if ! reth_head="$(git -C "${reth_repository}" rev-parse --verify HEAD 2>/dev/null)" ||
   ! reth_tree="$(git -C "${reth_repository}" rev-parse --verify 'HEAD^{tree}' 2>/dev/null)" ||
   ! reth_status="$(git -C "${reth_repository}" status --porcelain=v1 2>/dev/null)" ||
   [[ ! "${reth_head}" =~ ^[0-9a-f]{40}$ ]] ||
   [[ ! "${reth_tree}" =~ ^[0-9a-f]{40}$ ]]; then
    emit_failure "source_identity_unavailable" "vendored_Reth_git_identity_could_not_be_resolved"
    exit 2
fi
reth_clean=false
if [[ -z "${reth_status}" ]]; then
    reth_clean=true
fi
if [[ -n "${replayer_manifest}" ]] &&
   ! jq -e 'type == "object"' "${replayer_manifest}" >/dev/null 2>&1; then
    emit_failure "identity_unavailable" "replayer_manifest_is_not_valid_JSON"
    exit 2
fi

output_parent="$(dirname -- "${output}")"
output_name="$(basename -- "${output}")"
if [[ ! -d "${output_parent}" ]]; then
    emit_failure "invalid_arguments" "output_parent_does_not_exist"
    exit 2
fi
output_parent="$(cd -- "${output_parent}" && pwd -P)"
output="${output_parent}/${output_name}"
if [[ -e "${output}" ]]; then
    emit_failure "output_exists" "refusing_to_overwrite_output"
    exit 2
fi

session_root="$(mktemp -d -- "${output_parent}/.${output_name}.capture-session.XXXXXX")"
chmod 700 "${session_root}"
published=false
cleanup() {
    if [[ "${published}" != true ]]; then
        rm -rf -- "${session_root}"
    else
        rmdir -- "${session_root}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

attempt_history="${session_root}/attempts.jsonl"
: >"${attempt_history}"
rpc_id=0
last_failure="unknown"
attempt_failure=""

fail_attempt() {
    attempt_failure="$1"
    return 1
}

rpc_call() {
    local method="$1"
    local params="$2"
    local response="$3"
    local error_log="$4"
    rpc_id=$((rpc_id + 1))
    if ! jq -cn \
        --argjson id "${rpc_id}" \
        --arg method "${method}" \
        --argjson params "${params}" \
        '{jsonrpc: "2.0", id: $id, method: $method, params: $params}' |
        curl --fail --silent --show-error \
            --header 'content-type: application/json' \
            --data-binary @- \
            "${rpc_url}" >"${response}" 2>"${error_log}"; then
        return 1
    fi
    jq -e \
        --argjson id "${rpc_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error == null) and
            has("result")
        ' "${response}" >/dev/null
}

rpc_block_by_number() {
    local selector="$1"
    local response="$2"
    local error_log="$3"
    rpc_call \
        "eth_getBlockByNumber" \
        "$(jq -cn --arg selector "${selector}" '[$selector, false]')" \
        "${response}" \
        "${error_log}"
}

hex_to_dec() {
    local digits="${1#0x}"
    printf '%d' "$((16#${digits}))"
}

dec_to_hex() {
    printf '0x%x' "$1"
}

source_identity_json() {
    local harness_sha fetch_sha verify_sha
    harness_sha="$(sha256sum "${BASH_SOURCE[0]}" | awk '{print $1}')"
    fetch_sha="$(sha256sum "${fetch_witness}" | awk '{print $1}')"
    verify_sha="$(sha256sum "${verify_witness}" | awk '{print $1}')"
    jq -cn \
        --arg adapter_root "${script_root}" \
        --arg harness_path "$(realpath -- "${BASH_SOURCE[0]}")" \
        --arg harness_sha "${harness_sha}" \
        --arg fetch_path "$(realpath -- "${fetch_witness}")" \
        --arg fetch_sha "${fetch_sha}" \
        --arg verify_path "$(realpath -- "${verify_witness}")" \
        --arg verify_sha "${verify_sha}" \
        --arg reth_repository "$(realpath -- "${reth_repository}")" \
        --arg reth_head "${reth_head}" \
        --arg reth_tree "${reth_tree}" \
        --argjson reth_clean "${reth_clean}" \
        '{
            adapterRoot: $adapter_root,
            captureHarness: {realpath: $harness_path, sha256: $harness_sha},
            fetchWitness: {realpath: $fetch_path, sha256: $fetch_sha},
            verifyWitness: {realpath: $verify_path, sha256: $verify_sha},
            vendoredReth: {
                repositoryRealpath: $reth_repository,
                head: $reth_head,
                tree: $reth_tree,
                clean: $reth_clean
            }
        }'
}

dtvm_identity_json() {
    jq -cn \
        --arg path "$(realpath -- "${dtvm_identity_manifest}")" \
        --arg sha "$(sha256sum "${dtvm_identity_manifest}" | awk '{print $1}')" \
        --slurpfile epoch "${dtvm_identity_manifest}" \
        '{
            role: "frozen_source_identity_only_DTVM_not_executed_during_capture",
            manifestRealpath: $path,
            manifestSha256: $sha,
            epoch: {
                status: $epoch[0].status,
                canonicalRepository: $epoch[0].repository.canonical_name,
                captureUtc: $epoch[0].capture_utc,
                driftCheckUtc: $epoch[0].drift_check_utc,
                refs: $epoch[0].refs,
                mergeBase: $epoch[0].merge_base,
                trees: $epoch[0].trees,
                consumerPolicy: $epoch[0].consumer_policy
            }
        }'
}

replayer_identity_json() {
    if [[ -n "${replayer_manifest}" ]]; then
        jq -cn \
            --arg path "$(realpath -- "${replayer_manifest}")" \
            --arg sha "$(sha256sum "${replayer_manifest}" | awk '{print $1}')" \
            --slurpfile manifest "${replayer_manifest}" \
            '{
                role: "downstream_replayer_identity",
                manifestRealpath: $path,
                manifestSha256: $sha,
                replayer: ($manifest[0].replayer // null)
            }'
    else
        jq -cn '{
            role: "capture_only_no_replayer_invoked",
            manifestRealpath: null,
            manifestSha256: null,
            replayer: null
        }'
    fi
}

run_attempt() {
    local attempt="$1"
    local attempt_root="${session_root}/attempt-${attempt}"
    local bundles="${attempt_root}/bundles"
    local pin_response="${attempt_root}/pin.json"
    local chain_response="${attempt_root}/chain-id.json"
    local rpc_error="${attempt_root}/rpc.stderr"
    local blocks_jsonl="${attempt_root}/blocks.jsonl"
    local meta_jsonl="${attempt_root}/block-meta.jsonl"
    local capture_started
    local pin_number_hex pin_number pin_hash start_number
    local previous_hash=""
    local height height_hex header_response block_number_hex block_number
    local block_hash parent_hash gas_used_hex blob_gas_used_hex
    local transaction_count bundle_name bundle_path fetch_json verify_json
    local verify_number verify_hash verify_parent verify_raw_bytes verify_tx_count
    local bundle_sha capture_utc recheck_response recheck_hash

    attempt_failure=""
    mkdir -m 700 -- "${attempt_root}" "${bundles}"
    : >"${blocks_jsonl}"
    : >"${meta_jsonl}"
    capture_started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    if ! rpc_call "eth_chainId" '[]' "${chain_response}" "${rpc_error}"; then
        fail_attempt "chain_id_rpc_failed"
        return
    fi
    if ! jq -e '.result == "0x1"' "${chain_response}" >/dev/null; then
        fail_attempt "non_mainnet_chain"
        return
    fi

    if ! rpc_block_by_number "${requested_tag}" "${pin_response}" "${rpc_error}"; then
        fail_attempt "pin_rpc_failed"
        return
    fi
    if ! jq -e \
        'def quantity:
             type == "string" and test("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$");
         def hash:
             type == "string" and test("^0x[0-9a-fA-F]{64}$");
         (.result | type == "object") and
         (.result.number | quantity) and
         (.result.hash | hash) and
         (.result.parentHash | hash)' \
        "${pin_response}" >/dev/null; then
        fail_attempt "pin_block_missing_or_malformed"
        return
    fi
    pin_number_hex="$(jq -r '.result.number | ascii_downcase' "${pin_response}")"
    pin_hash="$(jq -r '.result.hash | ascii_downcase' "${pin_response}")"
    if ! pin_number="$(hex_to_dec "${pin_number_hex}")" ||
       (( pin_number + 1 < count )); then
        fail_attempt "pin_height_too_low"
        return
    fi
    start_number=$((pin_number - count + 1))

    for ((height = start_number; height <= pin_number; height++)); do
        height_hex="$(dec_to_hex "${height}")"
        header_response="${attempt_root}/header-${height}.json"
        if ! rpc_block_by_number "${height_hex}" "${header_response}" "${rpc_error}"; then
            fail_attempt "window_header_rpc_failed"
            return
        fi
        if ! jq -e \
            --arg number "${height_hex}" \
            'def quantity:
                 type == "string" and test("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$");
             def hash:
                 type == "string" and test("^0x[0-9a-fA-F]{64}$");
             (.result | type == "object") and
             ((.result.number | ascii_downcase) == ($number | ascii_downcase)) and
             (.result.number | quantity) and
             (.result.hash | hash) and
             (.result.parentHash | hash) and
             (.result.transactions | type == "array") and
             (.result.gasUsed | quantity) and
             (
                 ((.result | has("blobGasUsed")) | not) or
                 (.result.blobGasUsed == null) or
                 (.result.blobGasUsed | quantity)
             )' \
            "${header_response}" >/dev/null; then
            fail_attempt "window_block_missing_or_malformed"
            return
        fi
        block_number_hex="$(jq -r '.result.number | ascii_downcase' "${header_response}")"
        block_number="$(hex_to_dec "${block_number_hex}")"
        block_hash="$(jq -r '.result.hash | ascii_downcase' "${header_response}")"
        parent_hash="$(jq -r '.result.parentHash | ascii_downcase' "${header_response}")"
        gas_used_hex="$(jq -r '.result.gasUsed | ascii_downcase' "${header_response}")"
        blob_gas_used_hex="$(jq -r '(.result.blobGasUsed // "0x0") | ascii_downcase' "${header_response}")"
        transaction_count="$(jq -r '.result.transactions | length' "${header_response}")"
        if [[ -n "${previous_hash}" && "${parent_hash}" != "${previous_hash}" ]]; then
            fail_attempt "parent_hash_discontinuity"
            return
        fi
        previous_hash="${block_hash}"
        jq -c '.result' "${header_response}" >>"${blocks_jsonl}"
    done
    if [[ "${previous_hash}" != "${pin_hash}" ]]; then
        fail_attempt "pinned_head_changed_during_window_resolution"
        return
    fi

    while IFS= read -r block; do
        block_number_hex="$(jq -r '.number | ascii_downcase' <<<"${block}")"
        block_number="$(hex_to_dec "${block_number_hex}")"
        block_hash="$(jq -r '.hash | ascii_downcase' <<<"${block}")"
        parent_hash="$(jq -r '.parentHash | ascii_downcase' <<<"${block}")"
        gas_used_hex="$(jq -r '.gasUsed | ascii_downcase' <<<"${block}")"
        blob_gas_used_hex="$(jq -r '(.blobGasUsed // "0x0") | ascii_downcase' <<<"${block}")"
        transaction_count="$(jq -r '.transactions | length' <<<"${block}")"
        bundle_name="block-${block_number}-${block_hash}.json"
        bundle_path="${bundles}/${bundle_name}"
        fetch_json="${attempt_root}/fetch-${block_number}.json"
        verify_json="${attempt_root}/verify-${block_number}.json"

        # This is the only fetch invocation for this pinned hash in this attempt.
        if ! "${fetch_witness}" \
            --policy production \
            "${rpc_url}" \
            "${block_hash}" \
            "${bundle_path}" \
            canonical >"${fetch_json}" 2>"${attempt_root}/fetch-${block_number}.stderr"; then
            fail_attempt "bundle_fetch_failed"
            return
        fi
        if ! jq -e \
            --arg number "${block_number_hex}" \
            --arg hash "${block_hash}" \
            '
                .status == "captured" and
                ((.blockNumber | ascii_downcase) == $number) and
                ((.blockHash | ascii_downcase) == $hash) and
                .witnessMethod == "debug_executionWitnessByBlockHash" and
                .witnessMode == "canonical" and
                .witnessPolicy == "production" and
                .usedFallback == false
            ' "${fetch_json}" >/dev/null; then
            fail_attempt "bundle_fetch_contract_failed"
            return
        fi
        if ! "${verify_witness}" \
            --require-target-block \
            "${bundle_path}" >"${verify_json}" 2>"${attempt_root}/verify-${block_number}.stderr"; then
            fail_attempt "bundle_verify_failed"
            return
        fi
        if ! jq -e \
            --argjson number "${block_number}" \
            --arg hash "${block_hash}" \
            --arg parent "${parent_hash}" \
            '
                .status == "verified" and
                .rawBlockBound == true and
                .bodyCommitmentsVerified == true and
                .targetBlockNumber == $number and
                ((.targetBlockHash | ascii_downcase) == $hash) and
                ((.parentBlockHash | ascii_downcase) == $parent) and
                (.targetBlockRawBytes | type == "number") and
                (.targetBlockRawBytes > 0) and
                (.targetBlockTransactionCount | type == "number")
            ' "${verify_json}" >/dev/null; then
            fail_attempt "bundle_verify_contract_failed"
            return
        fi
        verify_raw_bytes="$(jq -r '.targetBlockRawBytes' "${verify_json}")"
        verify_tx_count="$(jq -r '.targetBlockTransactionCount' "${verify_json}")"
        if [[ "${verify_tx_count}" != "${transaction_count}" ]]; then
            fail_attempt "transaction_count_mismatch"
            return
        fi
        bundle_sha="$(sha256sum "${bundle_path}" | awk '{print $1}')"
        capture_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        jq -cn \
            --argjson number "${block_number}" \
            --arg number_hex "${block_number_hex}" \
            --arg hash "${block_hash}" \
            --arg parent_hash "${parent_hash}" \
            --arg bundle "bundles/${bundle_name}" \
            --arg bundle_sha "${bundle_sha}" \
            --argjson raw_bytes "${verify_raw_bytes}" \
            --argjson transactions "${verify_tx_count}" \
            --argjson gas_used "$(hex_to_dec "${gas_used_hex}")" \
            --arg gas_used_hex "${gas_used_hex}" \
            --argjson blob_gas_used "$(hex_to_dec "${blob_gas_used_hex}")" \
            --arg blob_gas_used_hex "${blob_gas_used_hex}" \
            --arg capture_utc "${capture_utc}" \
            '{
                number: $number,
                numberHex: $number_hex,
                hash: $hash,
                parentHash: $parent_hash,
                bundle: $bundle,
                bundleSha256: $bundle_sha,
                rawBytes: $raw_bytes,
                transactionCount: $transactions,
                gasUsed: $gas_used,
                gasUsedHex: $gas_used_hex,
                blobGasUsed: $blob_gas_used,
                blobGasUsedHex: $blob_gas_used_hex,
                captureUtc: $capture_utc,
                witnessMethod: "debug_executionWitnessByBlockHash",
                witnessMode: "canonical",
                witnessPolicy: "production"
            }' >>"${meta_jsonl}"
    done <"${blocks_jsonl}"

    while IFS= read -r block; do
        block_number_hex="$(jq -r '.number | ascii_downcase' <<<"${block}")"
        block_hash="$(jq -r '.hash | ascii_downcase' <<<"${block}")"
        block_number="$(hex_to_dec "${block_number_hex}")"
        recheck_response="${attempt_root}/recheck-${block_number}.json"
        if ! rpc_block_by_number "${block_number_hex}" "${recheck_response}" "${rpc_error}"; then
            fail_attempt "canonical_recheck_rpc_failed"
            return
        fi
        if ! jq -e \
            --arg number "${block_number_hex}" \
            'def hash:
                 type == "string" and test("^0x[0-9a-fA-F]{64}$");
             (.result | type == "object") and
             ((.result.number | ascii_downcase) == $number) and
             (.result.hash | hash)' \
            "${recheck_response}" >/dev/null; then
            fail_attempt "canonical_recheck_missing_or_malformed"
            return
        fi
        recheck_hash="$(jq -r '.result.hash | ascii_downcase' "${recheck_response}")"
        if [[ "${recheck_hash}" != "${block_hash}" ]]; then
            fail_attempt "canonical_window_changed"
            return
        fi
    done <"${blocks_jsonl}"

    local manifest_tmp="${attempt_root}/manifest.json.tmp"
    local capture_completed
    capture_completed="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    jq -s '.' "${meta_jsonl}" >"${attempt_root}/blocks-array.json"
    source_identity_json >"${attempt_root}/source-identity.json"
    dtvm_identity_json >"${attempt_root}/dtvm-identity.json"
    replayer_identity_json >"${attempt_root}/replayer-identity.json"
    jq -n \
        --arg tag "${requested_tag}" \
        --argjson count "${count}" \
        --argjson attempt_count "${attempt}" \
        --argjson pin_number "${pin_number}" \
        --arg pin_number_hex "${pin_number_hex}" \
        --arg pin_hash "${pin_hash}" \
        --argjson start_number "${start_number}" \
        --arg start_number_hex "$(dec_to_hex "${start_number}")" \
        --arg capture_started "${capture_started}" \
        --arg capture_completed "${capture_completed}" \
        --slurpfile blocks "${attempt_root}/blocks-array.json" \
        --slurpfile source "${attempt_root}/source-identity.json" \
        --slurpfile dtvm "${attempt_root}/dtvm-identity.json" \
        --slurpfile replayer "${attempt_root}/replayer-identity.json" \
        '{
            schema: "reth-dtvm.atomic-capture-window.v1",
            status: "success",
            success: true,
            requestedTag: $tag,
            chainId: "0x1",
            fork: {
                chainId: "0x1",
                rules: "Reth Mainnet canonical execution at pinned headers",
                explicitForkName: null
            },
            count: $count,
            attemptCount: $attempt_count,
            pinnedHead: {
                number: $pin_number,
                numberHex: $pin_number_hex,
                hash: $pin_hash
            },
            range: {
                firstNumber: $start_number,
                firstNumberHex: $start_number_hex,
                lastNumber: $pin_number,
                lastNumberHex: $pin_number_hex
            },
            captureStartedUtc: $capture_started,
            captureCompletedUtc: $capture_completed,
            witness: {
                method: "debug_executionWitnessByBlockHash",
                mode: "canonical",
                policy: "production",
                addressMode: "by_hash",
                fetchesPerHashPerAttempt: 1
            },
            canonicalRecheck: {
                checkedCount: $count,
                allPinnedHashesUnchanged: true
            },
            sourceIdentity: $source[0],
            dtvmIdentity: $dtvm[0],
            replayerIdentity: $replayer[0],
            blocks: $blocks[0],
            rpcUrlRecorded: false,
            atomicPublication: true
        }' >"${manifest_tmp}"
    mv -- "${manifest_tmp}" "${attempt_root}/manifest.json"

    rm -f -- \
        "${attempt_root}"/pin.json \
        "${attempt_root}"/chain-id.json \
        "${attempt_root}"/header-*.json \
        "${attempt_root}"/recheck-*.json \
        "${attempt_root}"/fetch-*.json \
        "${attempt_root}"/fetch-*.stderr \
        "${attempt_root}"/verify-*.json \
        "${attempt_root}"/verify-*.stderr \
        "${attempt_root}/rpc.stderr" \
        "${blocks_jsonl}" \
        "${meta_jsonl}" \
        "${attempt_root}/blocks-array.json" \
        "${attempt_root}/source-identity.json" \
        "${attempt_root}/dtvm-identity.json" \
        "${attempt_root}/replayer-identity.json"
    chmod -R go-rwx "${attempt_root}"

    if [[ -e "${output}" ]]; then
        fail_attempt "output_exists_before_publish"
        return
    fi
    mv -nT -- "${attempt_root}" "${output}"
    if [[ -e "${attempt_root}" || ! -d "${output}" ]]; then
        fail_attempt "output_exists_before_publish"
        return
    fi
    published=true
    rm -f -- "${attempt_history}"
    rmdir -- "${session_root}"
    jq '.' "${output}/manifest.json"
}

attempt=1
while (( attempt <= max_attempts )); do
    if run_attempt "${attempt}"; then
        exit 0
    fi
    last_failure="${attempt_failure:-unknown}"
    jq -cn \
        --argjson attempt "${attempt}" \
        --arg failure "${last_failure}" \
        '{attempt: $attempt, outcome: "discarded", failureCategory: $failure}' \
        >>"${attempt_history}"
    rm -rf -- "${session_root}/attempt-${attempt}"
    if [[ "${last_failure}" == "output_exists_before_publish" ]]; then
        emit_failure "output_exists" "refusing_to_overwrite_output" "${attempt}"
        exit 2
    fi
    attempt=$((attempt + 1))
done

emit_failure \
    "capture_retry_exhausted" \
    "all_private_attempts_discarded" \
    "${max_attempts}" \
    "${last_failure}"
exit 1
