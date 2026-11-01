//! W1VE v2 transport for an ESP32 radio-site Player.
//!
//! In LAN mode Players advertise their availability and operating-position Keyers browse the
//! list, then explicitly select one. W1VE is a trusted-LAN transport with no pairing PIN. CW
//! always flows from the selected Keyer to the ESP Player.
//!
//! W1VE v2 deliberately separates the reliable inner `RWK-PADDLE` edge stream from its UDP
//! envelope. The envelope accepts a bounded reordering window; the inner edge epoch/sequence
//! rules remain the authority for replaying key transitions. This prevents concurrent control
//! packets such as PING/PONG from invalidating valid CW frames.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;
use tokio::sync::Mutex as AsyncMutex;

use crate::protocol::edge::RwkPaddleFrame;
use crate::Result;

/// UDP discovery port for W1VE ESP32 Player advertisements and bounded Keyer discovery queries.
pub const ESP_PLAYER_DISCOVERY_PORT: u16 = 4994;
/// UDP port on which an ESP Player receives pairing and authenticated session traffic.
pub const ESP_PLAYER_TRANSPORT_PORT: u16 = 4995;
/// Player-advertising W1VE protocol version.
pub const ESP_PLAYER_PROTOCOL_VERSION: u8 = 2;
/// Interval for out-of-band PING/PONG supervision in each direction.
pub const ESP_PLAYER_PING_INTERVAL: Duration = Duration::from_secs(1);
/// Number of consecutive missing matching PONGs that ends a W1VE session.
pub const ESP_PLAYER_MISSED_PING_LIMIT: u8 = 3;
/// Maximum Player records retained by a desktop browser.
pub const ESP_PLAYER_TTL: Duration = Duration::from_secs(5);
/// Maximum local hosts probed by the unicast fallback when Wi-Fi suppresses broadcast.
pub const ESP_PLAYER_DISCOVERY_QUERY_LIMIT: u32 = 254;

const MAGIC: [u8; 4] = *b"W1VE";
const TYPE_PLAYER_ADVERTISE: u8 = 1;
const TYPE_PAIR_REQUEST: u8 = 2;
const TYPE_PAIR_ACCEPT: u8 = 3;
const TYPE_EDGE: u8 = 4;
const TYPE_PING: u8 = 6;
const TYPE_PONG: u8 = 7;
const TYPE_UNPAIR: u8 = 8;
const TYPE_DISCOVERY_QUERY: u8 = 10;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 21;
const PAIR_REQUEST_LEN: usize = 4 + 1 + 1 + 6 + 6 + 16;
const PAIR_ACCEPT_LEN: usize = 4 + 1 + 1 + 6 + 6 + 16 + 16;
const DISCOVERY_QUERY_LEN: usize = 4 + 1 + 1 + 6;
const RX_WINDOW: u32 = 64;

/// An ESP Player currently advertising on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspPlayerAdvertisement {
    /// Factory Wi-Fi MAC address used as the stable Player identity.
    pub player_id: [u8; 6],
    /// Player-configured human display name.
    pub name: String,
    /// Authoritative pairing endpoint: advertisement source IP plus advertised transport port.
    pub endpoint: SocketAddr,
    /// Player flags. Bit 0 means the Player is already paired/busy.
    pub flags: u8,
    last_seen: Instant,
}

impl EspPlayerAdvertisement {
    /// Recreates a browser selection at the Tauri command boundary.
    #[must_use]
    pub fn from_parts(player_id: [u8; 6], name: String, endpoint: SocketAddr, flags: u8) -> Self {
        Self {
            player_id,
            name: clamp_name(name),
            endpoint,
            flags,
            last_seen: Instant::now(),
        }
    }

    /// Stable upper-case hexadecimal identity for UI selection.
    #[must_use]
    pub fn id_hex(&self) -> String {
        self.player_id.iter().map(|byte| format!("{byte:02X}")).collect()
    }

    /// Whether this Player reports an active session.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// Collects ESP Player advertisements and sends bounded unicast discovery queries when a Wi-Fi
/// access point suppresses broadcasts between radio bands.
pub struct EspPlayerBrowser {
    socket: Arc<UdpSocket>,
    keyer_id: [u8; 6],
    players: HashMap<[u8; 6], EspPlayerAdvertisement>,
    query_cursor: Option<(u32, u32, u32)>,
}

impl EspPlayerBrowser {
    /// Binds the W1VE discovery port on the primary LAN IPv4 address.
    pub async fn bind(keyer_id: [u8; 6]) -> Result<Self> {
        let SocketAddr::V4(address) = primary_lan_address(ESP_PLAYER_DISCOVERY_PORT) else {
            unreachable!("primary LAN helper always selects IPv4")
        };
        Self::bind_on(keyer_id, *address.ip()).await
    }

