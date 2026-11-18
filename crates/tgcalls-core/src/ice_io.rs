//! UDP STUN and TURN for hosts that provide their own datagram sockets.
use crate::{Error, Result, negotiation::IceServer};
use rtc::{
    peer_connection::transport::RTCIceCandidateInit,
    sansio::Protocol,
    shared::{TaggedBytesMut, TransportContext, TransportProtocol, error::Error as RtcError},
    stun::{
        message::{
            BINDING_REQUEST, BINDING_SUCCESS, Getter, Message, TransactionId, is_stun_message,
        },
        xoraddr::XorMappedAddress,
    },
    turn::{
        client::{Client, ClientConfig, Event},
        proto::chandata::ChannelData,
    },
};
use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    time::{Duration, Instant},
};

const MAX_SERVERS: usize = 16;
const MAX_PERMISSIONS: usize = 16;
const MAX_PENDING_PACKETS: usize = 8;

struct Binding {
    transaction: TransactionId,
    packet: Vec<u8>,
    attempts: u8,
    next: Instant,
}

struct Allocation {
    client: Client,
    request: TransactionId,
    relay: Option<SocketAddr>,
    pending: BTreeMap<SocketAddr, (TransactionId, VecDeque<TaggedBytesMut>)>,
    finished: bool,
}

#[derive(Default)]
pub(crate) struct IceIo {
    bindings: BTreeMap<SocketAddr, Binding>,
    allocations: BTreeMap<SocketAddr, Allocation>,
}

impl IceIo {
    pub(crate) fn new(local: SocketAddr, servers: Vec<IceServer>, now: Instant) -> Result<Self> {
        if servers.len() > MAX_SERVERS {
            return Err(Error::Invalid("too many ICE servers".into()));
        }
        let mut io = Self::default();
        for server in servers {
            let address = server.address()?;
            if address.is_ipv4() != local.is_ipv4() {
                continue;
            }
            if server.turn {
                if server.username == "reflector" {
                    return Err(Error::Invalid(
                        "Telegram reflector requires its own transport".into(),
                    ));
                }
                if io.allocations.contains_key(&address) {
                    continue;
                }
                let provider =
                    rtc::crypto::default_provider().map_err(|e| Error::Invalid(e.to_string()))?;
                let mut client = Client::new(
                    ClientConfig {
                        local_addr: local,
                        turn_serv_addr: address.to_string(),
                        transport_protocol: TransportProtocol::UDP,
                        username: server.username,
                        password: server.password,
                        allocation_refresh_interval_cap: Some(Duration::from_secs(15)),
                        ..Default::default()
                    },
                    provider,
                )?;
                let request = client.allocate(now)?;
                io.allocations.insert(
                    address,
                    Allocation {
                        client,
                        request,
                        relay: None,
                        pending: BTreeMap::new(),
                        finished: false,
                    },
                );
            } else if !io.bindings.contains_key(&address) {
                let mut request = Message::new();
                request.build(&[Box::<TransactionId>::default(), Box::new(BINDING_REQUEST)])?;
                io.bindings.insert(
                    address,
                    Binding {
                        transaction: request.transaction_id,
                        packet: request.raw,
                        attempts: 0,
                        next: now,
                    },
                );
            }
        }
        Ok(io)
    }

    pub(crate) fn gathering_complete(&self) -> bool {
        self.bindings.is_empty() && self.allocations.values().all(|entry| entry.finished)
    }

    pub(crate) fn poll(&mut self, local: SocketAddr, now: Instant) -> Result<Vec<TaggedBytesMut>> {
        let mut packets = Vec::new();
        self.bindings.retain(|&address, entry| {
            if entry.next > now {
                return true;
            }
            if entry.attempts >= 5 {
                return false;
            }
            packets.push(datagram(
                local,
                address,
                entry.packet.as_slice().into(),
                now,
            ));
            entry.next = now + Duration::from_millis(250u64 << entry.attempts);
            entry.attempts += 1;
            true
        });
        for entry in self.allocations.values_mut() {
            entry.client.handle_timeout(now)?;
            while let Some(packet) = entry.client.poll_write() {
                packets.push(packet);
            }
        }
        Ok(packets)
    }

