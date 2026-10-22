//! UDP media socket with the socket options Lumen needs for low latency.

use std::io;
use std::net::{SocketAddr, UdpSocket};

use socket2::{Domain, Protocol, Type};

/// Socket options applied to a media socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketOptions {
    /// `SO_PRIORITY` value. 5 for video, 6 for audio (as used by Sunshine).
    pub priority: u32,
    /// IP traffic class / DSCP value: 40 (CS5) for video, 48 (CS6) for audio.
    ///
    /// The mark is only set toward a private address; a WAN path bleaches or rejects it, and
    /// marking a packet expedited that a switch does not honour buys nothing.
    pub dscp: u32,
    /// Size of the socket receive buffer in bytes.
    pub recv_buffer: usize,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            priority: 5,
            dscp: DSCP_VIDEO,
            recv_buffer: 4 * 1024 * 1024,
        }
    }
}

/// DSCP of video traffic: class selector 5.
pub const DSCP_VIDEO: u32 = 40;

/// DSCP of audio traffic: class selector 6.
pub const DSCP_AUDIO: u32 = 48;

/// Whether a peer address is one where a traffic class mark is worth setting: the private
/// ranges, the loopback and the link-local ones.
#[must_use]
pub fn is_private_peer(addr: &SocketAddr) -> bool {
    match addr {
        SocketAddr::V4(v4) => {
            v4.ip().is_private()
                || v4.ip().is_loopback()
                || v4.ip().is_link_local()
                || v4.ip().is_broadcast()
                || v4.ip().is_unspecified()
        }
        SocketAddr::V6(v6) => {
            let ip = v6.ip();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.segments()[0] == 0xfe80
        }
    }
}

/// A UDP socket configured for media traffic.
#[derive(Debug)]
pub struct MediaSocket {
    socket: UdpSocket,
    /// The traffic class to mark, in DSCP points.
    dscp: u32,
}

impl MediaSocket {
    /// Bind a socket to `addr` with the given options.
    ///
    /// # Errors
    /// Returns an io error if binding or setting socket options fails.
    pub fn bind(addr: SocketAddr, opts: SocketOptions) -> io::Result<Self> {
        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = socket2::Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        // SO_PRIORITY and IP_TOS drive the kernel's queueing priority. The traffic class is
        // set again in `connect`, once the peer is known.
        socket.set_priority(opts.priority)?;
        let _ = socket.set_recv_buffer_size(opts.recv_buffer);
        let _ = socket.set_send_buffer_size(opts.recv_buffer);
        let socket: UdpSocket = socket.into();
        Ok(Self {
            socket,
            dscp: opts.dscp,
        })
    }

    /// Connect the socket to a peer. `connect` on a UDP socket filters out
    /// packets from other sources and lets the kernel report errors.
    ///
    /// The traffic class is set here: a peer on a private network gets the mark, a peer
    /// anywhere else does not, because a WAN path bleaches or rejects it.
    ///
    /// # Errors
    /// Returns an io error if the connect fails.
    pub fn connect(&self, peer: SocketAddr) -> io::Result<()> {
        self.socket.connect(peer)?;
        self.set_traffic_class(peer)
    }

    /// Sets the traffic class of the socket, or clears it for a peer that is not private.
    ///
    /// # Errors
    /// Returns an io error if the option cannot be set.
    pub fn set_traffic_class(&self, peer: SocketAddr) -> io::Result<()> {
        // A borrowed view of the same socket, so the option is set on it.
        let socket = socket2::SockRef::from(&self.socket);
        let dscp = if is_private_peer(&peer) { self.dscp } else { 0 };
        if peer.is_ipv4() {
            socket.set_tos_v4(dscp << 2)?;
        } else {
            socket.set_tclass_v6(dscp << 2)?;
        }
        Ok(())
    }

    /// The local address the socket is bound to.
    ///
    /// # Errors
    /// Returns an io error if the address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Send a packet to `peer`.
    ///
    /// # Errors
    /// Returns an io error if the send fails.
    pub fn send_to(&self, buf: &[u8], peer: SocketAddr) -> io::Result<usize> {
        self.socket.send_to(buf, peer)
    }

    /// Receive a packet, returning the payload and the sender.
    ///
    /// # Errors
    /// Returns an io error if the receive fails.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf)
    }

    /// Makes `recv_from` wait up to `timeout` for a packet instead of failing at once with
    /// `WouldBlock`. A receiver that sleeps on `WouldBlock` delays every packet that arrives
    /// during the sleep, and a frame's slices arrive in one burst.
    ///
    /// # Errors
    /// Returns an io error if the socket cannot be switched to blocking.
    pub fn wait_for_packets(&self, timeout: std::time::Duration) -> io::Result<()> {
        self.socket.set_nonblocking(false)?;
        self.socket.set_read_timeout(Some(timeout))
    }

    /// The underlying socket, for advanced operations.
    #[must_use]
    pub fn inner(&self) -> &UdpSocket {
        &self.socket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traffic_class_is_only_marked_toward_a_private_peer() {
        assert!(is_private_peer(&"127.0.0.1:24801".parse().unwrap()));
        assert!(is_private_peer(&"192.168.1.10:24801".parse().unwrap()));
        assert!(is_private_peer(&"10.0.0.4:24801".parse().unwrap()));
        assert!(is_private_peer(&"172.16.0.9:24801".parse().unwrap()));
        assert!(is_private_peer(&"[fe80::1]:24801".parse().unwrap()));
        assert!(is_private_peer(&"[fd00::1]:24801".parse().unwrap()));
        assert!(!is_private_peer(&"8.8.8.8:24801".parse().unwrap()));
        assert!(!is_private_peer(
            &"[2606:4700::1111]:24801".parse().unwrap()
        ));
    }

    #[test]
    fn binds_and_reports_address() {
        let s =
            MediaSocket::bind("127.0.0.1:0".parse().unwrap(), SocketOptions::default()).unwrap();
        let addr = s.local_addr().unwrap();
        assert!(addr.port() > 0);
    }

    #[test]
    fn send_and_receive() {
        let a =
            MediaSocket::bind("127.0.0.1:0".parse().unwrap(), SocketOptions::default()).unwrap();
        let b =
            MediaSocket::bind("127.0.0.1:0".parse().unwrap(), SocketOptions::default()).unwrap();
        let b_addr = b.local_addr().unwrap();
        a.send_to(b"hello", b_addr).unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(from.ip(), b_addr.ip());
    }
}
