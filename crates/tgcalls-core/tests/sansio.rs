use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tgcalls_core::sansio::Engine;
#[test]
fn host_driven_transport_connects() -> Result<(), Box<dyn std::error::Error>> {
    let _ = env_logger::try_init();
    let addresses = ["127.0.0.1:40001".parse()?, "127.0.0.1:40002".parse()?];
    let mut peers = [
        Engine::new(addresses[0], 1000, None, Instant::now())?,
        Engine::new(addresses[1], 2000, None, Instant::now())?,
    ];
    let offer = peers[0].offer()?;
    let answer = peers[1].answer(offer)?;
    peers[0].accept_answer(answer)?;
    let mut queue = VecDeque::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        for (i, peer) in peers.iter_mut().enumerate() {
            peer.tick(Instant::now())?;
            for d in peer.outgoing()? {
                queue.push_back((1 - i, d.bytes));
            }
        }
        let mut count = 0;
        while let Some((target, packet)) = queue.pop_front() {
            peers[target].receive_datagram(addresses[1 - target], &packet, Instant::now())?;
            for d in peers[target].outgoing()? {
                queue.push_back((1 - target, d.bytes));
            }
            count += 1;
            assert!(count < 1024);
        }
        if peers
            .iter()
            .all(|p| p.connection_state.eq_ignore_ascii_case("connected"))
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(format!(
        "states: {}, {}",
        peers[0].connection_state, peers[1].connection_state
    )
    .into())
}

#[test]
fn gathers_server_reflexive_candidate_from_matching_transaction()
-> Result<(), Box<dyn std::error::Error>> {
    use rtc::stun::{
        message::{BINDING_SUCCESS, Message},
        xoraddr::XorMappedAddress,
    };
    use tgcalls_core::negotiation::IceServer;

    let local = "192.0.2.10:59000".parse()?;
    let server: std::net::SocketAddr = "192.0.2.20:3478".parse()?;
    let now = Instant::now();
    let mut engine = Engine::with_ice(
        local,
        111,
        None,
        vec![IceServer {
            host: "192.0.2.20".into(),
            port: 3478,
            username: String::new(),
            password: String::new(),
            turn: false,
        }],
        false,
        now,
    )?;
    let request = engine.outgoing()?.pop().ok_or("no STUN request")?;
    assert_eq!(request.peer, server.to_string());
    let mut request = Message {
        raw: request.bytes,
        ..Default::default()
    };
    request.decode()?;
    let mut response = Message::new();
    response.build(&[
        Box::new(request.transaction_id),
        Box::new(BINDING_SUCCESS),
        Box::new(XorMappedAddress {
            ip: "198.51.100.5".parse()?,
            port: 50101,
        }),
    ])?;
    // An identical response from a different source cannot publish a candidate.
    engine.receive_datagram("192.0.2.30:3478".parse()?, &response.raw, now)?;
    assert_eq!(engine.gathered_candidates().count(), 0);
    engine.receive_datagram(server, &response.raw, now)?;
    let result: Vec<_> = engine.gathered_candidates().collect();
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("198.51.100.5 50101 typ srflx"));
    Ok(())
}