    /// Binds the W1VE discovery port to a caller-selected LAN IPv4 address. A focused Keyer
    /// uses the same address as its desktop Player browser so its type-10 fallback queries reach
    /// station-subnet Players even when another adapter has the Internet default route.
    pub async fn bind_on(keyer_id: [u8; 6], address: Ipv4Addr) -> Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(address),
            ESP_PLAYER_DISCOVERY_PORT,
        ))
        .await?;
        socket.set_broadcast(true)?;
        Ok(Self {
            socket: Arc::new(socket),
            keyer_id,
            players: HashMap::new(),
            query_cursor: None,
        })
    }

    /// Receives and folds one Player advertisement into the current list.
    pub async fn recv_once(&mut self) -> Result<bool> {
        let mut bytes = [0u8; 128];
        let (length, source) = match self.socket.recv_from(&mut bytes).await {
            Ok(received) => received,
            // Windows reports an ICMP "port unreachable" from any empty host in the bounded
            // /24 query pass as WSAECONNRESET (10054) on recv_from. That is an expected negative
            // discovery result, not a failed browser; keep this socket and continue the sweep.
            Err(error) if transient_discovery_receive_error(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let Some((player_id, port, name, flags)) = decode_player_advertisement(&bytes[..length]) else {
            return Ok(false);
        };
        self.players.insert(
            player_id,
            EspPlayerAdvertisement {
                player_id,
                name,
                endpoint: SocketAddr::new(source.ip(), port),
                flags,
                last_seen: Instant::now(),
            },
        );
        Ok(true)
    }

    /// Starts one bounded `/24` unicast discovery pass from the browser's primary LAN address.
    /// A caller drives it by awaiting [`Self::send_next_discovery_query`] at a controlled rate.
    pub fn request_discovery(&mut self) {
        let Ok(SocketAddr::V4(address)) = self.socket.local_addr() else {
            return;
        };
        let host = u32::from(*address.ip());
        if host == 0 || address.ip().is_loopback() {
            return;
        }
        let subnet = host & 0xffff_ff00;
        self.query_cursor = Some((subnet + 1, subnet + ESP_PLAYER_DISCOVERY_QUERY_LIMIT, host));
    }

    /// Sends one type-10 query to the next local candidate. Returns `true` while candidates
    /// remain. This never auto-pairs; it merely asks an advertising Player to unicast type 1.
    pub async fn send_next_discovery_query(&mut self) -> Result<bool> {
        let Some((cursor, last, self_ip)) = self.query_cursor else {
            return Ok(false);
        };
        let next = cursor.saturating_add(1);
        self.query_cursor = if next > last { None } else { Some((next, last, self_ip)) };
        if cursor == self_ip {
            return Ok(self.query_cursor.is_some());
        }
        let mut query = [0u8; DISCOVERY_QUERY_LEN];
        query[..4].copy_from_slice(&MAGIC);
        query[4] = ESP_PLAYER_PROTOCOL_VERSION;
        query[5] = TYPE_DISCOVERY_QUERY;
        query[6..12].copy_from_slice(&self.keyer_id);
        let destination = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(cursor)), ESP_PLAYER_DISCOVERY_PORT);
        self.socket.send_to(&query, destination).await?;
        Ok(self.query_cursor.is_some())
    }

    /// Returns fresh Player advertisements sorted by display name and stable ID.
    #[must_use]
    pub fn players(&mut self) -> Vec<EspPlayerAdvertisement> {
        let now = Instant::now();
        self.players.retain(|_, player| now.duration_since(player.last_seen) < ESP_PLAYER_TTL);
        let mut players: Vec<_> = self.players.values().cloned().collect();
        players.sort_by(|left, right| left.name.cmp(&right.name).then(left.player_id.cmp(&right.player_id)));
        players
    }
}

/// A packet received from a paired ESP Player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EspPlayerInbound {
    /// Ping/Pong or other valid non-CW control traffic.
    Control,
    /// The Player explicitly cleared the active pairing.
    Unpaired,
}

