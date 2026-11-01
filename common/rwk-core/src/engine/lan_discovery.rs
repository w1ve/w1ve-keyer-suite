//! LAN Player discovery for relay-free direct pairing.
//!
//! Version 2 reverses the earlier prototype topology: radio-site Players advertise themselves;
//! operating-position Keyers browse and explicitly select a Player. WAN relay pairing is wholly
//! separate from this module.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::Result;

/// UDP port for desktop Player advertisements.
pub const DISCOVERY_PORT: u16 = 4993;
/// Versioned Player-advertisement format.
pub const DISCOVERY_VERSION: u8 = 2;
/// Player supports the focused CW keying session.
pub const CAPABILITY_CW: u8 = 1;
/// Player supports the Router control/forwarding session in addition to CW.
pub const CAPABILITY_ROUTER: u8 = 2;
/// How long a Player remains visible without another advertisement.
pub const PLAYER_TTL: Duration = Duration::from_secs(5);
/// Periodic Player advertisement interval.
pub const ADVERTISE_INTERVAL: Duration = Duration::from_millis(1500);

const MAGIC: [u8; 4] = *b"RWKP";

/// A desktop radio-site Player's LAN advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// Human-facing Player name, conventionally a station callsign.
    pub callsign: String,
    /// Optional Player label, such as `20m`.
    pub keyer_id: String,
    /// Player WireGuard public key.
    pub public_key: [u8; 32],
    /// Player WireGuard listener port.
    pub wg_port: u16,
    /// Capability bits, including [`CAPABILITY_CW`] and optionally [`CAPABILITY_ROUTER`].
    pub capabilities: u8,
}

impl Beacon {
    /// The Player label shown to an operating Keyer.
    #[must_use]
    pub fn display_name(&self) -> String {
        match (self.callsign.trim(), self.keyer_id.trim()) {
            ("", "") => "(unnamed Player)".to_string(),
            (name, "") => name.to_string(),
            ("", label) => label.to_string(),
            (name, label) => format!("{name}-{label}"),
        }
    }

    /// Encodes `RWKP | version | capabilities | name | label | public_key | wg_port`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let name = utf8_prefix(&self.callsign, 32).as_bytes();
        let label = utf8_prefix(&self.keyer_id, 32).as_bytes();
        let name_len = name.len();
        let label_len = label.len();
        let mut out = Vec::with_capacity(4 + 1 + 1 + 1 + name_len + 1 + label_len + 32 + 2);
        out.extend_from_slice(&MAGIC);
        out.push(DISCOVERY_VERSION);
        out.push(self.capabilities);
        out.push(name_len as u8);
        out.extend_from_slice(&name[..name_len]);
        out.push(label_len as u8);
        out.extend_from_slice(&label[..label_len]);
        out.extend_from_slice(&self.public_key);
        out.extend_from_slice(&self.wg_port.to_be_bytes());
        out
    }

    /// Strictly decodes a v2 Player advertisement. Old role-reversed RWKB beacons are ignored.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 4 + 1 + 1 + 1 + 1 + 34
            || bytes[..4] != MAGIC
            || bytes[4] != DISCOVERY_VERSION
        {
            return None;
        }
        let capabilities = bytes[5];
        let mut pos = 6;
        let callsign = take_string(bytes, &mut pos)?;
        let keyer_id = take_string(bytes, &mut pos)?;
        if bytes.len() != pos + 34 {
            return None;
        }
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&bytes[pos..pos + 32]);
        let wg_port = u16::from_be_bytes([bytes[pos + 32], bytes[pos + 33]]);
        if wg_port == 0 || capabilities & CAPABILITY_CW == 0 {
            return None;
        }
        Some(Self { callsign, keyer_id, public_key, wg_port, capabilities })
    }
}

