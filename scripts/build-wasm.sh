#!/usr/bin/env bash
set -euo pipefail
task_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "$(uname -s)" == Darwin && -z "${CC_wasm32_wasip1:-}" ]]; then
    task_llvm="$(brew --prefix llvm)"
    export CC_wasm32_wasip1="$task_llvm/bin/clang"
    export AR_wasm32_wasip1="$task_llvm/bin/llvm-ar"
    test -x "$CC_wasm32_wasip1" || { echo 'Install Homebrew LLVM to build the WASI worker' >&2; exit 1; }
fi
cargo build --locked --manifest-path "$task_root/crates/tgcalls-wasm/Cargo.toml" --target wasm32-wasip1 --release
mkdir -p "$task_root/artifacts"
cp "$task_root/crates/tgcalls-wasm/target/wasm32-wasip1/release/tgcalls_wasm.wasm" "$task_root/artifacts/tgcalls-worker.wasm"
cp "$task_root/artifacts/tgcalls-worker.wasm" "$task_root/sdk/apple/Tests/TgCallsRustTests/Resources/"
