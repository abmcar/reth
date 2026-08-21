#!/usr/bin/env bash
set -euo pipefail

crate_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
temporary_root="$(mktemp -d)"
trap 'rm -rf -- "${temporary_root}"' EXIT

fake_bin="${temporary_root}/bin"
mkdir -- "${fake_bin}"
cp -- "${crate_root}/tests/capture-window-fake-curl.sh" "${fake_bin}/curl"
chmod +x "${fake_bin}/curl"

fake_fetch="${crate_root}/tests/capture-window-fake-fetch.sh"
fake_verify="${crate_root}/tests/capture-window-fake-verify.sh"
dtvm_identity_manifest="${temporary_root}/frozen-identity-epoch.json"
fake_reth_repository="${temporary_root}/reth"
fake_replayer="${temporary_root}/replay-block"
replayer_manifest="${temporary_root}/approved-release-replayer-manifest.json"
rpc_user="fixture-user"
rpc_password="fixture-password"
rpc_authority="${rpc_user}:${rpc_password}"
rpc_url="http://${rpc_authority}@rpc.invalid:8545/private"
scenario_count=0
last_stdout=""
last_stderr=""
last_output=""
last_status=0
last_rpc_log=""
last_fetch_log=""
last_verify_log=""

jq -n '{
    status: "frozen",
    repository: {canonical_name: "DTVMStack/DTVM"},
    refs: {
        "refs/heads/main": "da0678eec4df9facfdc7ae1f6fcd97dcf96a6dd0",
        "refs/pull/577/head": "56729ee377635bbfd869b686713bbd44ab7ac606",
        "refs/pull/579/head": "e6238318de84ebf5a1cbddd594b80143565943ea"
    },
    trees: {
        baseline_main_plus_pr579: "43be400b562fc3cad7e36b48f39fd3c33613ec3b",
        candidate_main_plus_pr577_plus_pr579:
            "c696a3b30e238804606bd2d049775bc16320d9e9"
    }
}' >"${dtvm_identity_manifest}"
mkdir -- "${fake_reth_repository}"
git -C "${fake_reth_repository}" init -q
printf '%s\n' hermetic >"${fake_reth_repository}/README"
git -C "${fake_reth_repository}" add README
GIT_AUTHOR_NAME=Hermetic GIT_AUTHOR_EMAIL=hermetic@example.invalid \
GIT_COMMITTER_NAME=Hermetic GIT_COMMITTER_EMAIL=hermetic@example.invalid \
    git -C "${fake_reth_repository}" commit -qm "hermetic source identity"
printf '%s\n' hermetic-replayer >"${fake_replayer}"
fake_replayer_sha256="$(sha256sum "${fake_replayer}" | awk '{print $1}')"
jq -n \
    --arg realpath "$(realpath -- "${fake_replayer}")" \
    --arg sha256 "${fake_replayer_sha256}" \
    '{replayer: {realpath: $realpath, sha256: $sha256}}' \
    >"${replayer_manifest}"

run_capture() {
    local scenario="$1"
    local max_attempts="${2:-2}"
    local output_name="${3:-${scenario}-output}"
    local state="${temporary_root}/${scenario}.state"
    local -a capture_command
    last_stdout="${temporary_root}/${scenario}.stdout.json"
    last_stderr="${temporary_root}/${scenario}.stderr"
    last_output="${temporary_root}/${output_name}"
    last_rpc_log="${temporary_root}/${scenario}.rpc.jsonl"
    last_fetch_log="${temporary_root}/${scenario}.fetch.tsv"
    last_verify_log="${temporary_root}/${scenario}.verify.txt"
    printf '%s\n' 0 >"${state}"
    : >"${last_rpc_log}"
    : >"${last_fetch_log}"
    : >"${last_verify_log}"

    capture_command=(
        "${crate_root}/capture-window.sh"
        --tag finalized
        --count 16
        --max-attempts "${max_attempts}"
        --dtvm-identity-manifest "${dtvm_identity_manifest}"
        --output "${last_output}"
    )
    if [[ "${scenario}" == "secret_redaction" ]]; then
        capture_command+=(--replayer-manifest "${replayer_manifest}")
    fi
    if PATH="${fake_bin}:${PATH}" \
        FAKE_CAPTURE_SCENARIO="${scenario}" \
        FAKE_CAPTURE_STATE="${state}" \
        FAKE_CAPTURE_RPC_LOG="${last_rpc_log}" \
        FAKE_CAPTURE_FETCH_LOG="${last_fetch_log}" \
        FAKE_CAPTURE_VERIFY_LOG="${last_verify_log}" \
        CAPTURE_WINDOW_FETCH_WITNESS="${fake_fetch}" \
        CAPTURE_WINDOW_VERIFY_WITNESS="${fake_verify}" \
        CAPTURE_WINDOW_RETH_REPOSITORY="${fake_reth_repository}" \
        CAPTURE_WINDOW_MAX_ATTEMPTS="${max_attempts}" \
        DTVM_RETH_ALLOW_LEGACY_SINGLE_ENDPOINT=1 \
        RETH_RPC_URL="${rpc_url}" \
        "${capture_command[@]}" >"${last_stdout}" 2>"${last_stderr}"; then
        last_status=0
    else
        last_status="$?"
    fi
}

