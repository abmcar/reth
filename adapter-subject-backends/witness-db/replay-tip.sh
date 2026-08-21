#!/usr/bin/env bash
set -euo pipefail

crate_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
experiment_root="$(cd -- "${crate_root}/../.." && pwd -P)"
toolchain_root="${experiment_root}/build/toolchain"
target_root="${experiment_root}/build/witness-db-target"

tag="finalized"
max_attempts=3
output_argument=""
diagnose_only=false
rpc_url=""

print_usage() {
    cat >&2 <<EOF
usage: $0 [--tag finalized|safe|latest] [--max-attempts 1-10] [--output OUTPUT_DIR] RPC_URL
       $0 --diagnose-only [--tag finalized|safe|latest] RPC_URL

Production replay requires Mainnet and Reth's native
debug_executionWitnessByBlockHash(hash, "canonical") API.
EOF
}

argument_failure() {
    local category="$1"
    jq -cn \
        --arg requested_tag "${tag}" \
        --arg category "${category}" \
        '{
            schemaVersion: 1,
            status: "failure",
            success: false,
            requestedTag: $requested_tag,
            attemptCount: 0,
            reorgDetected: false,
            stale: false,
            failureCategory: $category
        }'
    exit 2
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --tag)
            [[ "$#" -ge 2 ]] || {
                print_usage
                argument_failure "invalid_arguments"
            }
            tag="$2"
            shift 2
            ;;
        --max-attempts)
            [[ "$#" -ge 2 ]] || {
                print_usage
                argument_failure "invalid_arguments"
            }
            max_attempts="$2"
            shift 2
            ;;
        --output)
            [[ "$#" -ge 2 ]] || {
                print_usage
                argument_failure "invalid_arguments"
            }
            output_argument="$2"
            shift 2
            ;;
        --diagnose-only)
            diagnose_only=true
            shift
            ;;
        --help|-h)
            print_usage
            exit 0
            ;;
        --)
            shift
            if [[ "$#" -ne 1 || -n "${rpc_url}" ]]; then
                print_usage
                argument_failure "invalid_arguments"
            fi
            rpc_url="$1"
            shift
            ;;
        -*)
            print_usage
            argument_failure "invalid_arguments"
            ;;
        *)
            if [[ -n "${rpc_url}" ]]; then
                print_usage
                argument_failure "invalid_arguments"
            fi
            rpc_url="$1"
            shift
            ;;
    esac
done

if [[ -z "${rpc_url}" ]]; then
    print_usage
    argument_failure "invalid_arguments"
fi
if [[ "${tag}" != "finalized" && "${tag}" != "safe" && "${tag}" != "latest" ]]; then
    argument_failure "invalid_tag"
fi
if [[ ! "${max_attempts}" =~ ^[1-9][0-9]*$ ]] ||
    ((max_attempts < 1 || max_attempts > 10)); then
    argument_failure "invalid_max_attempts"
fi
if [[ "${diagnose_only}" == true && -n "${output_argument}" ]]; then
    argument_failure "diagnostic_does_not_write_artifacts"
fi

for command_name in jq curl mktemp; do
    command -v "${command_name}" >/dev/null 2>&1 ||
        argument_failure "missing_local_dependency"
done

rpc_timeout_seconds="${REPLAY_TIP_RPC_TIMEOUT_SECONDS:-600}"
if [[ ! "${rpc_timeout_seconds}" =~ ^[1-9][0-9]*$ ]] ||
    ((rpc_timeout_seconds < 1 || rpc_timeout_seconds > 3600)); then
    argument_failure "invalid_rpc_timeout"
fi

output_directory=""
output_parent=""
output_name=""
if [[ "${diagnose_only}" == false ]]; then
    if [[ -z "${output_argument}" ]]; then
        output_argument="$(
            printf '%s/reth-dtvm-tip-%s-%s' \
                "$(pwd -P)" \
                "${tag}" \
                "$(date -u +%Y%m%dT%H%M%SZ)"
        )"
    fi
    output_parent_argument="$(dirname -- "${output_argument}")"
    output_name="$(basename -- "${output_argument}")"
    if [[ ! -d "${output_parent_argument}" ||
        "${output_name}" == "." ||
        "${output_name}" == ".." ||
        -z "${output_name}" ]]; then
        argument_failure "invalid_output_directory"
    fi
    output_parent="$(cd -- "${output_parent_argument}" && pwd -P)"
    output_directory="${output_parent}/${output_name}"
    if [[ -e "${output_directory}" || -L "${output_directory}" ]]; then
        argument_failure "artifact_exists"
    fi