    pub(crate) fn receive(
        &mut self,
        packet: TaggedBytesMut,
        allow_reflexive: bool,
    ) -> Result<(Option<TaggedBytesMut>, Option<RTCIceCandidateInit>)> {
        let peer = packet.transport.peer_addr;
        let mut stun = Message::new();
        let decoded = if is_stun_message(&packet.message) {
            stun.raw = packet.message.to_vec();
            stun.decode().is_ok()
        } else {
            false
        };
        if decoded
            && self
                .bindings
                .get(&peer)
                .is_some_and(|entry| entry.transaction == stun.transaction_id)
            && stun.typ == BINDING_SUCCESS
        {
            let mut addr = XorMappedAddress::default();
            addr.get_from(&stun)?;
            let mapped = SocketAddr::new(addr.ip, addr.port);
            let local = packet.transport.local_addr;
            self.bindings.remove(&peer);
            return Ok((
                None,
                (allow_reflexive
                    && mapped != local
                    && mapped.is_ipv4() == local.is_ipv4()
                    && !mapped.ip().is_unspecified()
                    && mapped.port() != 0)
                    .then(|| candidate(peer, local, mapped, "srflx")),
            ));
        }
        if let Some(entry) = self.allocations.get_mut(&peer)
            && (ChannelData::is_channel_data(&packet.message)
                || (decoded
                    && (entry.client.has_transaction(&stun.transaction_id)
                        || stun.typ.class == rtc::stun::message::CLASS_INDICATION)))
        {
            entry.client.handle_read(packet)?;
            return Ok((None, None));
        }
        Ok((Some(packet), None))
    }

    pub(crate) fn turn_events(
        &mut self,
        local: SocketAddr,
        now: Instant,
    ) -> Result<(Vec<RTCIceCandidateInit>, Vec<TaggedBytesMut>)> {
        let mut candidates = Vec::new();
        let mut received = Vec::new();
        for (&server, entry) in &mut self.allocations {
            while let Some(event) = entry.client.poll_event() {
                match event {
                    Event::AllocateResponse(id, address) if id == entry.request => {
                        entry.relay = Some(address);
                        entry.finished = true;
                        candidates.push(candidate(server, local, address, "relay"));
                    }
                    Event::AllocateError(id, _) | Event::TransactionTimeout(id)
                        if id == entry.request =>
                    {
                        entry.finished = true;
                    }
                    Event::CreatePermissionResponse(id, peer) => {
                        if entry
                            .pending
                            .get(&peer)
                            .is_some_and(|(current, _)| id == *current)
                            && let Some((_, mut packets)) = entry.pending.remove(&peer)
                            && let Some(relay) = entry.relay
                        {
                            let mut relay = entry.client.relay(relay)?;
                            while let Some(packet) = packets.pop_front() {
                                relay.send_to(now, &packet.message, peer)?;
                            }
                        }
                    }
                    Event::CreatePermissionError(id, _) | Event::TransactionTimeout(id) => {
                        entry.pending.retain(|_, (current, _)| id != *current);
                    }
                    Event::DataIndicationOrChannelData(_, peer, data) => {
                        if let Some(relay) = entry.relay {
                            received.push(datagram(relay, peer, data, now));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok((candidates, received))
    }

    /// Return the original datagram only when its local address is not a relay.
    pub(crate) fn send(&mut self, packet: TaggedBytesMut) -> Result<Option<TaggedBytesMut>> {
        let local = packet.transport.local_addr;
        let peer = packet.transport.peer_addr;
        let Some(entry) = self
            .allocations
            .values_mut()
            .find(|entry| entry.relay == Some(local))
        else {
            return Ok(Some(packet));
        };
        let mut relay = entry.client.relay(local)?;
        match relay.send_to(packet.now, &packet.message, peer) {
            Ok(()) => {}
            Err(RtcError::ErrNoPermission) => {
                if let Some((_, queue)) = entry.pending.get_mut(&peer) {
                    if queue.len() < MAX_PENDING_PACKETS {
                        queue.push_back(packet);
                    }
                } else if entry.pending.len() < MAX_PERMISSIONS
                    && let Some(id) = relay.create_permission(packet.now, peer)?
                {
                    entry.pending.insert(peer, (id, VecDeque::from([packet])));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(None)
    }
}

fn candidate(
    server: SocketAddr,
    local: SocketAddr,
    mapped: SocketAddr,
    kind: &str,
) -> RTCIceCandidateInit {
    let (priority, schema) = if kind == "relay" {
        (16_777_215, "turn")
    } else {
        (1_694_498_815, "stun")
    };
    RTCIceCandidateInit {
        candidate: format!(
            "candidate:{} 1 udp {priority} {} {} typ {kind} raddr {} rport {}",
            server.port(),
            mapped.ip(),
            mapped.port(),
            local.ip(),
            local.port(),
        ),
        url: Some(format!("{schema}:{server}")),
        ..Default::default()
    }
}

pub(crate) fn datagram(
    local: SocketAddr,
    peer: SocketAddr,
    message: bytes::BytesMut,
    now: Instant,
) -> TaggedBytesMut {
    TaggedBytesMut {
        now,
        transport: TransportContext {
            local_addr: local,
            peer_addr: peer,
            transport_protocol: TransportProtocol::UDP,
            ecn: None,
        },
        message,
    }
}
