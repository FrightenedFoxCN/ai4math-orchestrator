#!/usr/bin/env bash
# Run from the workspace root with no outer sandbox-exec profile.
set -euo pipefail

: "${AI4MATH_TEST_LEAN:?set this to the real Lean 4.35.0 binary, not an elan shim}"
[[ $(uname -s) == Darwin && $AI4MATH_TEST_LEAN == /* && -x $AI4MATH_TEST_LEAN ]]
version=$("$AI4MATH_TEST_LEAN" --version)
printf '%s\n' "$version"
[[ $version == *'version 4.35.0,'* && $version == *'commit eb6cafb7290e8bd5f2937522d8d804f58f02bafc,'* ]]
export AI4MATH_TEST_LEAN
export AI4MATH_BROKER_TIMEOUT_SECS=120

cleanup_native_processes() {
    pkill -x lean 2>/dev/null || true
    pkill -x ai4mathd 2>/dev/null || true
    sleep 5
}

run_one() {
    local name=$1 output
    cleanup_native_processes
    shift
    output=$(cargo test --locked "$@" -- "$name" --ignored --exact) || {
        printf '%s\n' "$output"
        return 1
    }
    printf '%s\n' "$output"
    # libtest otherwise exits 0 when a renamed test matches nothing.
    [[ $output == *'test result: ok. 1 passed; 0 failed; 0 ignored;'* ]]
}

cargo build --locked -p ai4math-cli -p ai4math-daemon
cargo test --locked -p ai4math-broker --test lean_kernel_policy --test t3 -- --ignored --test-threads=1
run_one native_lean_witness_checks_exact_proposition_and_ordered_sources -p ai4math-broker --test witness
