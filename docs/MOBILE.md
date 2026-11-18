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

## iOS interpreter and app integration

No helper-process launch, private XPC API, JIT entitlement or writable executable
memory is required. Wasm is translated to **interpreter bytecode**, not native
machine code. The worker module must be a trusted artifact bundled and signed with
the app, built from this repository; do not load downloaded modules as call data.
This design does not promise App Review approval.

The default limits are 256 MiB linear memory, one memory/table/instance,
100,000 table elements, 2 billion fuel units per request, 8 MiB input and 32 MiB
output. The import allowlist excludes network creation and blocking poll/sleep.
The restricted WASI host exposes clocks and up to 64 KiB of entropy per
request. It provides an empty environment and rejects filesystem operations.
The `wasm32-wasip1` Rust standard library supplies portable clock and entropy
entry points used by webrtc-rs and DTLS. The mobile build has no dependency on
the generic WASI host or its `rustix` version. Protocol datagrams cross an
explicit request/response API.

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

On macOS with Xcode, `bash scripts/build-apple.sh` builds the XCFramework with
an arm64 device slice plus arm64 and x86_64 simulator slices. Use `--device-only`
for an iPhone archive. The framework includes the C ABI and Objective-C
audio device. Copy `TgCallsRust.xcframework/Resources/tgcalls-worker.wasm` into
the host app bundle. Swift hosts can set the memory and fuel limits through
`WasmLimits`. For Swift Package host tests, link the native static library
with `-Xlinker -L/path/to/target/release`.

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

The Objective-C `TGCallsIOSAudioDevice` uses `AVAudioSession` and
`AVAudioEngine`. With app-owned activation it waits for the call session and
CallKit signals. With standalone activation it configures `.playAndRecord`
and `.voiceChat`. The render callback writes actual stereo playback samples
to the echo reference queue. The serial host drains 10-ms blocks for Sonora,
then submits the captured PCM. Route and interruption notifications rebuild
the graph and retain bounded real-time capture/playback queues.

The [Telegram iOS fork](https://github.com/omauser119/Telegram-iOS/tree/calls-rust)
owns MTProto, authenticated key establishment, CallKit and network routing.
The comparative build picks Rust plus Wasmi when the persistent **Calls
Harden Mode** debug option is enabled. It also includes the reference engine
to support user-controlled comparison. The unattended `iphoneos` job builds
an arm64 IPA for manual signing and checks the Mach-O platform plus the
bundled Wasm digest before uploading.

## Request protocol

Requests are JSON objects with an `op`. Byte arrays are JSON arrays of u8, not
base64 strings. Every successful response contains `ok=true`, `payload`, and
optional `datagrams`, `audio`, `video`, `signaling` arrays. **Consume media and
datagrams from every response**, including negotiation and tick responses.

1. Bind UDP in the app; `new(local, ssrc, video?, ice_servers?, relay_only?)`
   creates the worker. Standard UDP STUN/TURN endpoints use numerical addresses.
   Video is
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
6. For device PCM, send `configure_audio`, then `render_audio` and `capture_audio`
   in exact 10 ms blocks. AEC3 and adaptive digital gain use Sonora. `nnnoiseless`
   runs one denoiser per capture channel at 48 kHz. Feed the actual far-end playback
   mix and supply measured capture/playback delay in milliseconds. The worker
   combines two stereo blocks into one 20 ms Opus frame. The app owns devices.
7. When a network change requires rebinding a personal call, supply the new UDP
   socket address with `rebind_personal(local)` and forward new signaling and ICE.
   Destroy the client to end a call. Create a new one after isolation failure.

Group calls use `group_payload`, `phone.joinGroupCall`, `group_connect(params)`
and `group_sources(ssrcs)`. `video_layers` configures up to three increasing
H.264 resolutions as independent RTP encodings. The join payload includes
`SIM` and `FID` source groups. `group_video` applies H.264 source subscriptions
and sends receiver constraints on SCTP. The fork uses the same group path for
ReplayKit presentations with their own join and audio source.

Binary requests use `54 47 43 01` followed by an opcode and a bounded byte
span. Opcodes 1 through 4 take the app-verified 256-byte key, rendered PCM,
captured PCM with a little-endian 16-bit delay, and a full I420 frame.
In conference mode the worker emits `conference_frames` before emitting
outbound media or decoding received media. Opcode 5 takes the four-byte
little-endian request ID followed by the authenticated transform result.
An empty result drops the frame. The host uses its existing `tde2e` call state
and app-authenticated participants to resolve the sender's user ID.

Opcode 6 decodes a broadcast audio part through Remade FFmpeg's Ogg demuxer
and RustyOpus. A playback-only host requests one-second parts over MTProto
and drains decoded 20-ms PCM blocks. The implementation covers automatic
broadcast audio. RTMP and broadcast video retain their existing client path.

The personal iOS bridge uses MTProto V2 signaling and standard TURN over UDP.
Telegram-specific media reflectors, V3 signaling and roaming through a change
of address family need further work. The local video benchmark uses the
PyTGCalls external-frame path with a pre-negotiated loopback media session.
Full PyTGCalls conference join integration remains open.
[H.264/E2E benchmark results](results/pytgcalls-h264-e2e/README.md)

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
