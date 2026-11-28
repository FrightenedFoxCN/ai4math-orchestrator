#!/usr/bin/env bash
# Run from the workspace root with no outer sandbox-exec profile.
set -euo pipefail

: "${AI4MATH_TEST_LEAN:?set this to the real Lean 4.35.0 binary, not an elan shim}"
[[ $(uname -s) == Darwin && $AI4MATH_TEST_LEAN == /* && -x $AI4MATH_TEST_LEAN ]]
version=$("$AI4MATH_TEST_LEAN" --version)
printf '%s\n' "$version"
[[ $version == *'version 4.35.0,'* && $version == *'commit eb6cafb7290e8bd5f2937522d8d804f58f02bafc,'* ]]
export AI4MATH_TEST_LEAN

run_one() {
    local name=$1 output
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
cargo test --locked -p ai4math-broker --test lean_kernel_policy --test t3 -- --ignored
run_one native_lean_witness_checks_exact_proposition_and_ordered_sources -p ai4math-broker --test witness
run_one native_rfc_freeze_and_complete_with_real_lean -p ai4math-daemon --test role_rfc
run_one native_owner_promotion_execute_with_real_lean -p ai4math-daemon --test role_rfc
run_one native_formal_sketch_applies_l3_without_publishing -p ai4math-daemon --test role_rfc
run_one native_lean_promotion_and_downstream_regression -p ai4math-cli --test phase1
run_one native_downstream_failure_does_not_publish -p ai4math-cli --test phase1
run_one managed_tests::managed_grafted_history_neither_accounts_nor_recovers_fake_publication -p ai4math-cli --bin ai4math
run_one managed_tests::managed_promotion_recovers_after_git_publication_and_is_idempotent -p ai4math-cli --bin ai4math
run_one managed_tests::managed_promotion_recovers_after_gc_and_concurrent_cas_failure -p ai4math-cli --bin ai4math