/// Authenticated session with an explicitly selected ESP Player.
pub struct EspPlayerSession {
    socket: Arc<UdpSocket>,
    player: EspPlayerAdvertisement,
    session_key: [u8; 32],
    session_id: u32,
    /// Serializes packet construction and UDP submission across edge, refresh, ping, pong, and
    /// unpair producers. Sequence numbers are allocated immediately before serialized send.
    tx_sequence: AsyncMutex<u32>,
    /// Set before a 32-bit sender wrap. Callers must tear down and pair again rather than send
    /// a sequence that the peer's ordinary replay window would classify as stale.
    sequence_exhausted: AtomicBool,
    /// Bounded anti-replay window that permits normal UDP reordering.
    rx_window: Mutex<SequenceWindow>,
    last_rx_nanos: AtomicU64,
    last_ping_stamp: AtomicU64,
    last_pong_stamp: AtomicU64,
    rtt_nanos: AtomicU64,
}

impl EspPlayerSession {
    /// Initiates pairing with the explicitly selected Player.
    pub async fn pair(player: EspPlayerAdvertisement, keyer_id: [u8; 6]) -> Result<Self> {
        // Bind using the route to this Player, not the route to a public Internet probe. A VPN or
        // virtual adapter may own the latter even when the Player is on a different local LAN.
        let socket = Arc::new(UdpSocket::bind(local_address_for_peer(player.endpoint, 0)).await?);
        let mut initiator_nonce = [0u8; 16];
        getrandom::fill(&mut initiator_nonce)
            .map_err(|error| crate::Error::Config(format!("could not generate ESP pairing nonce: {error}")))?;
        let request = pair_request(&player.player_id, &keyer_id, &initiator_nonce);

        for _ in 0..3 {
            socket.send_to(&request, player.endpoint).await?;
            let mut response = [0u8; 96];
            match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut response)).await {
                Ok(Ok((length, source))) if source == player.endpoint => {
                    if let Some(responder_nonce) =
                        parse_pair_accept(&response[..length], &player.player_id, &keyer_id, &initiator_nonce)
                    {
                        return Ok(Self::new(socket, player, keyer_id, initiator_nonce, responder_nonce));
                    }
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
            }
        }
        Err(crate::Error::Config("selected ESP Player did not accept pairing".into()))
    }

    fn new(
        socket: Arc<UdpSocket>,
        player: EspPlayerAdvertisement,
        keyer_id: [u8; 6],
        initiator_nonce: [u8; 16],
        responder_nonce: [u8; 16],
    ) -> Self {
        let session_key = session_key(&player.player_id, &keyer_id, &initiator_nonce, &responder_nonce);
        let session_id = u32::from_be_bytes(session_key[..4].try_into().expect("four-byte slice"));
        Self {
            socket,
            player,
            session_key,
            session_id,
            tx_sequence: AsyncMutex::new(0),
            sequence_exhausted: AtomicBool::new(false),
            rx_window: Mutex::new(SequenceWindow::default()),
            last_rx_nanos: AtomicU64::new(monotonic_nanos()),
            last_ping_stamp: AtomicU64::new(0),
            last_pong_stamp: AtomicU64::new(0),
            rtt_nanos: AtomicU64::new(0),
        }
    }

    /// The selected Player record, including its pinned endpoint.
    #[must_use]
    pub fn player(&self) -> &EspPlayerAdvertisement {
        &self.player
    }

    /// Whether this session reached its non-wrapping envelope sequence limit and must be paired
    /// again. This is terminal for the current session because a wrapped sequence is replay-old.
    #[must_use]
    pub fn sequence_exhausted(&self) -> bool {
        self.sequence_exhausted.load(Ordering::Relaxed)
    }

    /// Sends one complete existing RWK-PADDLE frame. The ESP inner edge replayer handles
    /// redundant edges, order, and state safely.
    pub async fn send_frame(&self, frame: &RwkPaddleFrame) -> Result<usize> {
        self.send_packet(TYPE_EDGE, &frame.to_vec()).await
    }

    /// Sends an out-of-band ping and returns the stamp that its matching PONG must echo.
    pub async fn send_ping(&self) -> Result<u64> {
        let stamp = monotonic_nanos().max(1);
        self.send_packet(TYPE_PING, &stamp.to_be_bytes()).await?;
        self.last_ping_stamp.store(stamp, Ordering::Relaxed);
        Ok(stamp)
    }

    /// Sends a safe terminal unpair request.
    pub async fn send_unpair(&self) -> Result<usize> {
        self.send_packet(TYPE_UNPAIR, &[]).await
    }

    /// Receives one endpoint-pinned, HMAC-verified session packet. The envelope permits bounded
    /// reordering; only a duplicate or packet outside its replay window is ignored.
    pub async fn recv(&self) -> Result<Option<EspPlayerInbound>> {
        let mut packet = [0u8; 128];
        let (length, source) = self.socket.recv_from(&mut packet).await?;
        if source != self.player.endpoint {
            return Ok(None);
        }
        let Some((packet_type, sequence, payload)) = self.parse_session_packet(&packet[..length]) else {
            return Ok(None);
        };
        let valid = ((packet_type == TYPE_PING || packet_type == TYPE_PONG) && payload.len() == 8)
            || (packet_type == TYPE_UNPAIR && payload.is_empty());
        if !valid {
            return Ok(None);
        }
        if !self.rx_window.lock().unwrap().accept(sequence) {
            return Ok(None);
        }
        self.last_rx_nanos.store(monotonic_nanos(), Ordering::Relaxed);
        match packet_type {
            TYPE_PING => {
                let _ = self.send_packet(TYPE_PONG, payload).await;
                Ok(Some(EspPlayerInbound::Control))
            }
            TYPE_PONG => {
                let stamp = u64::from_be_bytes(payload.try_into().expect("eight-byte payload"));
                if stamp == 0 || stamp != self.last_ping_stamp.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                let now = monotonic_nanos();
                self.last_pong_stamp.store(stamp, Ordering::Relaxed);
                self.rtt_nanos.store(now.saturating_sub(stamp).max(1), Ordering::Relaxed);
                Ok(Some(EspPlayerInbound::Control))
            }
            TYPE_UNPAIR => Ok(Some(EspPlayerInbound::Unpaired)),
            _ => unreachable!("validated W1VE v2 session type"),
        }
    }

    /// Whether `stamp` has a matching Player PONG.
    #[must_use]
    pub fn ping_was_acknowledged(&self, stamp: u64) -> bool {
        stamp != 0 && self.last_pong_stamp.load(Ordering::Relaxed) == stamp
    }

    /// Latest matching ping round-trip time.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        match self.rtt_nanos.load(Ordering::Relaxed) {
            0 => None,
            nanos => Some(Duration::from_nanos(nanos)),
        }
    }

    /// Duration since any valid endpoint-pinned session packet arrived.
    #[must_use]
    pub fn peer_silent_for(&self) -> Duration {
        Duration::from_nanos(monotonic_nanos().saturating_sub(self.last_rx_nanos.load(Ordering::Relaxed)))
    }

    async fn send_packet(&self, packet_type: u8, payload: &[u8]) -> Result<usize> {
        if payload.len() > 64 {
            return Err(crate::Error::Config("ESP packet payload exceeds 64 bytes".into()));
        }
        let mut sequence = self.tx_sequence.lock().await;
        // Reserve `u32::MAX` rather than wrapping to 1. The receiver intentionally uses an
        // ordinary bounded replay window, so a wrapped packet would look 2^32 packets old.
        if *sequence >= u32::MAX - 1 {
            self.sequence_exhausted.store(true, Ordering::Relaxed);
            return Err(crate::Error::Config(
                "W1VE v2 envelope sequence exhausted; unpair and select the Player again".into(),
            ));
        }
        *sequence += 1;
        let mut packet = Vec::with_capacity(HEADER_LEN + payload.len() + TAG_LEN);
        packet.extend_from_slice(&MAGIC);
        packet.push(ESP_PLAYER_PROTOCOL_VERSION);
        packet.push(packet_type);
        packet.extend_from_slice(&self.player.player_id);
        packet.extend_from_slice(&self.session_id.to_be_bytes());
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.push(payload.len() as u8);
        packet.extend_from_slice(payload);
        let tag = hmac16(&self.session_key, &packet);
        packet.extend_from_slice(&tag);
        Ok(self.socket.send_to(&packet, self.player.endpoint).await?)
    }

    fn parse_session_packet<'a>(&self, packet: &'a [u8]) -> Option<(u8, u32, &'a [u8])> {
        if packet.len() < HEADER_LEN + TAG_LEN
            || packet[..4] != MAGIC
            || packet[4] != ESP_PLAYER_PROTOCOL_VERSION
            || packet[6..12] != self.player.player_id
            || u32::from_be_bytes(packet[12..16].try_into().ok()?) != self.session_id
        {
            return None;
        }
        let payload_len = packet[20] as usize;
        let unsigned_len = HEADER_LEN + payload_len;
        if packet.len() != unsigned_len + TAG_LEN {
            return None;
        }
        if hmac16(&self.session_key, &packet[..unsigned_len]) != packet[unsigned_len..] {
            return None;
        }
        Some((
            packet[5],
            u32::from_be_bytes(packet[16..20].try_into().ok()?),
            &packet[21..unsigned_len],
        ))
    }
}

