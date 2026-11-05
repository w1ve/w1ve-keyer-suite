//! A real RWK Keyer, driven from the suite's own rwk-core, for ESP32 WAN interop testing.
//!
//!   rwk_keyer <relay host:port> <pairing-code> [wpm] [seconds]
//!
//! It pairs through the relay (direct WireGuard if the NAT allows, else relayed), opens the keying
//! edge over the tunnel to the Station inner address, and sends "ET ET ..." at the given speed, the
//! way the desktop Remote Keyer Client does. Prints what path it got and whether PONGs came back,
//! so the ESP is proven against genuine suite transport code rather than a second implementation.
use std::time::Duration;

use rwk_core::config::{Config, RelayConfig, Role};
use rwk_core::engine::network::EdgeTransport;
use rwk_core::engine::session::Session;
use rwk_core::engine::tunnel::{self, Path};
use rwk_core::protocol::edge::{EdgeEntry, RwkPaddleFrame};

const EDGE_PORT: u16 = 5555;

// Morse "ET" is one dah (T) then one dit (E): the simplest pattern that gives the speed estimator a
// clean dit to measure. Returned as (key_down, hold_units) pairs in dit units.
fn et_elements() -> Vec<(bool, u32)> {
    vec![
        (true, 3), // T: dah
        (false, 1),
        (true, 1), // E: dit
        (false, 3), // inter-character
    ]
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: rwk_keyer <relay host:port> <code> [wpm] [seconds]");
        std::process::exit(2);
    }
    let relay: std::net::SocketAddr = match args[1].parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bad relay address: {e}");
            std::process::exit(2);
        }
    };
    let code = args[2].clone();
    let wpm: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);
    let secs: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(12);
    let dit_ms = 1200 / wpm;

    let mut config = Config::new(Role::Client).expect("identity");
    config.relay = Some(RelayConfig { address: relay });
    config.pairing_code = Some(code.clone());

    println!("keyer: pairing through {relay} with code '{code}' ...");
    let connected = match tunnel::connect(&config).await {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL could not connect: {e}");
            std::process::exit(1);
        }
    };
    let path = connected.path;
    println!("keyer: connected via {}", match path {
        Path::Direct => "DIRECT",
        Path::Relay => "RELAY",
    });

    let session = Session::start(connected, &config).expect("session");
    let net = session.handle();
    let conn = net.open_udp(EDGE_PORT).await.expect("edge udp");
    let edge = EdgeTransport::over_tunnel(conn);
    // The Station (the ESP) answers at its inner address, edge port.
    let peer = std::net::SocketAddr::new(
        std::net::IpAddr::V4(Role::Client.peer_inner_address()),
        EDGE_PORT,
    );
    edge.set_peer(peer).await;

    // A heartbeat+ping beacon, so the ESP sees us as alive and we can observe its PONGs through the
    // transport's RTT. This mirrors the desktop watch_edge_liveness loop.
    let hb = std::sync::Arc::new(edge);
    {
        let hb = hb.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            loop {
                tick.tick().await;
                let _ = hb.send_heartbeat().await;
                let _ = hb.send_ping().await;
            }
        });
    }
    // Drain inbound so pongs are processed and RTT is learned.
    {
        let rx = hb.clone();
        tokio::spawn(async move {
            loop {
                let _ = rx.recv_edge().await;
            }
        });
    }

    // Build and send CW frames on a dit-unit grid. Each frame carries the current edge plus up to
    // three previous ones (newest first) for redundancy, exactly as the wire format and the real
    // EdgeSender do.
    let epoch = 1u16;
    let mut seq: u32 = 0;
    let mut history: Vec<EdgeEntry> = Vec::new();
    let start = std::time::Instant::now();
    let mut t_ms: u32 = 100;
    let mut sent = 0u32;
    let mut saw_rtt = false;

    while start.elapsed() < Duration::from_secs(secs) {
        for (down, units) in et_elements() {
            seq += 1;
            let entry = if down {
                EdgeEntry::key_down_at(seq, t_ms, 0)
            } else {
                EdgeEntry::key_up_at(seq, t_ms, 0)
            };
            history.insert(0, entry);
            history.truncate(4);
            if let Some(frame) = RwkPaddleFrame::try_new(epoch, &history) {
                if hb.send_frame(&frame).await.is_ok() {
                    sent += 1;
                }
            }
            t_ms += (units as u32) * dit_ms as u32;
            tokio::time::sleep(Duration::from_millis((units as u64) * dit_ms)).await;
        }
        if let Some(rtt) = hb.rtt() {
            if !saw_rtt {
                println!("keyer: first RTT {} ms", rtt.as_millis());
                saw_rtt = true;
            }
        }
    }

    let rtt = hb.rtt().map(|d| d.as_millis()).unwrap_or(0);
    println!("keyer: sent {sent} frames, final RTT {rtt} ms");
    if sent > 0 && (saw_rtt || rtt > 0) {
        println!("PASS keyer kept a {} session and heard the Station's pongs", match path {
            Path::Direct => "direct",
            Path::Relay => "relayed",
        });
    } else if sent > 0 {
        println!("PARTIAL frames sent but no pong seen (one-way path?)");
    } else {
        println!("FAIL no frames sent");
    }
}
