//! The pairing model: how the Server and Client find each other, over WAN or LAN.
//!
//! The UI offers a **WAN/LAN** slider and a **pairing token** the Server generates and the
//! Client enters — the same model as the C# "WinKey Remote" pair in the screenshot, but on the
//! native `rwk-core` engine.
//!
//! Both transports use the engine's zero-setup rendezvous
//! ([`rwk_core::engine::tunnel::connect_paired`]): both ends present the same pairing code to a
//! relay, which introduces them (public keys + observed addresses), and the WireGuard tunnel
//! comes up **directly** (hole-punched) whenever the two can reach each other, falling back to
//! carrying traffic over the relay only when they cannot. The WAN/LAN choice selects *which
//! relay* does the introduction:
//!
//! * **WAN** — the public W1VE relay on the internet. Zero network setup: no port forwarding,
//!   no static IP. Traffic goes peer-to-peer when NATs allow, else via the relay.
//! * **LAN** — a relay reachable on the local network (often the Server itself running the
//!   relay). Two hosts on the same LAN hole-punch trivially, so after the brief rendezvous all
//!   keyed traffic stays on the LAN with no internet dependency.
//!
//! Why a relay even on the LAN: the engine's WireGuard peers are *static-keyed*, so each end
//! must learn the other's public key before the handshake. The relay rendezvous is what
//! exchanges those keys from a short human code, which is exactly what lets the operator pair
//! with a token instead of copying 32-byte keys by hand. A later increment can add a
//! key-learning LAN responder to drop the relay entirely on the LAN.
//!
//! [`build_config`] turns the UI's choice + token + relay into the
//! [`rwk_core::config::Config`] the engine consumes, so both controllers share one path.

use rwk_core::config::{Config, RelayConfig, Role};

/// Which transport the operator chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Over the internet via the public W1VE relay.
    Wan,
    /// On the local network via a LAN-reachable relay (zero internet dependency once paired).
    Lan,
}

impl Transport {
    /// Parses the UI slider value ("wan"/"lan"); anything unrecognised defaults to WAN.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("lan") {
            Transport::Lan
        } else {
            Transport::Wan
        }
    }

    /// The UI name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Wan => "wan",
            Transport::Lan => "lan",
        }
    }
}

/// The default public W1VE relay used for WAN pairing. The operator can override it.
pub const DEFAULT_WAN_RELAY: &str = "107.172.138.103:9000";

/// Builds a connect-ready **WAN** [`Config`] from the operator's token + relay, starting from
/// the node's persisted `base` config (which carries the stable WireGuard identity).
///
/// This is the WAN path only: it sets `relay` + `pairing_code` so the engine's `connect_paired`
/// does the relay rendezvous and prefers a direct hole-punched path. LAN pairing does not go
/// through here at all — it is relay-free and driven by the controllers' `tunnel::lan_*` calls
/// and the discovery beacons (see [`crate::client_controller`] / [`crate::server_controller`]).
///
/// # Errors
///
/// Returns an error string for the LAN transport (a programming error — call the LAN path
/// instead) or if the relay address does not parse.
pub fn build_config(
    base: &Config,
    role: Role,
    transport: Transport,
    token: &str,
    relay: &str,
) -> Result<Config, String> {
    if transport == Transport::Lan {
        return Err("LAN pairing does not use build_config; use the LAN discovery path".into());
    }
    let relay = relay.trim();
    let relay = if relay.is_empty() { DEFAULT_WAN_RELAY } else { relay };
    let address = relay
        .parse()
        .map_err(|e| format!("invalid relay address '{relay}': {e}"))?;

    let mut config = base.clone();
    config.role = role;
    config.relay = Some(RelayConfig { address });
    config.pairing_code = Some(token.trim().to_string());
    Ok(config)
}

/// A convenience "token" generator: a short, human-sharable random code the Server shows and
/// the Client types. Twelve hex chars is plenty of entropy for a rendezvous that is only open
/// for a short pairing window.
///
/// The randomness comes from a throwaway `rwk_core` identity, whose private key is drawn from
/// the OS CSPRNG — so no extra RNG dependency is needed here, and the bytes are high quality.
#[must_use]
pub fn generate_token() -> String {
    use rwk_core::config::base64_decode;
    // A fresh identity's public key is 32 CSPRNG-derived bytes; take six of them.
    let seed = Config::new(Role::Client)
        .ok()
        .and_then(|c| c.identity.public_key_base64().ok())
        .and_then(|b64| base64_decode(&b64).ok())
        .filter(|b| b.len() >= 6);
    let bytes: Vec<u8> = match seed {
        Some(b) => b[..6].to_vec(),
        None => {
            // Extremely unlikely fallback: time-seeded so the UI still gets a token.
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            n.to_le_bytes()[..6].to_vec()
        }
    };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_parses_and_round_trips() {
        assert_eq!(Transport::parse("lan"), Transport::Lan);
        assert_eq!(Transport::parse("LAN"), Transport::Lan);
        assert_eq!(Transport::parse("wan"), Transport::Wan);
        assert_eq!(Transport::parse("other"), Transport::Wan);
        assert_eq!(Transport::Lan.as_str(), "lan");
        assert_eq!(Transport::Wan.as_str(), "wan");
    }

    #[test]
    fn wan_config_uses_the_default_relay_when_blank() {
        let base = Config::new(Role::Client).unwrap();
        let config = build_config(&base, Role::Client, Transport::Wan, "ab12cd", "").unwrap();
        assert_eq!(config.relay.unwrap().address.to_string(), DEFAULT_WAN_RELAY);
        assert_eq!(config.pairing_code.as_deref(), Some("ab12cd"));
    }

    #[test]
    fn lan_does_not_use_build_config() {
        let base = Config::new(Role::Station).unwrap();
        // LAN is relay-free and handled by the discovery path, so build_config refuses it.
        assert!(build_config(&base, Role::Station, Transport::Lan, "x", "192.168.1.2:9000").is_err());
    }

    #[test]
    fn a_bad_wan_relay_errors() {
        let base = Config::new(Role::Client).unwrap();
        assert!(build_config(&base, Role::Client, Transport::Wan, "x", "nope").is_err());
    }

    #[test]
    fn generated_tokens_are_twelve_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 12);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