fn take_string(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let len = *bytes.get(*pos)? as usize;
    if len > 32 {
        return None;
    }
    *pos += 1;
    let end = *pos + len;
    let value = std::str::from_utf8(bytes.get(*pos..end)?).ok()?.to_string();
    *pos = end;
    Some(value)
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn primary_lan_address(port: u16) -> SocketAddr {
    let fallback = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let Ok(probe) = std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)) else {
        return fallback;
    };
    if probe
        .connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 80))
        .is_err()
    {
        return fallback;
    }
    match probe.local_addr() {
        Ok(SocketAddr::V4(address)) if !address.ip().is_unspecified() && !address.ip().is_loopback() => {
            SocketAddr::new(IpAddr::V4(*address.ip()), port)
        }
        _ => fallback,
    }
}

fn bind_reuse_broadcast(port: u16) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(crate::Error::from)?;
    socket.set_reuse_address(true).map_err(crate::Error::from)?;
    #[cfg(unix)]
    {
        let _ = socket.set_reuse_port(true);
    }
    socket.set_broadcast(true).map_err(crate::Error::from)?;
    socket.set_nonblocking(true).map_err(crate::Error::from)?;
    let address = primary_lan_address(port);
    socket.bind(&address.into()).map_err(crate::Error::from)?;
    UdpSocket::from_std(socket.into()).map_err(crate::Error::from)
}

/// Broadcasts a Player advertisement while the radio-site Player is available.
pub struct LanAdvertiser {
    socket: UdpSocket,
    beacon: Beacon,
}

impl LanAdvertiser {
    /// Creates a Player advertiser on the primary LAN address.
    pub fn new(beacon: Beacon) -> Result<Self> {
        Ok(Self { socket: bind_reuse_broadcast(0)?, beacon })
    }

    /// Broadcasts one Player advertisement.
    pub async fn announce_once(&self) -> Result<()> {
        let destination = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT));
        self.socket.send_to(&self.beacon.encode(), destination).await.map_err(crate::Error::from)?;
        Ok(())
    }

    /// Advertises until the task is cancelled/dropped.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(ADVERTISE_INTERVAL);
        loop {
            ticker.tick().await;
            let _ = self.announce_once().await;
        }
    }
}

/// A Player the operating Keyer has heard on the LAN.
#[derive(Debug, Clone)]
pub struct LanPeer {
    /// Stable WireGuard public key for selecting this Player.
    pub public_key: [u8; 32],
    /// Player callsign supplied by its current advertisement.
    pub callsign: String,
    /// Optional Player label supplied by its current advertisement.
    pub keyer_id: String,
    /// Source IP plus currently advertised WireGuard listener port.
    pub endpoint: SocketAddr,
    /// Player capability bits from the current advertisement.
    pub capabilities: u8,
    last_seen: Instant,
}

impl LanPeer {
    /// Builds the display name from the advertised callsign and optional label.
    #[must_use]
    pub fn display_name(&self) -> String {
        Beacon {
            callsign: self.callsign.clone(),
            keyer_id: self.keyer_id.clone(),
            public_key: self.public_key,
            wg_port: self.endpoint.port(),
            capabilities: self.capabilities,
        }
        .display_name()
    }

    /// Whether this Player permits the Router control and forwarding path.
    #[must_use]
    pub fn supports_router(&self) -> bool {
        self.capabilities & CAPABILITY_ROUTER != 0
    }
}

/// Browses LAN Player advertisements for an operating Keyer.
pub struct LanBrowser {
    socket: UdpSocket,
    self_key: [u8; 32],
    peers: HashMap<[u8; 32], LanPeer>,
}

impl LanBrowser {
    /// Binds a Player browser that ignores advertisements from `self_key`.
    pub fn bind(self_key: [u8; 32]) -> Result<Self> {
        Ok(Self { socket: bind_reuse_broadcast(DISCOVERY_PORT)?, self_key, peers: HashMap::new() })
    }