fi

temporary_root="$(mktemp -d)"
attempt_directory=""
cleanup() {
    if [[ -n "${attempt_directory}" && -d "${attempt_directory}" ]]; then
        rm -rf -- "${attempt_directory}"
    fi
    if [[ -n "${temporary_root}" && -d "${temporary_root}" ]]; then
        rm -rf -- "${temporary_root}"
    fi
}
trap cleanup EXIT

rpc_id=0
rpc_call() {
    local method="$1"
    local params="$2"
    local response="$3"
    local request="${temporary_root}/request-$((rpc_id + 1)).json"
    local curl_error="${temporary_root}/curl-$((rpc_id + 1)).stderr"

    rpc_id=$((rpc_id + 1))
    jq -cn \
        --argjson id "${rpc_id}" \
        --arg method "${method}" \
        --argjson params "${params}" \
        '{jsonrpc: "2.0", id: $id, method: $method, params: $params}' \
        >"${request}"
    curl --fail --silent --show-error \
        --max-time "${rpc_timeout_seconds}" \
        --header 'content-type: application/json' \
        --data-binary @- \
        "${rpc_url}" <"${request}" >"${response}" 2>"${curl_error}"
}

rpc_success() {
    local response="$1"
    local expected_id="$2"
    jq -e \
        --argjson id "${expected_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error == null) and
            has("result")
        ' "${response}" >/dev/null
}

rpc_method_missing() {
    local response="$1"
    jq -e \
        '
            (type == "object") and
            (.error | type == "object") and
            (.error.code == -32601 or .error.code == -32602)
        ' "${response}" >/dev/null 2>&1
}

chain_id=""
captured_number_hex=""
captured_number_decimal=""
captured_hash=""
recheck_hash=""
attempt_count=0
reorg_detected=false
stale_attempt_count=0
attempts='[]'

emit_failure() {
    local category="$1"
    local missing_capabilities="${2:-[]}"
    local stale="${3:-false}"
    jq -cn \
        --arg requested_tag "${tag}" \
        --arg chain_id "${chain_id}" \
        --arg captured_number_hex "${captured_number_hex}" \
        --arg captured_number_decimal "${captured_number_decimal}" \
        --arg captured_hash "${captured_hash}" \
        --arg recheck_hash "${recheck_hash}" \
        --argjson attempt_count "${attempt_count}" \
        --argjson max_attempts "${max_attempts}" \
        --argjson reorg_detected "${reorg_detected}" \
        --argjson stale "${stale}" \
        --argjson stale_attempt_count "${stale_attempt_count}" \
        --arg category "${category}" \
        --argjson missing_capabilities "${missing_capabilities}" \
        --argjson attempts "${attempts}" \
        '{
            schemaVersion: 1,
            status: "failure",
            success: false,
            requestedTag: $requested_tag,
            chainId: (if $chain_id == "" then null else $chain_id end),
            capturedBlockNumber: (
                if $captured_number_decimal == ""
                then null
                else ($captured_number_decimal | tonumber)
                end
            ),
            capturedBlockNumberHex: (
                if $captured_number_hex == "" then null else $captured_number_hex end
            ),
            capturedBlockHash: (
                if $captured_hash == "" then null else $captured_hash end
            ),
            captureHash: (if $captured_hash == "" then null else $captured_hash end),
            recheckHash: (if $recheck_hash == "" then null else $recheck_hash end),
            attemptCount: $attempt_count,
            maxAttempts: $max_attempts,
            reorgDetected: $reorg_detected,
            staleAttemptCount: $stale_attempt_count,
            stale: $stale,
            witness: {
                method: "debug_executionWitnessByBlockHash",
                mode: "canonical",
                policy: "production"
            },
            replay: null,
            failureCategory: $category,
            missingCapabilities: $missing_capabilities,
            attempts: $attempts
        }'
}

