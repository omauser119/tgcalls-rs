#!/usr/bin/env bash
set -euo pipefail
task_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$task_root"
test "$(uname -s)" = Darwin || { echo 'Apple builds require macOS and Xcode' >&2; exit 1; }
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    cargo build --locked --release -p tgcalls-ffi --target "$target"
done
mkdir -p artifacts/apple/headers artifacts/apple/simulator
cp crates/tgcalls-ffi/include/tgcalls.h artifacts/apple/headers/
printf '%s\n' 'module CTgCalls { header "tgcalls.h" export * }' > artifacts/apple/headers/module.modulemap
lipo -create target/aarch64-apple-ios-sim/release/libtgcalls_ffi.a target/x86_64-apple-ios/release/libtgcalls_ffi.a -output artifacts/apple/simulator/libtgcalls_ffi.a
rm -rf artifacts/apple/TgCallsRust.xcframework
xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/libtgcalls_ffi.a -headers artifacts/apple/headers \
    -library artifacts/apple/simulator/libtgcalls_ffi.a -headers artifacts/apple/headers \
    -output artifacts/apple/TgCallsRust.xcframework
cp artifacts/tgcalls-worker.wasm artifacts/apple/