#[derive(Default)]
struct SequenceWindow {
    initialized: bool,
    highest: u32,
    seen: u64,
}

impl SequenceWindow {
    fn accept(&mut self, sequence: u32) -> bool {
        if sequence == 0 {
            return false;
        }
        if !self.initialized {
            self.initialized = true;
            self.highest = sequence;
            self.seen = 1;
            return true;
        }
        if sequence > self.highest {
            let shift = sequence - self.highest;
            self.seen = if shift >= RX_WINDOW { 1 } else { (self.seen << shift) | 1 };
            self.highest = sequence;
            return true;
        }
        let distance = self.highest - sequence;
        if distance >= RX_WINDOW {
            return false;
        }
        let bit = 1u64 << distance;
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }
}

fn transient_discovery_receive_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ConnectionReset
}

fn local_address_for_peer(peer: SocketAddr, port: u16) -> SocketAddr {
    let fallback = primary_lan_address(port);
    let Ok(probe) = std::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)) else {
        return fallback;
    };
    if probe.connect(peer).is_err() {
        return fallback;
    }
    match probe.local_addr() {
        Ok(SocketAddr::V4(address)) if !address.ip().is_unspecified() => {
            SocketAddr::new(IpAddr::V4(*address.ip()), port)
        }
        _ => fallback,
    }
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
        Ok(SocketAddr::V4(address))
            if !address.ip().is_unspecified() && !address.ip().is_loopback() =>
        {
            SocketAddr::new(IpAddr::V4(*address.ip()), port)
        }
        _ => fallback,
    }
}

