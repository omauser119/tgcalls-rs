# Upstream transport patch

`vendor/webrtc` is the published `webrtc` crate **0.21.1** from
<https://github.com/webrtc-rs/webrtc>. The crate's `.cargo_vcs_info.json` records
upstream revision `d658b5a803dd8dc389547450e245877310c9cbed`.
Upstream MIT/Apache-2.0 licenses are retained. Cargo.lock pins the remaining
crates.io dependencies. RustyOpus is separately pinned to
`782bd611d05b266c72338b74fd3631ae7afe563b`.

Local changes are limited to two transport files, reproduced in
[webrtc-prebound.patch](webrtc-prebound.patch):

1. The peer-connection builder accepts owned, already-bound UDP sockets.
2. The driver wraps those descriptors directly and advertises their actual
   `local_addr`, rather than creating replacement sockets.
3. Mixing prebound sockets with address-based binding fails. An ICE transport
   rebind also fails rather than silently creating non-brokered sockets; the
   supervisor/application must broker a new connection.

The worker disables mDNS and does not use DNS, TCP, STUN or TURN servers during
local bootstrap. Telegram group-call server candidates are obtained through
MTProto and validated as numeric addresses. The local SFU and sandbox tests
exercise real ICE/DTLS/SRTP through this transport path.

When upgrading upstream, reapply/review this patch, regenerate the diff, check
the syscall policy, and rerun the SFU/audio/crash tests. Vendoring does not imply
that upstream or this patch has received an independent security audit.