append_attempt() {
    local outcome="$1"
    local stale="$2"
    local failure_category="${3:-}"
    attempts="$(
        jq -cn \
            --argjson attempts "${attempts}" \
            --argjson attempt "${attempt_count}" \
            --arg number "${captured_number_hex}" \
            --arg capture_hash "${captured_hash}" \
            --arg recheck_hash "${recheck_hash}" \
            --arg outcome "${outcome}" \
            --argjson stale "${stale}" \
            --arg failure_category "${failure_category}" \
            '
                $attempts + [{
                    attempt: $attempt,
                    capturedNumber: $number,
                    captureHash: $capture_hash,
                    recheckHash: (
                        if $recheck_hash == "" then null else $recheck_hash end
                    ),
                    outcome: $outcome,
                    stale: $stale,
                    failureCategory: (
                        if $failure_category == "" then null else $failure_category end
                    )
                }]
            '
    )"
}

check_chain_id() {
    local response="${temporary_root}/chain-id.json"
    if ! rpc_call "eth_chainId" '[]' "${response}"; then
        return 10
    fi
    local response_id="${rpc_id}"
    if jq -e \
        --argjson id "${response_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error | type == "object")
        ' "${response}" >/dev/null 2>&1; then
        return 13
    fi
    if ! rpc_success "${response}" "${response_id}" ||
        ! jq -e \
            '.result | type == "string" and test("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$")' \
            "${response}" >/dev/null; then
        return 11
    fi
    chain_id="$(jq -r '.result | ascii_downcase' "${response}")"
    if [[ "${chain_id}" != "0x1" ]]; then
        return 12
    fi
}

capture_tag() {
    local response="${temporary_root}/capture-${attempt_count}.json"
    if ! rpc_call \
        "eth_getBlockByNumber" \
        "$(jq -cn --arg tag "${tag}" '[$tag, false]')" \
        "${response}"; then
        return 10
    fi
    local response_id="${rpc_id}"
    if jq -e \
        --argjson id "${response_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error == null) and
            (.result == null)
        ' "${response}" >/dev/null 2>&1; then
        return 12
    fi
    if jq -e \
        '
            (type == "object") and
            (.error | type == "object") and
            (.error.message | type == "string") and
            (.error.message | ascii_downcase | contains("not found"))
        ' "${response}" >/dev/null 2>&1; then
        return 12
    fi
    if jq -e \
        --argjson id "${response_id}" \
        '
            (type == "object") and
            (.jsonrpc == "2.0") and
            (.id == $id) and
            (.error | type == "object")
        ' "${response}" >/dev/null 2>&1; then
        return 13
    fi
    if ! rpc_success "${response}" "${response_id}" ||
        ! jq -e \
            '
                (.result | type == "object") and
                (.result.hash | type == "string") and
                (.result.hash | test("^0x[0-9a-fA-F]{64}$")) and
                (.result.number | type == "string") and
                (.result.number | test("^0x(0|[1-9a-fA-F][0-9a-fA-F]*)$"))
            ' "${response}" >/dev/null; then
        return 11
    fi
    captured_number_hex="$(jq -r '.result.number | ascii_downcase' "${response}")"
    captured_number_decimal="$(printf '%d' "$((captured_number_hex))")"
    captured_hash="$(jq -r '.result.hash | ascii_downcase' "${response}")"
}

recheck_height() {
    local response="${temporary_root}/recheck-${attempt_count}.json"
    recheck_hash=""
    if ! rpc_call \
        "eth_getBlockByNumber" \
        "$(jq -cn --arg number "${captured_number_hex}" '[$number, false]')" \
        "${response}"; then
        return 10
    fi
    local response_id="${rpc_id}"
    if ! rpc_success "${response}" "${response_id}" ||
        ! jq -e \
            --arg number "${captured_number_hex}" \
            '
                (.result | type == "object") and
                (.result.hash | type == "string") and
                (.result.hash | test("^0x[0-9a-fA-F]{64}$")) and
                (.result.number | ascii_downcase) == $number
            ' "${response}" >/dev/null; then
        return 11
    fi
    recheck_hash="$(jq -r '.result.hash | ascii_downcase' "${response}")"
}