fn clamp_name(name: String) -> String {
    let mut result = String::new();
    for character in name.chars() {
        if result.len() + character.len_utf8() > 32 {
            break;
        }
        result.push(character);
    }
    if result.is_empty() { "(unnamed)".to_string() } else { result }
}

#[cfg(test)]
fn player_advertisement(player_id: &[u8; 6], port: u16, name: &str, flags: u8) -> Vec<u8> {
    let name = clamp_name(name.to_string());
    let mut packet = Vec::with_capacity(16 + name.len());
    packet.extend_from_slice(&MAGIC);
    packet.push(ESP_PLAYER_PROTOCOL_VERSION);
    packet.push(TYPE_PLAYER_ADVERTISE);
    packet.extend_from_slice(player_id);
    packet.extend_from_slice(&port.to_be_bytes());
    packet.push(name.len() as u8);
    packet.extend_from_slice(name.as_bytes());
    packet.push(flags);
    packet
}

fn decode_player_advertisement(packet: &[u8]) -> Option<([u8; 6], u16, String, u8)> {
    if packet.len() < 16
        || packet[..4] != MAGIC
        || packet[4] != ESP_PLAYER_PROTOCOL_VERSION
        || packet[5] != TYPE_PLAYER_ADVERTISE
    {
        return None;
    }
    let mut player_id = [0u8; 6];
    player_id.copy_from_slice(&packet[6..12]);
    let port = u16::from_be_bytes(packet[12..14].try_into().ok()?);
    let name_len = packet[14] as usize;
    if player_id == [0; 6] || port == 0 || name_len > 32 || packet.len() != 16 + name_len {
        return None;
    }
    let flags = packet[15 + name_len];
    if flags & !1 != 0 {
        return None;
    }
    let name = std::str::from_utf8(&packet[15..15 + name_len]).ok()?.to_string();
    Some((player_id, port, name, flags))
}

fn pair_request(player_id: &[u8; 6], keyer_id: &[u8; 6], initiator_nonce: &[u8; 16]) -> [u8; PAIR_REQUEST_LEN] {
    let mut packet = [0u8; PAIR_REQUEST_LEN];
    packet[..4].copy_from_slice(&MAGIC);
    packet[4] = ESP_PLAYER_PROTOCOL_VERSION;
    packet[5] = TYPE_PAIR_REQUEST;
    packet[6..12].copy_from_slice(player_id);
    packet[12..18].copy_from_slice(keyer_id);
    packet[18..34].copy_from_slice(initiator_nonce);
    packet
}

#[cfg(test)]
fn parse_pair_request(packet: &[u8], player_id: &[u8; 6]) -> Option<([u8; 6], [u8; 16])> {
    if packet.len() != PAIR_REQUEST_LEN
        || packet[..4] != MAGIC
        || packet[4] != ESP_PLAYER_PROTOCOL_VERSION
        || packet[5] != TYPE_PAIR_REQUEST
        || packet[6..12] != player_id[..]
    {
        return None;
    }
    let mut keyer_id = [0u8; 6];
    keyer_id.copy_from_slice(&packet[12..18]);
    if keyer_id == [0; 6] {
        return None;
    }
    let mut initiator_nonce = [0u8; 16];
    initiator_nonce.copy_from_slice(&packet[18..34]);
    Some((keyer_id, initiator_nonce))
}

