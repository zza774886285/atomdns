//! Reply source addresses for wildcard UDP listeners.
//!
//! A listener bound to the wildcard address answers from whatever source the
//! routing table picks — the primary address of the interface, not the address
//! the query was actually sent to. Behind a floating VIP (keepalived/VRRP) that
//! breaks every client that checks the responder address, so the destination of
//! each datagram has to be carried over to the reply.
//!
//! Linux hands it over as an ancillary message when `IP_PKTINFO` (IPv4) or
//! `IPV6_RECVPKTINFO` (IPv6, and IPv4-mapped packets on a dual-stack socket) is
//! enabled: `recvmsg` reports the destination address, `sendmsg` pins the same
//! address as the source. Everything here is a no-op wrapper on other systems.

// 这个模块绕不开系统调用（recvmsg/sendmsg 的辅助数据），与 local_time / memory 一样
// 单独放开 unsafe 检查，crate 其余部分仍保持 deny。
#![allow(unsafe_code)]

use std::io;
use std::net::SocketAddr;

use tokio::net::UdpSocket;

/// Socket family of the listener the option is enabled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    V4,
    V6,
}

/// The destination address a datagram arrived on, plus the interface index the
/// kernel reported. Echoed back on the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalAddr {
    pub(crate) addr: std::net::IpAddr,
    pub(crate) ifindex: u32,
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::mem::{self, MaybeUninit};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::fd::{AsRawFd, RawFd};

    use tokio::io::Interest;
    use tokio::net::UdpSocket;

    use super::{Family, LocalAddr};

    /// `struct in_pktinfo` is not defined by the `libc` crate for Linux, so the
    /// layout from `ip(7)` is spelled out here.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InPktinfo {
        ipi_ifindex: libc::c_int,
        ipi_spec_dst: libc::in_addr,
        ipi_addr: libc::in_addr,
    }

    /// `struct in6_pktinfo` from `ipv6(7)`; also absent from `libc` on Linux.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct In6Pktinfo {
        ipi6_addr: libc::in6_addr,
        ipi6_ifindex: libc::c_uint,
    }

    /// Ancillary data has to start on a `cmsghdr` boundary.
    #[repr(C, align(8))]
    struct ControlBuffer {
        bytes: [u8; 128],
    }

    impl ControlBuffer {
        fn zeroed() -> Self {
            Self { bytes: [0; 128] }
        }
    }

    pub(super) fn enable(socket: &socket2::Socket, family: Family) -> io::Result<()> {
        match family {
            Family::V4 => set_option(socket.as_raw_fd(), libc::IPPROTO_IP, libc::IP_PKTINFO),
            Family::V6 => {
                // IPv4 packets that reach a dual-stack socket are reported as
                // IPv4-mapped addresses in `IPV6_PKTINFO`, so one option covers
                // both families there.
                set_option(
                    socket.as_raw_fd(),
                    libc::IPPROTO_IPV6,
                    libc::IPV6_RECVPKTINFO,
                )
            }
        }
    }

    fn set_option(fd: RawFd, level: libc::c_int, name: libc::c_int) -> io::Result<()> {
        let value: libc::c_int = 1;
        // SAFETY: `value` outlives the call and its size is passed explicitly.
        let result = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&raw const value).cast(),
                mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// `recvmsg` with the datagram's destination address, if the kernel reported
    /// one.
    pub(super) async fn recv_from(
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, Option<LocalAddr>)> {
        loop {
            match socket.try_io(Interest::READABLE, || recv_message(socket, buffer)) {
                Ok(received) => return Ok(received),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    socket.readable().await?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn recv_message(
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, Option<LocalAddr>)> {
        let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let mut control = ControlBuffer::zeroed();
        // SAFETY: a zeroed `msghdr` is valid; every pointer below refers to a
        // live local that outlives the call.
        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_name = peer.as_mut_ptr().cast();
        header.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.bytes.as_mut_ptr().cast();
        header.msg_controllen = control.bytes.len() as _;

        // SAFETY: the header describes buffers that are all live and correctly
        // sized for the call.
        let length = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut header, 0) };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel filled the sockaddr for the datagram just received.
        let address = unsafe { socket_addr(&peer.assume_init(), header.msg_namelen)? };
        // SAFETY: `header` still describes the control data the kernel wrote.
        let local = unsafe { local_addr(&header) };
        Ok((length as usize, address, local))
    }

    /// The source address the reply has to use, echoed back on `sendmsg`.
    pub(super) async fn send_to(
        socket: &UdpSocket,
        data: &[u8],
        peer: SocketAddr,
        local: Option<LocalAddr>,
    ) -> io::Result<usize> {
        loop {
            match socket.try_io(Interest::WRITABLE, || {
                send_message(socket, data, peer, local)
            }) {
                Ok(sent) => return Ok(sent),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    socket.writable().await?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn send_message(
        socket: &UdpSocket,
        data: &[u8],
        peer: SocketAddr,
        local: Option<LocalAddr>,
    ) -> io::Result<usize> {
        let (name, name_length) = raw_address(peer);
        let mut iov = libc::iovec {
            iov_base: data.as_ptr().cast_mut().cast(),
            iov_len: data.len(),
        };
        let mut control = ControlBuffer::zeroed();
        let mut control_length = 0usize;
        if let Some(local) = local {
            control_length = write_pktinfo(&mut control, peer, local);
        }
        // SAFETY: a zeroed `msghdr` is valid; the pointers below refer to live
        // locals.
        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_name = (&raw const name).cast_mut().cast();
        header.msg_namelen = name_length;
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        if control_length > 0 {
            header.msg_control = control.bytes.as_mut_ptr().cast();
            header.msg_controllen = control_length as _;
        }
        // SAFETY: the header describes live buffers and a valid destination.
        let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &header, 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sent as usize)
    }

    /// A socket address in its raw form, matching the family of the peer.
    fn raw_address(peer: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
        // SAFETY: a zeroed `sockaddr_storage` is a valid starting point for
        // writing both address families.
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        match peer {
            SocketAddr::V4(address) => {
                // SAFETY: the storage is at least as large as `sockaddr_in` and
                // correctly aligned for it.
                let raw = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in>() };
                raw.sin_family = libc::AF_INET as libc::sa_family_t;
                raw.sin_port = address.port().to_be();
                raw.sin_addr = in_addr(Ipv4Addr::from(*address.ip()));
                (
                    storage,
                    mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
            SocketAddr::V6(address) => {
                // SAFETY: same reasoning as the IPv4 branch.
                let raw = unsafe { &mut *(&raw mut storage).cast::<libc::sockaddr_in6>() };
                raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                raw.sin6_port = address.port().to_be();
                raw.sin6_addr = in6_addr(*address.ip());
                raw.sin6_scope_id = address.scope_id();
                (
                    storage,
                    mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    }

    fn in_addr(address: Ipv4Addr) -> libc::in_addr {
        libc::in_addr {
            s_addr: u32::from_ne_bytes(address.octets()),
        }
    }

    fn in6_addr(address: Ipv6Addr) -> libc::in6_addr {
        libc::in6_addr {
            s6_addr: address.octets(),
        }
    }

    /// Writes the ancillary message that pins the reply's source address and
    /// returns the control length to pass to `sendmsg` (0 when unsupported).
    #[allow(clippy::cast_possible_truncation)]
    fn write_pktinfo(
        control: &mut ControlBuffer,
        peer: SocketAddr,
        local: LocalAddr,
    ) -> usize {
        // The ancillary message has to belong to the socket's family: a
        // dual-stack listener reports IPv4 peers as IPv4-mapped addresses and
        // expects `IPV6_PKTINFO` back, not `IP_PKTINFO`.
        let (level, kind, payload_length) = match peer {
            SocketAddr::V4(_) => (
                libc::IPPROTO_IP,
                libc::IP_PKTINFO,
                mem::size_of::<InPktinfo>() as libc::c_uint,
            ),
            SocketAddr::V6(_) => (
                libc::IPPROTO_IPV6,
                libc::IPV6_PKTINFO,
                mem::size_of::<In6Pktinfo>() as libc::c_uint,
            ),
        };
        // SAFETY: the control buffer is aligned for `cmsghdr` and large enough
        // for one message of `payload_length` bytes.
        unsafe {
            let header = control.bytes.as_mut_ptr().cast::<libc::cmsghdr>();
            (*header).cmsg_level = level;
            (*header).cmsg_type = kind;
            (*header).cmsg_len = libc::CMSG_LEN(payload_length) as _;
            let data = libc::CMSG_DATA(header);
            match peer {
                SocketAddr::V4(_) => data.cast::<InPktinfo>().write_unaligned(InPktinfo {
                    ipi_ifindex: local.ifindex as libc::c_int,
                    ipi_spec_dst: in_addr(match local.addr {
                        IpAddr::V4(address) => address,
                        IpAddr::V6(address) => address.to_ipv4_mapped().unwrap_or(Ipv4Addr::UNSPECIFIED),
                    }),
                    ipi_addr: in_addr(Ipv4Addr::UNSPECIFIED),
                }),
                SocketAddr::V6(_) => data.cast::<In6Pktinfo>().write_unaligned(In6Pktinfo {
                    ipi6_addr: in6_addr(match local.addr {
                        IpAddr::V6(address) => address,
                        IpAddr::V4(address) => address.to_ipv6_mapped(),
                    }),
                    ipi6_ifindex: local.ifindex,
                }),
            }
            libc::CMSG_SPACE(payload_length) as usize
        }
    }

    /// The destination address and interface index carried by a received
    /// datagram, when the kernel reported them.
    ///
    /// # Safety
    /// `header` must describe control data written by the kernel.
    unsafe fn local_addr(header: &libc::msghdr) -> Option<LocalAddr> {
        // SAFETY: the caller guarantees `header` came from `recvmsg`.
        let mut message = unsafe { libc::CMSG_FIRSTHDR(header) };
        while !message.is_null() {
            // SAFETY: `message` points inside the control buffer of `header`.
            let (level, kind) = unsafe { ((*message).cmsg_level, (*message).cmsg_type) };
            // SAFETY: the kernel wrote `cmsg_len` bytes after the header.
            let data = unsafe { libc::CMSG_DATA(message) };
            if level == libc::IPPROTO_IP && kind == libc::IP_PKTINFO {
                // SAFETY: the payload of `IP_PKTINFO` is a whole `in_pktinfo`.
                let info = unsafe { data.cast::<InPktinfo>().read_unaligned() };
                return Some(LocalAddr {
                    addr: IpAddr::V4(Ipv4Addr::from(info.ipi_addr.s_addr.to_ne_bytes())),
                    ifindex: info.ipi_ifindex as u32,
                });
            }
            if level == libc::IPPROTO_IPV6 && kind == libc::IPV6_PKTINFO {
                // SAFETY: the payload of `IPV6_PKTINFO` is a whole `in6_pktinfo`.
                let info = unsafe { data.cast::<In6Pktinfo>().read_unaligned() };
                return Some(LocalAddr {
                    addr: IpAddr::V6(Ipv6Addr::from(info.ipi6_addr.s6_addr)),
                    ifindex: info.ipi6_ifindex,
                });
            }
            // SAFETY: `message` points inside the control buffer of `header`.
            message = unsafe { libc::CMSG_NXTHDR(header, message) };
        }
        None
    }

    /// # Safety
    /// `storage` must have been filled in by the kernel for `length` bytes.
    unsafe fn socket_addr(
        storage: &libc::sockaddr_storage,
        length: libc::socklen_t,
    ) -> io::Result<SocketAddr> {
        match storage.ss_family as libc::c_int {
            libc::AF_INET => {
                if (length as usize) < mem::size_of::<libc::sockaddr_in>() {
                    return Err(io::Error::from(io::ErrorKind::InvalidData));
                }
                // SAFETY: the family says this is a `sockaddr_in`.
                let raw = unsafe { &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
                let address = Ipv4Addr::from(raw.sin_addr.s_addr.to_ne_bytes());
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    address,
                    u16::from_be(raw.sin_port),
                )))
            }
            libc::AF_INET6 => {
                if (length as usize) < mem::size_of::<libc::sockaddr_in6>() {
                    return Err(io::Error::from(io::ErrorKind::InvalidData));
                }
                // SAFETY: the family says this is a `sockaddr_in6`.
                let raw = unsafe { &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
                let address = Ipv6Addr::from(raw.sin6_addr.s6_addr);
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    address,
                    u16::from_be(raw.sin6_port),
                    raw.sin6_flowinfo,
                    raw.sin6_scope_id,
                )))
            }
            _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use std::io;
    use std::net::SocketAddr;

    use tokio::net::UdpSocket;

    use super::{Family, LocalAddr};

    /// Other systems have no portable way to pin a reply's source address; the
    /// listener keeps its historical behaviour.
    pub(super) fn enable(_socket: &socket2::Socket, _family: Family) -> io::Result<()> {
        Ok(())
    }

    pub(super) async fn recv_from(
        socket: &UdpSocket,
        buffer: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, Option<LocalAddr>)> {
        let (length, peer) = socket.recv_from(buffer).await?;
        Ok((length, peer, None))
    }

    pub(super) async fn send_to(
        socket: &UdpSocket,
        data: &[u8],
        peer: SocketAddr,
        _local: Option<LocalAddr>,
    ) -> io::Result<usize> {
        socket.send_to(data, peer).await
    }
}

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(not(target_os = "linux"))]
use other as imp;

/// Enables destination-address reporting on a UDP socket before it is bound or
/// converted to a tokio socket.
pub(crate) fn enable(socket: &socket2::Socket, family: Family) -> io::Result<()> {
    imp::enable(socket, family)
}

/// Receives one datagram together with the address it was sent to.
pub(crate) async fn recv_from(
    socket: &UdpSocket,
    buffer: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<LocalAddr>)> {
    imp::recv_from(socket, buffer).await
}

/// Sends a datagram, pinning `local` as the source address when known.
pub(crate) async fn send_to(
    socket: &UdpSocket,
    data: &[u8],
    peer: SocketAddr,
    local: Option<LocalAddr>,
) -> io::Result<usize> {
    imp::send_to(socket, data, peer, local).await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use tokio::net::UdpSocket;

    use super::{Family, enable, recv_from, send_to};

    /// The reply of a wildcard listener has to leave from the address the query
    /// was sent to, not from the primary address of the interface.
    #[test]
    fn the_reply_leaves_from_the_queried_address() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let raw = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )
            .expect("wildcard socket");
            raw.set_nonblocking(true).expect("non-blocking");
            enable(&raw, Family::V4).expect("pktinfo");
            raw.bind(&"0.0.0.0:0".parse::<std::net::SocketAddr>().unwrap().into())
                .expect("bind");
            let server = UdpSocket::from_std(raw.into()).expect("tokio socket");
            let port = server.local_addr().expect("local address").port();

            let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
            client
                .send_to(b"ping", ("127.0.0.2", port))
                .await
                .expect("query");

            let mut query = [0u8; 32];
            let (length, peer, local) = recv_from(&server, &mut query).await.expect("receive");
            assert_eq!(&query[..length], b"ping");
            assert_eq!(
                local.map(|local| local.addr),
                Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)))
            );

            send_to(&server, b"pong", peer, local).await.expect("reply");
            let mut reply = [0u8; 32];
            let (length, from) = client.recv_from(&mut reply).await.expect("reply");
            assert_eq!(&reply[..length], b"pong");
            assert_eq!(from.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)));
        });
    }
}
