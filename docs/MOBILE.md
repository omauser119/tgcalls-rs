# Apple / Android clients and Hardened Mode

The native C API is in `crates/tgcalls-ffi/include/tgcalls.h`. Swift sources are
in `sdk/apple`; Android sources are in `sdk/android` (API 28+, arm64 build script).
These are library clients: the embedding app owns MTProto authentication,
capture/playback, camera permissions and UDP sockets. No app/session credentials
are included. Benchmarks are not run by these build scripts or CI.

| Platform | Ordinary mode | Hardened boundary |
| --- | --- | --- |
| Linux Python | Tokio/webrtc-rs in process | Existing exec helper, seccomp, brokered UDP FDs and sealed audio rings |
| Android client | Native worker request API | Non-exported bound Service, `isolatedProcess=true`, separate isolated UID; Binder + SharedMemory |
| macOS client | Native worker request API | Signed bundled XPC Service with its own App Sandbox container |
| iOS client | Native worker request API | Wasmi portable interpreter, bounded Wasm linear memory and explicit capabilities |

The latter three use the same host-driven `rtc` engine from webrtc-rs, with
RustyOpus and Remade-With-Rust/rusty_h264. Parsing, DH, encrypted signaling,
ICE/DTLS/SRTP, jitter buffering and codecs execute in the worker. Host UDP brokers
must restrict destinations/rates to the call's negotiated peers/relays. Media and
control requests currently copy through bounded JSON buffers; there is **no claim
of zero-copy media on these mobile bridges**. Linux audio retains its shared ring.

## iOS: working interpreter boundary

No helper-process launch, private XPC API, JIT entitlement or writable executable
memory is required. Wasm is translated to **interpreter bytecode**, not native
machine code. The worker module must be a trusted artifact bundled and signed with
the app, built from this repository; do not load downloaded modules as call data.
This design does not promise App Review approval.

The default limits are 256 MiB linear memory, one memory/table/instance,
100,000 table elements, 2 billion fuel units per request, 8 MiB input and 32 MiB
output. The import allowlist excludes network creation and blocking poll/sleep.
The empty WASI context inherits no descriptors, directories, arguments or
environment. Clocks and OS entropy are available; filesystem imports have no
directory capability. Protocol datagrams cross an explicit request/response API.

A memory fault, panic/abort or fuel exhaustion becomes an interpreter trap.
The complete instance is discarded; generation increases and the next request
creates a fresh instance. Keys, SDP and old call state are **not replayed**. The
app must repeat authenticated negotiation/rejoin and notify the UI. This protects
against faults in guest code; the Rust interpreter, its dependencies and host ABI
remain trusted. It is memory/capability isolation within the iOS process, not an
OS process boundary or protection against an interpreter vulnerability.

Build the artifact and run functional tests on Linux/macOS:

```sh
rustup target add wasm32-wasip1
bash scripts/build-wasm.sh
TGCALLS_WORKER_WASM="$PWD/artifacts/tgcalls-worker.wasm" \
  cargo test --locked -p tgcalls-isolation --features worker-tests
```

On macOS with Xcode, `bash scripts/build-apple.sh` builds an XCFramework for iOS
device and simulator. Add it and the Swift client sources to the app, and add
`artifacts/tgcalls-worker.wasm` to Copy Bundle Resources. For Swift Package builds,
link the static library with `-Xlinker -L/path/to/target/release`.

```swift
let url = Bundle.main.url(forResource: "tgcalls-worker", withExtension: "wasm")!
let client = try TgCallsClient(isolation: .hardened,
                              wasmModule: Data(contentsOf: url))
let response = try await client.request(Data(
    #"{"op":"new","local":"192.0.2.10:40000","ssrc":1234}"#.utf8))
```

The example address represents a UDP socket already bound by the app; use its
actual local address. Handle the missing resource with a normal app error in
production. Missing/invalid modules cause initialization failure; no native-mode
fallback occurs. The Swift test target exercises a real Wasm worker; it requires
`build-wasm.sh` first. Device signing, audio sessions, CallKit and real Telegram
interoperability require embedding-app/device validation.

## Request protocol

Requests are JSON objects with an `op`. Byte arrays are JSON arrays of u8, not
base64 strings. Every successful response contains `ok=true`, `payload`, and
optional `datagrams`, `audio`, `video`, `signaling` arrays. **Consume media and
datagrams from every response**, including negotiation and tick responses.