#[cfg(test)]
fn pair_accept(
    player_id: &[u8; 6],
    keyer_id: &[u8; 6],
    initiator_nonce: &[u8; 16],
    responder_nonce: &[u8; 16],
) -> [u8; PAIR_ACCEPT_LEN] {
    let mut packet = [0u8; PAIR_ACCEPT_LEN];
    packet[..4].copy_from_slice(&MAGIC);
    packet[4] = ESP_PLAYER_PROTOCOL_VERSION;
    packet[5] = TYPE_PAIR_ACCEPT;
    packet[6..12].copy_from_slice(player_id);
    packet[12..18].copy_from_slice(keyer_id);
    packet[18..34].copy_from_slice(initiator_nonce);
    packet[34..50].copy_from_slice(responder_nonce);
    packet
}

fn parse_pair_accept(
    packet: &[u8],
    player_id: &[u8; 6],
    keyer_id: &[u8; 6],
    initiator_nonce: &[u8; 16],
) -> Option<[u8; 16]> {
    if packet.len() != PAIR_ACCEPT_LEN
        || packet[..4] != MAGIC
        || packet[4] != ESP_PLAYER_PROTOCOL_VERSION
        || packet[5] != TYPE_PAIR_ACCEPT
        || packet[6..12] != player_id[..]
        || packet[12..18] != keyer_id[..]
        || packet[18..34] != initiator_nonce[..]
    {
        return None;
    }
    packet[34..50].try_into().ok()
}

fn session_key(
    player_id: &[u8; 6],
    keyer_id: &[u8; 6],
    initiator_nonce: &[u8; 16],
    responder_nonce: &[u8; 16],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"W1VE-SESSION-v2");
    hasher.update(player_id);
    hasher.update(keyer_id);
    hasher.update(initiator_nonce);
    hasher.update(responder_nonce);
    hasher.finalize().into()
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut normalized = [0u8; 64];
    if key.len() > normalized.len() {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner = [0x36u8; 64];
    let mut outer = [0x5cu8; 64];
    for (index, byte) in normalized.iter().enumerate() {
        inner[index] ^= byte;
        outer[index] ^= byte;
    }
    let mut hasher = Sha256::new();
    hasher.update(inner);
    hasher.update(message);
    let inner_hash = hasher.finalize();
    let mut hasher = Sha256::new();
    hasher.update(outer);
    hasher.update(inner_hash);
    hasher.finalize().into()
}

fn hmac16(key: &[u8], message: &[u8]) -> [u8; 16] {
    hmac_sha256(key, message)[..16].try_into().expect("fixed slice")
}