probe_capability() {
    local method="$1"
    local params="$2"
    local result_kind="$3"
    local response="${temporary_root}/probe-${method}.json"
    if ! rpc_call "${method}" "${params}" "${response}"; then
        return 10
    fi
    local response_id="${rpc_id}"
    if rpc_method_missing "${response}"; then
        return 20
    fi
    if ! rpc_success "${response}" "${response_id}"; then
        return 21
    fi
    if [[ "${result_kind}" == "raw" ]]; then
        jq -e \
            '.result | type == "string" and test("^0x([0-9a-fA-F]{2})+$")' \
            "${response}" >/dev/null || return 21
    else
        jq -e '
            (.result | type == "object") and
            (.result.state | type == "array") and
            all(.result.state[]; type == "string" and test("^0x([0-9a-fA-F]{2})+$")) and
            (.result.codes | type == "array") and
            all(.result.codes[]; type == "string" and test("^0x([0-9a-fA-F]{2})*$")) and
            (.result.headers | type == "array")
        ' "${response}" >/dev/null || return 21
    fi
}

run_diagnostic() {
    attempt_count=1
    if capture_tag; then
        :
    else
        local capture_status="$?"
        case "${capture_status}" in
            10) emit_failure "rpc_transport_error" '["eth_getBlockByNumber"]' ;;
            12) emit_failure "tag_not_found" '["eth_getBlockByNumber"]' ;;
            13) emit_failure "tag_lookup_failed" '["eth_getBlockByNumber"]' ;;
            *) emit_failure "malformed_rpc_response" '["eth_getBlockByNumber"]' ;;
        esac
        return 1
    fi

    local raw_params
    raw_params="$(
        jq -cn \
            --arg hash "${captured_hash}" \
            '[{blockHash: $hash, requireCanonical: true}]'
    )"
    local witness_params
    witness_params="$(
        jq -cn \
            --arg hash "${captured_hash}" \
            '[$hash, "canonical"]'
    )"

    local method
    local params
    local result_kind
    for capability in \
        "debug_getRawHeader|${raw_params}|raw" \
        "debug_executionWitnessByBlockHash|${witness_params}|witness" \
        "debug_getRawBlock|${raw_params}|raw"; do
        IFS='|' read -r method params result_kind <<<"${capability}"
        if probe_capability "${method}" "${params}" "${result_kind}"; then
            :
        else
            local probe_status="$?"
            local missing
            missing="$(jq -cn --arg method "${method}" '[$method]')"
            if [[ "${probe_status}" == 20 ]]; then
                emit_failure "capability_missing" "${missing}"
            elif [[ "${probe_status}" == 10 ]]; then
                emit_failure "rpc_transport_error" "${missing}"
            else
                emit_failure "capability_invalid_response" "${missing}"
            fi
            return 1
        fi
    done

    jq -cn \
        --arg requested_tag "${tag}" \
        --arg chain_id "${chain_id}" \
        --arg captured_number_hex "${captured_number_hex}" \
        --arg captured_number_decimal "${captured_number_decimal}" \
        --arg captured_hash "${captured_hash}" \
        '{
            schemaVersion: 1,
            status: "ready",
            success: true,
            requestedTag: $requested_tag,
            chainId: $chain_id,
            capturedBlockNumber: ($captured_number_decimal | tonumber),
            capturedBlockNumberHex: $captured_number_hex,
            capturedBlockHash: $captured_hash,
            witness: {
                method: "debug_executionWitnessByBlockHash",
                mode: "canonical",
                policy: "production"
            },
            capabilities: {
                ethChainId: true,
                tagLookup: true,
                debugExecutionWitnessByBlockHashCanonical: true,
                debugGetRawHeaderByHashCanonical: true,
                debugGetRawBlockByHashCanonical: true
            },
            ready: true,
            failureCategory: null,
            missingCapabilities: []
        }'
}

if check_chain_id; then
    :
else
    chain_status="$?"
    case "${chain_status}" in
        10) emit_failure "rpc_transport_error" '["eth_chainId"]' ;;
        12) emit_failure "chain_id_mismatch" '["mainnet"]' ;;
        13) emit_failure "chain_id_lookup_failed" '["eth_chainId"]' ;;
        *) emit_failure "malformed_rpc_response" '["eth_chainId"]' ;;
    esac
    exit 1
fi

if [[ "${diagnose_only}" == true ]]; then
    if run_diagnostic; then
        exit 0
    fi
    exit 1
fi

fetch_witness="${REPLAY_TIP_FETCH_WITNESS:-${crate_root}/fetch-witness.sh}"
if [[ ! -f "${fetch_witness}" ]]; then
    emit_failure "missing_local_dependency" '["fetch-witness.sh"]'
    exit 1
fi