assert_no_secret() {
    local path
    for path in "$@"; do
        [[ -e "${path}" ]] || continue
        if grep -R -F "${rpc_url}" "${path}" >/dev/null 2>&1 ||
           grep -R -F "${rpc_password}" "${path}" >/dev/null 2>&1; then
            echo "RPC secret leaked into ${path}" >&2
            exit 1
        fi
    done
}

assert_success() {
    local scenario="$1"
    local expected_attempts="$2"
    local expected_fetches="$3"
    run_capture "${scenario}" 2
    if [[ "${last_status}" != 0 ]]; then
        echo "${scenario} failed unexpectedly" >&2
        cat "${last_stdout}" >&2
        cat "${last_stderr}" >&2
        exit 1
    fi
    jq -e \
        --argjson attempts "${expected_attempts}" \
        '
            .schema == "reth-dtvm.atomic-capture-window.v1" and
            .status == "success" and
            .success == true and
            .requestedTag == "finalized" and
            .chainId == "0x1" and
            .fork == {
                chainId: "0x1",
                rules: "Reth Mainnet canonical execution at pinned headers",
                explicitForkName: null
            } and
            .count == 16 and
            .attemptCount == $attempts and
            .pinnedHead.number == 16 and
            .range.firstNumber == 1 and
            .range.lastNumber == 16 and
            .canonicalRecheck == {
                checkedCount: 16,
                allPinnedHashesUnchanged: true
            } and
            .witness.method == "debug_executionWitnessByBlockHash" and
            .witness.mode == "canonical" and
            .witness.policy == "production" and
            .witness.fetchesPerHashPerAttempt == 1 and
            (.blocks | length) == 16 and
            ([.blocks[].number] == [range(1; 17)]) and
            all(.blocks[];
                (.bundleSha256 | test("^[0-9a-f]{64}$")) and
                .rawBytes > 0 and
                .transactionCount == 2 and
                .gasUsed == 21000 and
                .blobGasUsed == 131072 and
                .witnessMethod == "debug_executionWitnessByBlockHash" and
                .witnessMode == "canonical"
            ) and
            .sourceIdentity.fetchWitness.sha256 != null and
            .sourceIdentity.verifyWitness.sha256 != null and
            (.sourceIdentity.vendoredReth.head | test("^[0-9a-f]{40}$")) and
            (.sourceIdentity.vendoredReth.tree | test("^[0-9a-f]{40}$")) and
            .sourceIdentity.vendoredReth.clean == true and
            .dtvmIdentity.role ==
                "frozen_source_identity_only_DTVM_not_executed_during_capture" and
            (.dtvmIdentity.manifestSha256 | test("^[0-9a-f]{64}$")) and
            .dtvmIdentity.epoch.refs["refs/heads/main"] ==
                "da0678eec4df9facfdc7ae1f6fcd97dcf96a6dd0" and
            .dtvmIdentity.epoch.trees.baseline_main_plus_pr579 ==
                "43be400b562fc3cad7e36b48f39fd3c33613ec3b" and
            .dtvmIdentity.epoch.trees.candidate_main_plus_pr577_plus_pr579 ==
                "c696a3b30e238804606bd2d049775bc16320d9e9" and
            (
                .replayerIdentity.role == "capture_only_no_replayer_invoked" or
                .replayerIdentity.role == "downstream_replayer_identity"
            ) and
            .rpcUrlRecorded == false and
            .atomicPublication == true
        ' "${last_stdout}" >/dev/null
    cmp -s "${last_stdout}" "${last_output}/manifest.json"
    test "$(find "${last_output}/bundles" -maxdepth 1 -type f | wc -l)" = 16
    test "$(wc -l <"${last_fetch_log}")" = "${expected_fetches}"
    test "$(wc -l <"${last_verify_log}")" = "${expected_fetches}"
    jq -s -e '.[0].method == "eth_chainId" and .[0].params == []' \
        "${last_rpc_log}" >/dev/null
    if [[ "${scenario}" == "secret_redaction" ]]; then
        jq -e \
            --arg replayer_sha256 "${fake_replayer_sha256}" \
            '.replayerIdentity.role == "downstream_replayer_identity" and
             (.replayerIdentity.manifestRealpath |
                endswith("/approved-release-replayer-manifest.json")) and
             .replayerIdentity.replayer.sha256 == $replayer_sha256' \
            "${last_stdout}" >/dev/null
    else
        jq -e \
            '.replayerIdentity.role == "capture_only_no_replayer_invoked"' \
            "${last_stdout}" >/dev/null
    fi
    if find "${temporary_root}" -maxdepth 1 \
        -name ".${scenario}-output.capture-session.*" \
        -print -quit | grep -q .; then
        echo "${scenario} left a private session after publication" >&2
        exit 1
    fi
    assert_no_secret \
        "${last_stdout}" "${last_stderr}" "${last_rpc_log}" \
        "${last_fetch_log}" "${last_verify_log}" "${last_output}"
    scenario_count=$((scenario_count + 1))
}

