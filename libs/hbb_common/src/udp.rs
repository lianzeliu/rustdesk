use crate::ResultType;
use anyhow::{anyhow, Context};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use protobuf::Message;
use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;
use tokio::net::{lookup_host, ToSocketAddrs, UdpSocket};
use tokio_socks::{udp::Socks5UdpFramed, IntoTargetAddr, TargetAddr, ToProxyAddrs};
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

pub enum FramedSocket {
    Direct(UdpFramed<BytesCodec>),
    ProxySocks(Socks5UdpFramed),
}

fn new_socket(addr: SocketAddr, reuse: bool, buf_size: usize) -> Result<Socket, std::io::Error> {
    let socket = match addr {
        SocketAddr::V4(..) => Socket::new(Domain::ipv4(), Type::dgram(), None),
        SocketAddr::V6(..) => Socket::new(Domain::ipv6(), Type::dgram(), None),
    }?;
    if reuse {
        // windows has no reuse_port, but its reuse_address
        // almost equals to unix's reuse_port + reuse_address,
        // though may introduce nondeterministic behavior
        // illumos has no support for SO_REUSEPORT
        #[cfg(all(unix, not(target_os = "illumos")))]
        socket.set_reuse_port(true).ok();
        socket.set_reuse_address(true).ok();
    }
    // only nonblocking work with tokio, https://stackoverflow.com/questions/64649405/receiver-on-tokiompscchannel-only-receives-messages-when-buffer-is-full
    socket.set_nonblocking(true)?;
    if buf_size > 0 {
        socket.set_recv_buffer_size(buf_size).ok();
    }
    log::debug!(
        "Receive buf size of udp {}: {:?}",
        addr,
        socket.recv_buffer_size()
    );
    if addr.is_ipv6() && addr.ip().is_unspecified() && addr.port() > 0 {
        socket.set_only_v6(false).ok();
    }
    socket.bind(&addr.into())?;
    Ok(socket)
}

impl FramedSocket {
    pub async fn new<T: ToSocketAddrs>(addr: T) -> ResultType<Self> {
        Self::new_reuse(addr, false, 0).await
    }

    pub async fn new_reuse<T: ToSocketAddrs>(
        addr: T,
        reuse: bool,
        buf_size: usize,
    ) -> ResultType<Self> {
        let addr = lookup_host(&addr)
            .await?
            .next()
            .context("could not resolve to any address")?;
        Ok(Self::Direct(UdpFramed::new(
            UdpSocket::from_std(new_socket(addr, reuse, buf_size)?.into_udp_socket())?,
            BytesCodec::new(),
        )))
    }

    pub async fn new_proxy<'a, 't, P: ToProxyAddrs, T: ToSocketAddrs>(
        proxy: P,
        local: T,
        username: &'a str,
        password: &'a str,
        ms_timeout: u64,
    ) -> ResultType<Self> {
        let framed = if username.trim().is_empty() {
            super::timeout(ms_timeout, Socks5UdpFramed::connect(proxy, Some(local))).await??
        } else {
            super::timeout(
                ms_timeout,
                Socks5UdpFramed::connect_with_password(proxy, Some(local), username, password),
            )
            .await??
        };
        log::trace!(
            "Socks5 udp connected, local addr: {:?}, target addr: {}",
            framed.local_addr(),
            framed.socks_addr()
        );
        Ok(Self::ProxySocks(framed))
    }

    #[inline]
    pub async fn send(
        &mut self,
        msg: &impl Message,
        addr: impl IntoTargetAddr<'_>,
    ) -> ResultType<()> {
        let addr = addr.into_target_addr()?.to_owned();
        let send_data = Bytes::from(msg.write_to_bytes()?);
        match self {
            Self::Direct(f) => {
                if let TargetAddr::Ip(addr) = addr {
                    f.send((send_data, addr)).await?
                }
            }
            Self::ProxySocks(f) => f.send((send_data, addr)).await?,
        };
        Ok(())
    }

