#!/usr/bin/env bash
set -euo pipefail
task_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
task_ndk="${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to an Android NDK}"
task_host=linux-x86_64
if [[ "$(uname -s)" = Darwin ]]; then task_host=darwin-x86_64; fi
task_cc="$task_ndk/toolchains/llvm/prebuilt/$task_host/bin/aarch64-linux-android28-clang"
test -x "$task_cc"
export CC_aarch64_linux_android="$task_cc"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$task_cc"
rustup target add aarch64-linux-android
cd "$task_root"
cargo build --locked --release -p tgcalls-ffi --target aarch64-linux-android
mkdir -p sdk/android/src/main/jniLibs/arm64-v8a
cp target/aarch64-linux-android/release/libtgcalls_ffi.so sdk/android/src/main/jniLibs/arm64-v8a/
