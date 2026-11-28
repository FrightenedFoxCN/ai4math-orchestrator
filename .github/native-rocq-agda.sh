#!/usr/bin/env bash
# Run from the workspace root with no outer sandbox-exec profile.
# Brokers still apply their own macOS Seatbelt to every native verifier.
set -euo pipefail

[[ $(uname -s) == Darwin ]]
: "${AI4MATH_TEST_AGDA:?set the native Agda Mach-O executable, not a shell wrapper}"
: "${AI4MATH_TEST_AGDA_DATA:?set the trusted Agda prim data directory}"
: "${AI4MATH_TEST_ROCQ:?set the native Rocq 9.3 driver}"
: "${AI4MATH_TEST_ROCQ_DATA:?set the paired Rocq data directory}"
: "${AI4MATH_TEST_ROCQ_SHA256:?set the paired rocqworker SHA-256}"
: "${AI4MATH_TEST_COQCHK:?set the paired native coqchk}"
: "${AI4MATH_TEST_COQCHK_SHA256:?set the paired coqchk SHA-256}"
: "${AI4MATH_TEST_ROCQ_READS:?set narrow compiler runtime dylib grants}"

for executable in "$AI4MATH_TEST_AGDA" "$AI4MATH_TEST_ROCQ" "$AI4MATH_TEST_COQCHK"; do
  [[ $executable == /* && -x $executable ]]
  [[ $(file -b "$executable") == Mach-O* ]]
done
export AI4MATH_ROCQ_RUNTIME_READS="${AI4MATH_ROCQ_RUNTIME_READS:-$AI4MATH_TEST_ROCQ_READS}"

run_one() {
  local name=$1 output
  shift
  output=$(cargo test --locked "$@" -- "$name" --ignored --exact) || {
    printf '%s\n' "$output"
    return 1
  }
  printf '%s\n' "$output"
  [[ $output == *'test result: ok. 1 passed; 0 failed; 0 ignored;'* ]]
}

cargo build --locked -p ai4math-cli -p ai4math-daemon
run_one native_agda_promotion -p ai4math-cli --test phase1
cargo test --locked -p ai4math-broker --test rocq -- --ignored
cargo test --locked -p ai4math-cli rocq_tests -- --include-ignored
run_one rocq_t2::tests::native_rocq_t2_checks_fresh_objects_without_changing_git_or_ledger \
  -p ai4math-cli --bin ai4math
run_one native_daemon_rocq_slot_checks_without_promotion_authority \
  -p ai4math-daemon --test workflow