assert_failure() {
    local scenario="$1"
    local last_category="$2"
    local expected_fetches="$3"
    run_capture "${scenario}" 2
    if [[ "${last_status}" == 0 ]]; then
        echo "${scenario} unexpectedly succeeded" >&2
        exit 1
    fi
    jq -e \
        --arg category "${last_category}" \
        '
            .status == "failure" and
            .success == false and
            .failureCategory == "capture_retry_exhausted" and
            .attemptCount == 2 and
            .lastAttemptFailure == $category and
            .rpcUrlRecorded == false and
            .outputPublished == false
        ' "${last_stdout}" >/dev/null
    [[ ! -e "${last_output}" ]]
    test "$(wc -l <"${last_fetch_log}")" = "${expected_fetches}"
    if find "${temporary_root}" -maxdepth 1 \
        \( -name ".${scenario}-output.capture-session.*" -o \
           -name ".${scenario}-output.attempt-*" \) \
        -print -quit | grep -q .; then
        echo "${scenario} left a partial attempt" >&2
        exit 1
    fi
    assert_no_secret \
        "${last_stdout}" "${last_stderr}" "${last_rpc_log}" \
        "${last_fetch_log}" "${last_verify_log}"
    scenario_count=$((scenario_count + 1))
}

# 1: exact contiguous 16-block success.
assert_success "success" 1 16

# The Mainnet hard gate is part of the success protocol, not an additional
# capture scenario: a non-Mainnet endpoint must fail before pinning or fetch.
run_capture "wrong_chain" 2
test "${last_status}" != 0
jq -e \
    '.failureCategory == "capture_retry_exhausted" and
     .lastAttemptFailure == "non_mainnet_chain" and
     .attemptCount == 2 and
     .outputPublished == false' "${last_stdout}" >/dev/null
test "$(wc -l <"${last_fetch_log}")" = 0
test "$(jq -s '[.[] | select(.method == "eth_chainId")] | length' "${last_rpc_log}")" = 2
test "$(jq -s '[.[] | select(.method != "eth_chainId")] | length' "${last_rpc_log}")" = 0
[[ ! -e "${last_output}" ]]
assert_no_secret "${last_stdout}" "${last_stderr}" "${last_rpc_log}"

# 2: a missing height discards each whole attempt.
assert_failure "gap" "window_block_missing_or_malformed" 0

# 3: a broken parent edge is rejected before capture.
assert_failure "parent_mismatch" "parent_hash_discontinuity" 0

# 4: one failed bundle fetch discards the attempt (8 fetches per attempt).
assert_failure "fetch_fail" "bundle_fetch_failed" 16

# 5: one failed verifier discards the attempt (8 fetches per attempt).
assert_failure "verify_fail" "bundle_verify_failed" 16

# 6: a recheck reorg discards all 16 first-attempt bundles, then succeeds.
assert_success "reorg_retry" 2 32

# 7: a reorg on every attempt exhausts the bounded retry.
assert_failure "retry_exhausted" "canonical_window_changed" 32

# 8: an existing output is rejected before any RPC or fetch.
mkdir -- "${temporary_root}/overwrite-output"
run_capture "overwrite" 2 "overwrite-output"
test "${last_status}" = 2
jq -e \
    '.failureCategory == "output_exists" and
     .attemptCount == 0 and
     .outputPublished == false' "${last_stdout}" >/dev/null
test "$(wc -l <"${last_rpc_log}")" = 0
test "$(wc -l <"${last_fetch_log}")" = 0
scenario_count=$((scenario_count + 1))

# 9: a separate successful run makes credential-redaction an explicit gate.
assert_success "secret_redaction" 1 16
assert_no_secret "${temporary_root}"

jq -cn \
    --argjson passed "${scenario_count}" \
    '{
        schema: "reth-dtvm.capture-window-hermetic-tests.v1",
        status: "passed",
        scenariosPassed: $passed,
        scenariosFailed: 0,
        hardGatesPassed: {
            mainnetChainId: true
        },
        realRpcUsed: false,
        realDTVMUsed: false,
        realReplayerUsed: false
    }'