run_replay_block() {
    local bundle="$1"
    local stdout_file="$2"
    local stderr_file="$3"
    if [[ -n "${REPLAY_TIP_REPLAY_BLOCK:-}" ]]; then
        "${REPLAY_TIP_REPLAY_BLOCK}" "${bundle}" >"${stdout_file}" 2>"${stderr_file}"
        return
    fi

    export CARGO_HOME="${CARGO_HOME:-${toolchain_root}/cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-${toolchain_root}/rustup}"
    export PATH="${CARGO_HOME}/bin:${PATH}"
    export CARGO_NET_OFFLINE=true
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${target_root}}"
    export DTVM_REQUIRED=1
    export DTVM_EVM_STRICT_ADDR_CACHE_VALIDATION=true
    if [[ -z "${DTVM_LIBRARY:-}" || -z "${DTVM_LIBRARY_SHA256:-}" ]]; then
        echo "DTVM_LIBRARY and DTVM_LIBRARY_SHA256 are required" >&2
        return 78
    fi
    export DTVM_LIBRARY
    export DTVM_LIBRARY_SHA256

    cargo run \
        --quiet \
        --manifest-path "${crate_root}/Cargo.toml" \
        --locked \
        --offline \
        --bin replay-block \
        -- "${bundle}" >"${stdout_file}" 2>"${stderr_file}"
}