    // https://stackoverflow.com/a/68733302/1926020
    #[inline]
    pub async fn send_raw(
        &mut self,
        msg: &'static [u8],
        addr: impl IntoTargetAddr<'static>,
    ) -> ResultType<()> {
        let addr = addr.into_target_addr()?.to_owned();

        match self {
            Self::Direct(f) => {
                if let TargetAddr::Ip(addr) = addr {
                    f.send((Bytes::from(msg), addr)).await?
                }
            }
            Self::ProxySocks(f) => f.send((Bytes::from(msg), addr)).await?,
        };
        Ok(())
    }

    #[inline]
    pub async fn next(&mut self) -> Option<ResultType<(BytesMut, TargetAddr<'static>)>> {
        match self {
            Self::Direct(f) => match f.next().await {
                Some(Ok((data, addr))) => {
                    Some(Ok((data, addr.into_target_addr().ok()?.to_owned())))
                }
                Some(Err(e)) => Some(Err(anyhow!(e))),
                None => None,
            },
            Self::ProxySocks(f) => match f.next().await {
                Some(Ok((data, _))) => Some(Ok((data.data, data.dst_addr))),
                Some(Err(e)) => Some(Err(anyhow!(e))),
                None => None,
            },
        }
    }

    #[inline]
    pub async fn next_timeout(
        &mut self,
        ms: u64,
    ) -> Option<ResultType<(BytesMut, TargetAddr<'static>)>> {
        if let Ok(res) =
            tokio::time::timeout(std::time::Duration::from_millis(ms), self.next()).await
        {
            res
        } else {
            None
        }
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        if let FramedSocket::Direct(x) = self {
            if let Ok(v) = x.get_ref().local_addr() {
                return Some(v);
            }
        }
        None
    }
}

/// Adds the frame header `BytesCodec` writes, for the raw `UdpSocket` paths that talk to the
/// rendezvous server directly - the UDP NAT test and the UDP `PunchHoleSent`. hbbs decodes its
/// UDP with `BytesCodec`, so a bare protobuf datagram is never parsed and never answered, and
/// the socket's mapping is never learned.
pub fn frame_msg(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    let mut out = Vec::with_capacity(n + 4);
    if n <= 0x3F {
        out.push((n << 2) as u8);
    } else if n <= 0x3FFF {
        out.extend_from_slice(&(((n << 2) as u16) | 0x1).to_le_bytes());
    } else if n <= 0x3F_FFFF {
        let h = (n << 2) as u32 | 0x2;
        out.extend_from_slice(&(h as u16).to_le_bytes());
        out.push((h >> 16) as u8);
    } else {
        out.extend_from_slice(&(((n << 2) as u32) | 0x3).to_le_bytes());
    }
    out.extend_from_slice(data);
    out
}

/// The payload of a [`frame_msg`] datagram, or `None` when the header does not describe the
/// whole datagram - which is also how a bare, unframed reply is told apart from a framed one.
pub fn unframe_msg(data: &[u8]) -> Option<&[u8]> {
    let first = *data.first()?;
    let head_len = ((first & 0x3) + 1) as usize;
    if data.len() < head_len {
        return None;
    }
    let mut n = first as usize;
    if head_len > 1 {
        n |= (data[1] as usize) << 8;
    }
    if head_len > 2 {
        n |= (data[2] as usize) << 16;
    }
    if head_len > 3 {
        n |= (data[3] as usize) << 24;
    }
    if (n >> 2) != data.len() - head_len {
        return None;
    }
    Some(&data[head_len..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_datagrams_round_trip() {
        for len in [0usize, 1, 63, 64, 0x3FFF, 0x4000, 0x3F_FFFF, 0x40_0000] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let framed = frame_msg(&data);
            assert_eq!(unframe_msg(&framed), Some(&data[..]), "len {len}");
        }
    }

    #[test]
    fn a_bare_datagram_is_not_mistaken_for_a_frame() {
        let data = [0xAAu8, 0x01, 0x04, 0x08, 0xF8, 0xB2, 0x03];
        assert_eq!(unframe_msg(&data), None);
    }
}