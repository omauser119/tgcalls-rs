#!/usr/bin/env bash
set -euo pipefail
task_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$task_root"
test "$(uname -s)" = Darwin || { echo 'Apple builds require macOS and Xcode' >&2; exit 1; }
if [[ ! -s artifacts/tgcalls-worker.wasm ]]; then
    rustup target add wasm32-wasip1
    bash scripts/build-wasm.sh
fi
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-15.0}"
audio_sources="$task_root/sdk/apple/Sources/TgCallsIOSAudio"
mkdir -p artifacts/apple/{device,headers,simulator}
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
    sdk=iphonesimulator
    environment=-simulator
    arch=arm64
    case "$target" in
        aarch64-apple-ios) sdk=iphoneos; environment= ;;
        x86_64-apple-ios) arch=x86_64 ;;
    esac
    cargo build --locked --release -p tgcalls-ffi --target "$target"
    xcrun --sdk "$sdk" clang -target "$arch-apple-ios$IPHONEOS_DEPLOYMENT_TARGET$environment" \
        -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" -fobjc-arc -O2 \
        -I "$audio_sources/include" -c "$audio_sources/TGCallsIOSAudioDevice.m" \
        -o "artifacts/apple/$target-audio.o"
    xcrun libtool -static -o "artifacts/apple/$target.a" \
        "target/$target/release/libtgcalls_ffi.a" "artifacts/apple/$target-audio.o"
done
cp crates/tgcalls-ffi/include/tgcalls.h artifacts/apple/headers/
cp "$audio_sources/include/TGCallsIOSAudioDevice.h" artifacts/apple/headers/
cat > artifacts/apple/headers/module.modulemap <<'EOF'
module CTgCalls {
    header "tgcalls.h"
    header "TGCallsIOSAudioDevice.h"
    link framework "AVFoundation"
    link framework "AudioToolbox"
    link framework "Security"
    export *
}
EOF
cp artifacts/apple/aarch64-apple-ios.a artifacts/apple/device/libtgcalls_ffi.a
xcrun lipo -create artifacts/apple/aarch64-apple-ios-sim.a artifacts/apple/x86_64-apple-ios.a \
    -output artifacts/apple/simulator/libtgcalls_ffi.a
rm -rf artifacts/apple/TgCallsRust.xcframework
xcodebuild -create-xcframework \
    -library artifacts/apple/device/libtgcalls_ffi.a -headers artifacts/apple/headers \
    -library artifacts/apple/simulator/libtgcalls_ffi.a -headers artifacts/apple/headers \
    -output artifacts/apple/TgCallsRust.xcframework
mkdir -p artifacts/apple/TgCallsRust.xcframework/Resources
cp artifacts/tgcalls-worker.wasm artifacts/apple/TgCallsRust.xcframework/Resources/
cp artifacts/tgcalls-worker.wasm artifacts/apple/
