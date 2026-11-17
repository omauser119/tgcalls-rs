#!/usr/bin/env bash
set -euo pipefail
task_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
task_identifier="${1:?usage: package-macos-worker.sh com.yourapp.tgcalls.worker}"
[[ "$task_identifier" =~ ^[A-Za-z0-9.-]+$ ]] || exit 2
cd "$task_root"
cargo build --locked --release -p tgcalls-ffi
swift build --package-path sdk/apple -c release -Xlinker "-L$task_root/target/release"
task_bin="$(swift build --package-path sdk/apple -c release --show-bin-path)"
task_bundle="$task_root/artifacts/apple/$task_identifier.xpc"
mkdir -p "$task_bundle/Contents/MacOS"
cp "$task_bin/TgCallsWorker" "$task_bundle/Contents/MacOS/"
cat > "$task_bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>$task_identifier</string>
<key>CFBundleExecutable</key><string>TgCallsWorker</string>
<key>CFBundlePackageType</key><string>XPC!</string>
<key>XPCService</key><dict><key>ServiceType</key><string>Application</string></dict>
</dict></plist>
PLIST
echo "Built $task_bundle. Sign with sdk/apple/worker.entitlements and embed in your signed app's Contents/XPCServices."
