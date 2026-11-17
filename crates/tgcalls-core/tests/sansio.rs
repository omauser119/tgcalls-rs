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
            for d in peer.outgoing() {
                queue.push_back((1 - i, d.bytes));
            }
        }
        let mut count = 0;
        while let Some((target, packet)) = queue.pop_front() {
            peers[target].receive_datagram(addresses[1 - target], &packet, Instant::now())?;
            for d in peers[target].outgoing() {
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
