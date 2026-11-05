//! boringtun (the WireGuard implementation rwk-core uses) as an independent peer for the C core.
//!
//!   bt_interop initiator <sk_hex> <peer_pk_hex> <listen_port> <peer_port>
//!   bt_interop responder <sk_hex> <peer_pk_hex> <listen_port>
//!   bt_interop pubkey    <sk_hex>
//!
//! Test tooling only. Mirrors how rwk-core drives a Tunn: decapsulate, then call again with an
//! empty datagram until Done, and run update_timers on a short tick.
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

fn unhex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex");
    }
    out
}

/// IPv4 header checksum.
fn ip_checksum(h: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for c in h.chunks(2) {
        sum += u32::from(c[0]) << 8 | u32::from(*c.get(1).unwrap_or(&0));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// 10.6.0.1:5555 -> 10.6.0.2:5555 UDP, checksum 0 (legal for IPv4), like the desktop Client.
fn client_packet(payload: &[u8]) -> Vec<u8> {
    let total = 20 + 8 + payload.len();
    let mut p = vec![0u8; total];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[6] = 0x40;
    p[8] = 64;
    p[9] = 17;
    p[12..16].copy_from_slice(&[10, 6, 0, 1]);
    p[16..20].copy_from_slice(&[10, 6, 0, 2]);
    let c = ip_checksum(&p[..20]);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    p[20..22].copy_from_slice(&5555u16.to_be_bytes());
    p[22..24].copy_from_slice(&5555u16.to_be_bytes());
    p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload);
    p
}

/// Swaps addresses and ports; the one's complement sums are unchanged by a swap.
fn echo_packet(pkt: &[u8]) -> Vec<u8> {
    let mut e = pkt.to_vec();
    let (a, b) = (e[12..16].to_vec(), e[16..20].to_vec());
    e[12..16].copy_from_slice(&b);
    e[16..20].copy_from_slice(&a);
    let (sp, dp) = (e[20..22].to_vec(), e[22..24].to_vec());
    e[20..22].copy_from_slice(&dp);
    e[22..24].copy_from_slice(&sp);
    e
}

struct Peer {
    tunn: Tunn,
    sock: UdpSocket,
    dest: Option<SocketAddr>,
}

impl Peer {
    fn send(&self, data: &[u8]) {
        if let Some(d) = self.dest {
            let _ = self.sock.send_to(data, d);
        }
    }

    /// Feeds one datagram and returns every inner IPv4 packet it produced.
    fn feed(&mut self, src: SocketAddr, data: &[u8]) -> Vec<Vec<u8>> {
        let mut inner = Vec::new();
        let mut dst = vec![0u8; 2048];
        let mut first = true;
        loop {
            let result = if first {
                self.tunn.decapsulate(Some(src.ip()), data, &mut dst)
            } else {
                self.tunn.decapsulate(None, &[], &mut dst)
            };
            first = false;
            match result {
                TunnResult::Done => break,
                TunnResult::Err(_) => break,
                TunnResult::WriteToNetwork(b) => {
                    let b = b.to_vec();
                    self.dest = Some(src);
                    self.send(&b);
                }
                TunnResult::WriteToTunnelV4(b, _) => inner.push(b.to_vec()),
                TunnResult::WriteToTunnelV6(_, _) => {}
            }
        }
        inner
    }

    fn tick(&mut self) {
        let mut dst = vec![0u8; 2048];
        if let TunnResult::WriteToNetwork(b) = self.tunn.update_timers(&mut dst) {
            let b = b.to_vec();
            self.send(&b);
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage");
        std::process::exit(2);
    }
    let sk = unhex32(&a[2]);
    if a[1] == "pubkey" {
        let pk = PublicKey::from(&StaticSecret::from(sk));
        println!("{}", pk.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>());
        return;
    }
    let initiator = a[1] == "initiator";
    let peer_pk = unhex32(&a[3]);
    let listen: u16 = a[4].parse().unwrap();
    let sock = UdpSocket::bind(("127.0.0.1", listen)).expect("bind");
    sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let dest = if initiator {
        Some(SocketAddr::from(([127, 0, 0, 1], a[5].parse::<u16>().unwrap())))
    } else {
        None
    };
    let tunn = Tunn::new(StaticSecret::from(sk), PublicKey::from(peer_pk), None, None, 7, None);
    let mut peer = Peer { tunn, sock, dest };

    println!("ready");
    let start = Instant::now();
    let mut echoed = 0;
    let mut last_send = Instant::now() - Duration::from_secs(1);
    let mut last_init = Instant::now() - Duration::from_secs(1);
    let mut buf = vec![0u8; 2048];
    while start.elapsed() < Duration::from_secs(15) {
        peer.tick();
        if initiator {
            let established = peer.tunn.time_since_last_handshake().is_some();
            if !established && last_init.elapsed() > Duration::from_millis(250) {
                let mut dst = vec![0u8; 2048];
                if let TunnResult::WriteToNetwork(b) = peer.tunn.format_handshake_initiation(&mut dst, false) {
                    let b = b.to_vec();
                    peer.send(&b);
                }
                last_init = Instant::now();
            }
            if established && last_send.elapsed() > Duration::from_millis(200) {
                // Edge-shaped payload: a one-entry RWK-PADDLE frame (epoch 1, count 1, key down).
                let frame = [1u8, 0, 1, 0, 1, 0, 0, 0, 0x10, 0, 0, 0, 1, 0, 0, 0];
                let pkt = client_packet(&frame);
                let mut dst = vec![0u8; 2048];
                if let TunnResult::WriteToNetwork(b) = peer.tunn.encapsulate(&pkt, &mut dst) {
                    let b = b.to_vec();
                    peer.send(&b);
                }
                last_send = Instant::now();
            }
        }
        let (n, src) = match peer.sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let data = buf[..n].to_vec();
        for pkt in peer.feed(src, &data) {
            if initiator {
                // The echo comes back from the station address with our payload intact.
                if pkt.len() >= 28 && pkt[12..16] == [10, 6, 0, 2] && pkt[28..] == [1u8, 0, 1, 0, 1, 0, 0, 0, 0x10, 0, 0, 0, 1, 0, 0, 0] {
                    println!("PASS initiator got echo from the C core");
                    return;
                }
            } else {
                let e = echo_packet(&pkt);
                let mut dst = vec![0u8; 2048];
                if let TunnResult::WriteToNetwork(b) = peer.tunn.encapsulate(&e, &mut dst) {
                    let b = b.to_vec();
                    peer.send(&b);
                    echoed += 1;
                }
            }
        }
        if !initiator && echoed >= 1 {
            println!("PASS responder echoed {echoed} packets");
            return;
        }
    }
    println!("FAIL timeout");
    std::process::exit(1);
}