while ((attempt_count < max_attempts)); do
    attempt_count=$((attempt_count + 1))
    captured_number_hex=""
    captured_number_decimal=""
    captured_hash=""
    recheck_hash=""

    if capture_tag; then
        :
    else
        capture_status="$?"
        case "${capture_status}" in
            10) emit_failure "rpc_transport_error" '["eth_getBlockByNumber"]' ;;
            12) emit_failure "tag_not_found" '["eth_getBlockByNumber"]' ;;
            13) emit_failure "tag_lookup_failed" '["eth_getBlockByNumber"]' ;;
            *) emit_failure "malformed_rpc_response" '["eth_getBlockByNumber"]' ;;
        esac
        exit 1
    fi

    attempt_directory="$(
        mktemp -d -- "${output_parent}/.${output_name}.attempt-${attempt_count}.XXXXXX"
    )"
    bundle="${attempt_directory}/bundle.json"
    replay_json="${attempt_directory}/replay.json"
    fetch_stdout="${temporary_root}/fetch-${attempt_count}.stdout"
    fetch_stderr="${temporary_root}/fetch-${attempt_count}.stderr"
    replay_stderr="${temporary_root}/replay-${attempt_count}.stderr"
    attempt_failure=""
    missing_capabilities='[]'

    if ! bash "${fetch_witness}" \
        --policy production \
        "${rpc_url}" \
        "${captured_hash}" \
        "${bundle}" \
        canonical >"${fetch_stdout}" 2>"${fetch_stderr}"; then
        attempt_failure="witness_capture_failed"
        if jq -e \
            '
                .failureCategory == "capability_missing" and
                (.missingCapabilities | type == "array")
            ' "${fetch_stderr}" >/dev/null 2>&1; then
            attempt_failure="capability_missing"
            missing_capabilities="$(jq -c '.missingCapabilities' "${fetch_stderr}")"
        fi
    elif ! jq -e \
        --arg hash "${captured_hash}" \
        '
            .status == "captured" and
            .blockHash == $hash and
            .witnessMethod == "debug_executionWitnessByBlockHash" and
            .witnessMode == "canonical" and
            .witnessPolicy == "production" and
            .usedFallback == false
        ' "${fetch_stdout}" >/dev/null ||
        ! jq -e \
            --arg hash "${captured_hash}" \
            '.targetBlockHash == $hash' \
            "${bundle}" >/dev/null; then
        attempt_failure="witness_capture_identity_mismatch"
    fi

    if [[ -z "${attempt_failure}" ]]; then
        if ! run_replay_block "${bundle}" "${replay_json}" "${replay_stderr}"; then
            attempt_failure="strict_replay_failed"
            rm -f -- "${replay_json}"
        elif ! jq -e \
            --arg hash "${captured_hash}" \
            --argjson number "${captured_number_decimal}" \
            '
                (.blockHash | ascii_downcase) == $hash and
                .blockNumber == $number and
                .differentialMatch == true and
                .rawBound == true and
                .preExecutionCommitments == true and
                .postExecutionCommitments.gasUsed == true and
                .postExecutionCommitments.receiptsRoot == true and
                .postExecutionCommitments.logsBloom == true and
                .postExecutionCommitments.requestsHash == true and
                .postExecutionCommitments.blobGasUsed == true and
                .preStateRootVerified == true and
                .postStateRootVerified == true and
                (.postStateRoot | type == "string") and
                (.postStateRoot | test("^0x[0-9a-fA-F]{64}$"))
            ' "${replay_json}" >/dev/null; then
            attempt_failure="replay_identity_or_commitment_mismatch"
            rm -f -- "${replay_json}"
        fi
    fi

    if recheck_height; then
        :
    else
        append_attempt "failed" false "canonical_recheck_failed"
        rm -rf -- "${attempt_directory}"
        attempt_directory=""
        emit_failure "canonical_recheck_failed" '["eth_getBlockByNumber"]'
        exit 1
    fi

    if [[ "${recheck_hash}" != "${captured_hash}" ]]; then
        reorg_detected=true
        stale_attempt_count=$((stale_attempt_count + 1))
        append_attempt "stale" true ""
        rm -rf -- "${attempt_directory}"
        attempt_directory=""
        if ((attempt_count < max_attempts)); then
            continue
        fi
        emit_failure "reorg_retry_exhausted" '[]' true
        exit 1
    fi

    if [[ -n "${attempt_failure}" ]]; then
        append_attempt "failed" false "${attempt_failure}"
        rm -rf -- "${attempt_directory}"
        attempt_directory=""
        emit_failure "${attempt_failure}" "${missing_capabilities}"
        exit 1
    fi

    append_attempt "success" false ""
    result_json="${attempt_directory}/result.json"
    jq -n \
        --arg requested_tag "${tag}" \
        --arg chain_id "${chain_id}" \
        --arg captured_number_hex "${captured_number_hex}" \
        --argjson captured_number "${captured_number_decimal}" \
        --arg captured_hash "${captured_hash}" \
        --arg recheck_hash "${recheck_hash}" \
        --arg output_directory "${output_directory}" \
        --argjson attempt_count "${attempt_count}" \
        --argjson max_attempts "${max_attempts}" \
        --argjson reorg_detected "${reorg_detected}" \
        --argjson stale_attempt_count "${stale_attempt_count}" \
        --argjson attempts "${attempts}" \
        --slurpfile replay "${replay_json}" \
        '{
            schemaVersion: 1,
            status: "success",
            success: true,
            requestedTag: $requested_tag,
            chainId: $chain_id,
            capturedBlockNumber: $captured_number,
            capturedBlockNumberHex: $captured_number_hex,
            capturedBlockHash: $captured_hash,
            captureHash: $captured_hash,
            recheckHash: $recheck_hash,
            attemptCount: $attempt_count,
            maxAttempts: $max_attempts,
            reorgDetected: $reorg_detected,
            staleAttemptCount: $stale_attempt_count,
            stale: false,
            witness: {
                method: "debug_executionWitnessByBlockHash",
                mode: "canonical",
                policy: "production"
            },
            replayCommitments: {
                differentialMatch: $replay[0].differentialMatch,
                rawBound: $replay[0].rawBound,
                preExecutionCommitments: $replay[0].preExecutionCommitments,
                postExecutionCommitments: $replay[0].postExecutionCommitments,
                preStateRoot: $replay[0].preStateRoot,
                preStateRootVerified: $replay[0].preStateRootVerified,
                postStateRoot: $replay[0].postStateRoot,
                postStateRootVerified: $replay[0].postStateRootVerified
            },
            postStateRoot: $replay[0].postStateRoot,
            replay: $replay[0],
            failureCategory: null,
            missingCapabilities: [],
            attempts: $attempts,
            artifacts: {
                outputDirectory: $output_directory,
                bundle: "bundle.json",
                replay: "replay.json",
                result: "result.json"
            }
        }' >"${result_json}"

    mv -T -n -- "${attempt_directory}" "${output_directory}"
    if [[ -d "${attempt_directory}" ]]; then
        rm -rf -- "${attempt_directory}"
        attempt_directory=""
        emit_failure "artifact_exists"
        exit 1
    fi
    attempt_directory=""
    cat "${output_directory}/result.json"
    exit 0
done

emit_failure "reorg_retry_exhausted" '[]' true
exit 1
