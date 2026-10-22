//! UDP media socket with the socket options Lumen needs for low latency.

use std::io;
use std::net::{SocketAddr, UdpSocket};

use socket2::{Domain, Protocol, Type};

/// Socket options applied to a media socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketOptions {
    /// `SO_PRIORITY` value. 5 for video, 6 for audio (as used by Sunshine).
    pub priority: u32,
    /// IP traffic class / DSCP value. 46 (EF) for media.
    pub dscp: u32,
    /// Size of the socket receive buffer in bytes.
    pub recv_buffer: usize,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            priority: 5,
            dscp: 46,
            recv_buffer: 4 * 1024 * 1024,
        }
    }
}

/// A UDP socket configured for media traffic.
#[derive(Debug)]
pub struct MediaSocket {
    socket: UdpSocket,
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
        // SO_PRIORITY and IP_TOS drive the kernel's queueing priority.
        socket.set_priority(opts.priority)?;
        if addr.is_ipv4() {
            socket.set_tos_v4(opts.dscp << 2)?;
        } else {
            socket.set_tclass_v6(opts.dscp << 2)?;
        }
        let _ = socket.set_recv_buffer_size(opts.recv_buffer);
        let _ = socket.set_send_buffer_size(opts.recv_buffer);
        let socket: UdpSocket = socket.into();
        Ok(Self { socket })
    }

    /// Connect the socket to a peer. `connect` on a UDP socket filters out
    /// packets from other sources and lets the kernel report errors.
    ///
    /// # Errors
    /// Returns an io error if the connect fails.
    pub fn connect(&self, peer: SocketAddr) -> io::Result<()> {
        self.socket.connect(peer)
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