1. Bind UDP in the app; `new(local, ssrc, video?)` creates the worker. Video is
   bounded Baseline H.264 level 3.1, I420, multiples of 16, up to 1280x720,
   1–60 fps subject to 108,000 macroblocks/sec (720p is at most 30 fps).
2. Generic WebRTC tests use `offer`, `answer(sdp)`, `accept_answer(sdp)`.
3. Personal Telegram calls use `init_dh(g, prime, random, commitment?)` and
   `complete_dh(remote, fingerprint?)`. The caller first sends its commitment;
   the receiver must supply that commitment and later the confirmed fingerprint.
   Only the pinned Telegram 2048-bit safe prime and generator 3 are accepted.
   Private exponents/auth keys never leave the worker. `payload` returns public
   values/fingerprint/verification hash for the app's `phone.*` MTProto exchange.
4. After authenticated DH, `start_personal(outgoing)`; forward `poll_signaling`
   packets via `phone.sendSignalingData` and feed updates to `signaling(packet)`.
   Supported framing/versions: V2, 8.0.0/9.0.0. The app must negotiate that version.
5. Feed incoming UDP to `packet(peer, bytes)`, send every outgoing datagram, and
   call `tick` every 10 ms. `send_audio(pcm)` accepts exactly 3840 s16le bytes
   (20 ms, 48 kHz stereo); `send_video(i420)` accepts the configured frame size.
6. Destroy the client to end a call. Create a new one after isolation failure.

Group audio uses `group_payload` → `phone.joinGroupCall` → `group_connect(params)`;
the richer Linux Python adapter also manages remote audio SSRCs. The mobile
host-driven path currently has host ICE candidates, without STUN gathering or
TURN allocation. For arbitrary Internet/NAT connectivity this must be extended;
do not advertise universal Telegram calling based on LAN loopback tests.
Telegram reflector/MTProto media transport, V3 signaling and simulcast are not
implemented. Core personal/video support is not yet exposed by the legacy Python
NTgCalls adapter. See COMPATIBILITY.md for the current scope.

## macOS XPC

`bash scripts/package-macos-worker.sh com.yourapp.tgcalls.worker` creates a bundle.
Sign it with `sdk/apple/worker.entitlements` and your application signing identity,
then embed in `YourApp.app/Contents/XPCServices`. No sandbox inheritance, network,
user-file or shared app-group entitlement is used. The service verifies these
entitlements and refuses connections if its isolation policy is absent.
`TgCallsClient(isolation: .hardened, macosService: "com.yourapp.tgcalls.worker")`
uses that service; interruption/timeouts fail the operation. XPC launch may create
a fresh service, so the app must start a new call after a failure. Signing and
the running app's XPC containment must be checked in the packaged app, not merely
with `swift test`.

## Android isolated service

Set ANDROID_NDK_HOME, run `bash scripts/build-android.sh`, then build/embed the
Android library with Gradle. Keep the manifest's non-exported isolated service.
`HardenedClient` verifies successful binding; the service checks its isolated UID.
Call `ready()` asynchronously and perform synchronous `request(byte[])` calls on
a dedicated executor, never the UI thread. Binder only carries bounded control
and SharedMemory descriptors; input/output mappings carry larger media requests.
JNI copies bounded byte arrays and catches unwinds. Android's isolated UID and
SELinux policy provide the boundary; Linux helper/seccomp is not applied to ART.

Binding death invalidates the call. Close and create a new HardenedClient, then
renegotiate with fresh keys. Add application backoff (at most 5 restarts/minute).
There is no silent ordinary-mode fallback. Test microphone/camera, background
limits, process death and SELinux on real supported Android versions before
shipping; a cross-build alone does not establish those guarantees.

Primary platform references: [Android Service isolation](https://developer.android.com/guide/topics/manifest/service-element#isolated),
[SharedMemory](https://developer.android.com/reference/android/os/SharedMemory),
[Apple XPC services](https://developer.apple.com/documentation/xpc/creating-xpc-services),
[App Sandbox entitlements](https://developer.apple.com/library/archive/documentation/Miscellaneous/Reference/EntitlementKeyReference/Chapters/EnablingAppSandbox.html).
