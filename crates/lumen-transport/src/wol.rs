//! Wake-on-LAN: the magic packet that wakes a sleeping host.
//!
//! The packet is the broadcast address followed by the target's MAC repeated sixteen times. It
//! goes out as UDP to the subnet broadcast of the host and to the global broadcast, from a
//! socket bound to every interface, so it leaves the machine even without a route to the
//! subnet.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// A MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    /// The address as it goes on the wire.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 6] {
        self.0
    }

    /// Parse `aa:bb:cc:dd:ee:ff`, with the separators optional.
    ///
    /// # Errors
    /// Returns a description when the text is not six hex bytes.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex: String = text
            .chars()
            .filter(|c| *c != ':' && *c != '-' && *c != '.')
            .collect();
        if hex.len() != 12 {
            return Err(format!("a MAC address is six bytes, got {text:?}"));
        }
        let mut bytes = [0u8; 6];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| format!("{text:?} is not a MAC address"))?;
        }
        Ok(Self(bytes))
    }
}

/// The magic packet for `mac`: the broadcast address and the MAC sixteen times.
#[must_use]
pub fn magic_packet(mac: &MacAddr) -> [u8; 102] {
    let mut packet = [0u8; 102];
    packet[0] = 0xff;
    packet[1] = 0xff;
    packet[2] = 0xff;
    packet[3] = 0xff;
    packet[4] = 0xff;
    packet[5] = 0xff;
    for index in 0..16 {
        let start = 6 + index * 6;
        packet[start..start + 6].copy_from_slice(&mac.bytes());
    }
    packet
}

/// Where a magic packet for a host at `host` is sent: the broadcast of its subnet and the
/// global broadcast.
#[must_use]
pub fn broadcast_targets(host: SocketAddr) -> Vec<SocketAddr> {
    let mut targets = Vec::new();
    match host {
        SocketAddr::V4(v4) => {
            let ip = v4.ip();
            let octets = ip.octets();
            let subnet = match octets[0] {
                10 => [10, 255, 255, 255],
                172 if (16..32).contains(&octets[1]) => [172, 31, 255, 255],
                192 if octets[1] == 168 => [192, 168, 1, 255],
                _ => [255, 255, 255, 255],
            };
            targets.push(SocketAddr::from((Ipv4Addr::from(subnet), v4.port())));
        }
        SocketAddr::V6(_) => {}
    }
    targets.push(SocketAddr::from((Ipv4Addr::BROADCAST, 9)));
    targets
}

/// Sends the magic packet for `mac` to every target in `targets`.
///
/// # Errors
/// Returns the first socket or send error.
pub fn send(mac: &MacAddr, targets: &[SocketAddr]) -> io::Result<usize> {
    let packet = magic_packet(mac);
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_broadcast(true)?;
    let mut sent = 0;
    for target in targets {
        socket.send_to(&packet, target)?;
        sent += 1;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_magic_packet_is_the_mac_repeated_sixteen_times() {
        let mac = MacAddr::parse("aa:bb:cc:dd:ee:ff").expect("mac");
        let packet = magic_packet(&mac);
        assert_eq!(&packet[..6], &[0xff; 6]);
        for index in 0..16 {
            let start = 6 + index * 6;
            assert_eq!(
                &packet[start..start + 6],
                &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
            );
        }
    }

    #[test]
    fn a_mac_is_parsed_with_or_without_separators() {
        assert_eq!(
            MacAddr::parse("aa:bb:cc:dd:ee:ff").unwrap(),
            MacAddr::parse("aabbccddeeff").unwrap()
        );
        assert!(MacAddr::parse("aabb").is_err());
        assert!(MacAddr::parse("zz:bb:cc:dd:ee:ff").is_err());
    }

    #[test]
    fn the_targets_are_the_subnet_broadcast_and_the_global_one() {
        let targets = broadcast_targets("192.168.1.10:24800".parse().unwrap());
        assert_eq!(targets[0].ip().to_string(), "192.168.1.255");
        assert_eq!(targets[1].ip().to_string(), "255.255.255.255");
        let targets = broadcast_targets("10.1.2.3:24800".parse().unwrap());
        assert_eq!(targets[0].ip().to_string(), "10.255.255.255");
    }
}