fn monotonic_nanos() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::edge::EdgeEntry;

    #[test]
    fn player_advertisement_round_trips_and_filters_invalid_role_records() {
        let packet = player_advertisement(&[1, 2, 3, 4, 5, 6], 4995, "W1VE ESP32 Player", 0);
        let (id, port, name, flags) = decode_player_advertisement(&packet).expect("player advertisement");
        assert_eq!(id, [1, 2, 3, 4, 5, 6]);
        assert_eq!(port, 4995);
        assert_eq!(name, "W1VE ESP32 Player");
        assert_eq!(flags, 0);

        let mut unsupported_flags = packet.clone();
        *unsupported_flags.last_mut().expect("advertisement flag") = 0x80;
        assert_eq!(decode_player_advertisement(&unsupported_flags), None);
        let zero_id = player_advertisement(&[0; 6], 4995, "W1VE ESP32 Player", 0);
        assert_eq!(decode_player_advertisement(&zero_id), None);
    }

    #[test]
    fn windows_udp_port_unreachable_is_a_transient_discovery_result() {
        let reset = std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "ICMP port unreachable from an empty discovery candidate",
        );
        let fatal = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "bind policy");
        assert!(transient_discovery_receive_error(&reset));
        assert!(!transient_discovery_receive_error(&fatal));
    }

    #[test]
    fn envelope_window_accepts_bounded_reordering_and_rejects_replays() {
        let mut window = SequenceWindow::default();
        assert!(window.accept(4));
        assert!(window.accept(6));
        assert!(window.accept(5));
        assert!(!window.accept(5));
        assert!(!window.accept(0));
        assert!(window.accept(70));
        assert!(!window.accept(6), "a sequence 64 positions behind is outside the window");
    }

    #[test]
    fn pair_request_targets_the_selected_player() {
        let player = [1, 2, 3, 4, 5, 6];
        let keyer = [6, 5, 4, 3, 2, 1];
        let nonce = [9u8; 16];
        let request = pair_request(&player, &keyer, &nonce);
        assert_eq!(parse_pair_request(&request, &player), Some((keyer, nonce)));
        assert_eq!(parse_pair_request(&request, &[0u8; 6]), None);
        let zero_keyer = pair_request(&player, &[0; 6], &nonce);
        assert_eq!(parse_pair_request(&zero_keyer, &player), None);
    }

    #[tokio::test]
    async fn keyer_initiates_pair_and_serializes_frame_refresh_and_ping_envelopes() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let endpoint = server.local_addr().unwrap();
        let player_id = [10, 11, 12, 13, 14, 15];
        let keyer_id = [1, 2, 3, 4, 5, 6];
        let responder = tokio::spawn(async move {
            let mut request = [0u8; 64];
            let (length, client) = server.recv_from(&mut request).await.unwrap();
            assert_eq!(length, PAIR_REQUEST_LEN);
            let (_, initiator_nonce) = parse_pair_request(&request[..length], &player_id).unwrap();
            let responder_nonce = [7u8; 16];
            let key = session_key(&player_id, &keyer_id, &initiator_nonce, &responder_nonce);
            server
                .send_to(&pair_accept(&player_id, &keyer_id, &initiator_nonce, &responder_nonce), client)
                .await
                .unwrap();

            let mut sequences = Vec::new();
            let mut frames = Vec::new();
            for _ in 0..3 {
                let mut packet = [0u8; 128];
                let (length, _) = server.recv_from(&mut packet).await.unwrap();
                let unsigned_length = length - TAG_LEN;
                assert_eq!(&hmac16(&key, &packet[..unsigned_length]), &packet[unsigned_length..length]);
                sequences.push(u32::from_be_bytes(packet[16..20].try_into().unwrap()));
                if packet[5] == TYPE_EDGE {
                    frames.push(packet[HEADER_LEN..unsigned_length].to_vec());
                } else {
                    assert_eq!(packet[5], TYPE_PING);
                }
            }
            sequences.sort_unstable();
            assert_eq!(sequences, vec![1, 2, 3], "every envelope has a fresh serialized sequence");
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0], frames[1], "a frame refresh repeats the complete inner frame");
        });
        let player = EspPlayerAdvertisement::from_parts(player_id, "Test Player".into(), endpoint, 0);
        let session = EspPlayerSession::pair(player, keyer_id).await.unwrap();
        let frame = RwkPaddleFrame::try_new(1, &[EdgeEntry::key_down_at(0, 0, 0)]).unwrap();
        let (frame_result, ping_result) = tokio::join!(session.send_frame(&frame), session.send_ping());
        frame_result.unwrap();
        ping_result.unwrap();
        session.send_frame(&frame).await.unwrap();
        responder.await.unwrap();
    }

    #[test]
    fn session_material_is_specific_to_the_selected_player_and_nonces() {
        let left = session_key(&[1; 6], &[2; 6], &[3; 16], &[4; 16]);
        let right = session_key(&[1; 6], &[2; 6], &[3; 16], &[5; 16]);
        assert_ne!(left, right);
        assert_eq!(left, session_key(&[1; 6], &[2; 6], &[3; 16], &[4; 16]));
    }

    #[test]
    fn session_key_and_hmac_match_the_w1ve_v2_cross_language_vector() {
        let player_id = [1, 2, 3, 4, 5, 6];
        let keyer_id = [0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6];
        let initiator_nonce = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let responder_nonce = [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];
        let key = session_key(&player_id, &keyer_id, &initiator_nonce, &responder_nonce);
        assert_eq!(
            key,
            [
                0x8d, 0x9d, 0x3a, 0x9f, 0x13, 0x24, 0x65, 0x19, 0x84, 0xa3, 0xd8, 0x28, 0xf9,
                0x32, 0x3e, 0xc3, 0x5e, 0x06, 0xba, 0x3d, 0x36, 0x35, 0x6c, 0x5f, 0xf1, 0x40,
                0xd0, 0x76, 0x17, 0x94, 0xf2, 0x25,
            ]
        );
        let mut message = Vec::new();
        message.extend_from_slice(&MAGIC);
        message.extend_from_slice(&[ESP_PLAYER_PROTOCOL_VERSION, TYPE_PING]);
        message.extend_from_slice(&player_id);
        message.extend_from_slice(&key[..4]);
        message.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        message.push(8);
        message.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            hmac16(&key, &message),
            [
                0x6e, 0x40, 0x31, 0xde, 0x64, 0x88, 0x4c, 0x69, 0x60, 0x13, 0x12, 0x3b, 0xe2,
                0x87, 0xf3, 0x93,
            ]
        );
    }

    fn authenticated_player_packet(
        session: &EspPlayerSession,
        packet_type: u8,
        sequence: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut packet = Vec::with_capacity(HEADER_LEN + payload.len() + TAG_LEN);
        packet.extend_from_slice(&MAGIC);
        packet.extend_from_slice(&[ESP_PLAYER_PROTOCOL_VERSION, packet_type]);
        packet.extend_from_slice(&session.player.player_id);
        packet.extend_from_slice(&session.session_id.to_be_bytes());
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.push(payload.len().try_into().expect("test payload length"));
        packet.extend_from_slice(payload);
        packet.extend_from_slice(&hmac16(&session.session_key, &packet));
        packet
    }

    #[tokio::test]
    async fn reordered_control_packets_do_not_mask_valid_pongs_or_consume_invalid_types() {
        let player_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let player_endpoint = player_socket.local_addr().unwrap();
        let keyer_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let keyer_endpoint = keyer_socket.local_addr().unwrap();
        let player = EspPlayerAdvertisement::from_parts([1, 2, 3, 4, 5, 6], "Test Player".into(), player_endpoint, 0);
        let session = EspPlayerSession::new(
            Arc::clone(&keyer_socket),
            player,
            [7, 8, 9, 10, 11, 12],
            [13; 16],
            [14; 16],
        );

        let keyer_ping = session.send_ping().await.unwrap();
        let mut sent = [0u8; 128];
        let _ = player_socket.recv_from(&mut sent).await.unwrap();

        player_socket
            .send_to(
                &authenticated_player_packet(&session, TYPE_PONG, 4, &keyer_ping.to_be_bytes()),
                keyer_endpoint,
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap(), Some(EspPlayerInbound::Control));
        assert!(session.ping_was_acknowledged(keyer_ping));

        player_socket
            .send_to(&authenticated_player_packet(&session, TYPE_PING, 6, b"PINGPING"), keyer_endpoint)
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap(), Some(EspPlayerInbound::Control));
        let _ = player_socket.recv_from(&mut sent).await.unwrap();

        player_socket
            .send_to(
                &authenticated_player_packet(&session, TYPE_PONG, 5, &keyer_ping.to_be_bytes()),
                keyer_endpoint,
            )
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap(), Some(EspPlayerInbound::Control));

        // Type 4 is invalid from a Player and must not consume its envelope sequence.
        player_socket
            .send_to(&authenticated_player_packet(&session, TYPE_EDGE, 7, &[]), keyer_endpoint)
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap(), None);
        player_socket
            .send_to(&authenticated_player_packet(&session, TYPE_PING, 7, b"SEQPING!"), keyer_endpoint)
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap(), Some(EspPlayerInbound::Control));
        let _ = player_socket.recv_from(&mut sent).await.unwrap();

        player_socket
            .send_to(&authenticated_player_packet(&session, TYPE_UNPAIR, 5, &[]), keyer_endpoint)
            .await
            .unwrap();
        assert_eq!(session.recv().await.unwrap(), None, "a replayed unpair is ignored");
    }

    #[tokio::test]
    async fn outbound_envelope_counter_stops_before_wrap() {
        let player_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let player = EspPlayerAdvertisement::from_parts(
            [1, 2, 3, 4, 5, 6],
            "Test Player".into(),
            player_socket.local_addr().unwrap(),
            0,
        );
        let session = EspPlayerSession::new(
            Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap()),
            player,
            [7, 8, 9, 10, 11, 12],
            [13; 16],
            [14; 16],
        );
        *session.tx_sequence.lock().await = u32::MAX - 2;

        session.send_ping().await.expect("last permitted sequence sends");
        let mut packet = [0u8; 128];
        let (length, _) = player_socket.recv_from(&mut packet).await.unwrap();
        assert_eq!(length, HEADER_LEN + 8 + TAG_LEN);
        assert_eq!(u32::from_be_bytes(packet[16..20].try_into().unwrap()), u32::MAX - 1);

        assert!(session.send_ping().await.is_err());
        assert!(session.sequence_exhausted());
    }
}
