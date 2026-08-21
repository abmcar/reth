#!/usr/bin/env bash
set -euo pipefail

probe_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
experiment_root="$(cd -- "${probe_dir}/../.." && pwd -P)"
reth_root="${experiment_root}/src/reth"
toolchain_root="${experiment_root}/build/toolchain"
target_dir="${experiment_root}/build/factory-probe-target"

expected_reth_commit="1c2942abc6d3b78a7656acdaa985bdac03408a26"
expected_reth_tree="15168a4a1c04e27a93b7a86baadd3f30722514fe"
expected_reth_lock_sha256="9dd1ae47a32ef0c8d789f294115cee44395ca39e2989d8b6cd07a0b25257064d"

export CARGO_HOME="${toolchain_root}/cargo"
export RUSTUP_HOME="${toolchain_root}/rustup"
export PATH="${CARGO_HOME}/bin:${PATH}"
export CARGO_TARGET_DIR="${target_dir}"

actual_reth_commit="$(git -C "${reth_root}" rev-parse HEAD^{commit})"
actual_reth_tree="$(git -C "${reth_root}" rev-parse HEAD^{tree})"
actual_reth_lock_sha256="$(sha256sum "${reth_root}/Cargo.lock" | awk '{print $1}')"

test "${actual_reth_commit}" = "${expected_reth_commit}"
test "${actual_reth_tree}" = "${expected_reth_tree}"
test "${actual_reth_lock_sha256}" = "${expected_reth_lock_sha256}"
test -z "$(git -C "${reth_root}" status --short)"

mkdir -p "${target_dir}"

lock_tuples() {
    awk '
        BEGIN { RS = ""; FS = "\n" }
        /^\[\[package\]\]/ {
            name = version = source = checksum = ""
            for (i = 1; i <= NF; i++) {
                if ($i ~ /^name = /) {
                    name = $i
                    sub(/^name = "/, "", name)
                    sub(/"$/, "", name)
                } else if ($i ~ /^version = /) {
                    version = $i
                    sub(/^version = "/, "", version)
                    sub(/"$/, "", version)
                } else if ($i ~ /^source = /) {
                    source = $i
                    sub(/^source = "/, "", source)
                    sub(/"$/, "", source)
                } else if ($i ~ /^checksum = /) {
                    checksum = $i
                    sub(/^checksum = "/, "", checksum)
                    sub(/"$/, "", checksum)
                }
            }
            if (source ~ /^registry/) {
                print name "\t" version "\t" source "\t" checksum
            }
        }
    ' "$1" | sort -u
}

probe_lock_tuples="${target_dir}/probe-registry-lock-tuples.tsv"
reth_lock_tuples="${target_dir}/reth-registry-lock-tuples.tsv"
lock_tuples "${probe_dir}/Cargo.lock" > "${probe_lock_tuples}"
lock_tuples "${reth_root}/Cargo.lock" > "${reth_lock_tuples}"

if ! comm -23 "${probe_lock_tuples}" "${reth_lock_tuples}" |
    awk 'BEGIN { clean = 1 } { print; clean = 0 } END { exit clean ? 0 : 1 }'
then
    echo "probe Cargo.lock contains registry packages not pinned identically by Reth Cargo.lock" >&2
    exit 1
fi

rustc --version --verbose
cargo --version --verbose
printf 'reth_commit=%s\n' "${actual_reth_commit}"
printf 'reth_tree=%s\n' "${actual_reth_tree}"
printf 'reth_lock_sha256=%s\n' "${actual_reth_lock_sha256}"
printf 'probe_lock_sha256=%s\n' "$(sha256sum "${probe_dir}/Cargo.lock" | awk '{print $1}')"

cargo check --manifest-path "${probe_dir}/Cargo.toml" --tests --locked --offline
cargo test --manifest-path "${probe_dir}/Cargo.toml" --locked --offline

sha256sum \
    "${probe_dir}/Cargo.toml" \
    "${probe_dir}/Cargo.lock" \
    "${probe_dir}/src/lib.rs" \
    "${probe_dir}/README.md" \
    "${probe_dir}/verify.sh"

find "${target_dir}/debug/deps" -maxdepth 1 -type f -executable \
    -name 'reth_dtvm_factory_compile_probe-*' -print0 |
    sort -z |
    xargs -0 -r sha256sum