    /// The IPv4 address selected for this Player browser's local listener.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket.local_addr().map_err(crate::Error::from)
    }

    /// Receives a Player advertisement. Self echo and malformed/old advertisements are ignored.
    pub async fn recv_once(&mut self) -> Result<bool> {
        let mut buf = [0u8; 256];
        let (length, source) = self.socket.recv_from(&mut buf).await.map_err(crate::Error::from)?;
        let Some(beacon) = Beacon::decode(&buf[..length]) else { return Ok(false) };
        if beacon.public_key == self.self_key {
            return Ok(false);
        }
        let endpoint = SocketAddr::new(source.ip(), beacon.wg_port);
        self.peers.insert(
            beacon.public_key,
            LanPeer {
                public_key: beacon.public_key,
                callsign: beacon.callsign,
                keyer_id: beacon.keyer_id,
                endpoint,
                capabilities: beacon.capabilities,
                last_seen: Instant::now(),
            },
        );
        Ok(true)
    }

    /// Returns current, TTL-pruned Player records sorted by display name.
    #[must_use]
    pub fn peers(&mut self) -> Vec<LanPeer> {
        prune_and_sort(&mut self.peers, Instant::now())
    }

    /// Resolves the current endpoint for a Player identity from a fresh advertisement.
    pub async fn resolve(self_key: [u8; 32], key: [u8; 32], timeout: Duration) -> Result<SocketAddr> {
        let mut browser = Self::bind(self_key)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(crate::Error::Config("the selected Player is no longer advertising on the LAN".into()));
            }
            match tokio::time::timeout(remaining, browser.recv_once()).await {
                Ok(Ok(_)) => {
                    if let Some(peer) = browser.peers().into_iter().find(|peer| peer.public_key == key) {
                        return Ok(peer.endpoint);
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(crate::Error::Config("the selected Player is no longer advertising on the LAN".into())),
            }
        }
    }
}

fn prune_and_sort(peers: &mut HashMap<[u8; 32], LanPeer>, now: Instant) -> Vec<LanPeer> {
    peers.retain(|_, peer| now.duration_since(peer.last_seen) < PLAYER_TTL);
    let mut list: Vec<_> = peers.values().cloned().collect();
    list.sort_by(|left, right| left.display_name().cmp(&right.display_name()));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon() -> Beacon {
        Beacon {
            callsign: "W1VE".into(),
            keyer_id: "20m".into(),
            public_key: [7u8; 32],
            wg_port: 51820,
            capabilities: CAPABILITY_CW,
        }
    }

    #[test]
    fn player_name_composes_name_and_label() {
        assert_eq!(beacon().display_name(), "W1VE-20m");
        assert_eq!(Beacon { keyer_id: String::new(), ..beacon() }.display_name(), "W1VE");
    }

    #[test]
    fn v2_player_beacon_round_trips_strictly() {
        let record = beacon();
        assert_eq!(Beacon::decode(&record.encode()), Some(record));
        let mut trailing = beacon().encode();
        trailing.push(0);
        assert_eq!(Beacon::decode(&trailing), None);
        assert_eq!(Beacon::decode(b"RWKB\x01"), None);
    }

    #[test]
    fn capability_reports_router_support() {
        let mut peer = LanPeer {
            public_key: [1u8; 32], callsign: "W1VE".into(), keyer_id: String::new(),
            endpoint: "127.0.0.1:1".parse().unwrap(), capabilities: CAPABILITY_CW,
            last_seen: Instant::now(),
        };
        assert!(!peer.supports_router());
        peer.capabilities |= CAPABILITY_ROUTER;
        assert!(peer.supports_router());
    }

    #[test]
    fn player_beacon_clamps_multibyte_labels_on_character_boundaries() {
        let beacon = Beacon {
            callsign: format!("{}é", "A".repeat(31)),
            keyer_id: format!("{}🚀", "B".repeat(29)),
            ..beacon()
        };
        let decoded = Beacon::decode(&beacon.encode()).expect("UTF-8-safe Player beacon");
        assert_eq!(decoded.callsign, "A".repeat(31));
        assert_eq!(decoded.keyer_id, "B".repeat(29));
        assert!(decoded.callsign.is_char_boundary(decoded.callsign.len()));
        assert!(decoded.keyer_id.is_char_boundary(decoded.keyer_id.len()));
    }
}
