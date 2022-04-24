use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::BytesMut;
use mio::{event::Event, Poll, Token};
use smoltcp::{iface::SocketHandle, socket::UdpSocket, wire::IpEndpoint, Error};

use crate::{
    idle_pool::IdlePool,
    proto::{TrojanRequest, UdpAssociate, UdpParseResultEndpoint, UDP_ASSOCIATE},
    resolver::DnsResolver,
    tls_conn::TlsConn,
    utils::send_all,
    wintun::{waker::Wakers, SocketSet, CHANNEL_CNT, CHANNEL_UDP, MAX_INDEX, MIN_INDEX},
    OPTIONS,
};

fn next_token() -> Token {
    static mut NEXT_INDEX: usize = MIN_INDEX;
    unsafe {
        let index = NEXT_INDEX;
        NEXT_INDEX += 1;
        if NEXT_INDEX >= MAX_INDEX {
            NEXT_INDEX = MIN_INDEX;
        }
        Token(index * CHANNEL_CNT + CHANNEL_UDP)
    }
}

pub struct UdpSocketRef<'a, 'b> {
    socket: &'a mut UdpSocket<'b>,
    endpoint: Option<IpEndpoint>,
}

impl<'a, 'b> std::io::Read for UdpSocketRef<'a, 'b> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.socket.recv_slice(buf) {
            Ok((n, endpoint)) => {
                self.endpoint.replace(endpoint);
                Ok(n)
            }
            Err(Error::Exhausted) => Err(ErrorKind::WouldBlock.into()),
            Err(err) => Err(std::io::Error::new(ErrorKind::UnexpectedEof, err)),
        }
    }
}

impl<'a, 'b> std::io::Write for UdpSocketRef<'a, 'b> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let endpoint = self.endpoint.unwrap();
        match self.socket.send_slice(buf, endpoint) {
            Ok(()) => Ok(buf.len()),
            Err(Error::Exhausted) => Err(ErrorKind::WouldBlock.into()),
            Err(err) => Err(std::io::Error::new(ErrorKind::UnexpectedEof, err)),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Connection {
    token: Token,
    local: SocketHandle,
    remote: TlsConn,
    rclosed: bool,
    rbuffer: Vec<u8>,
    lbuffer: Vec<u8>,
    endpoint: IpEndpoint,
    established: bool,
}

impl Connection {
    fn do_local(&mut self, poll: &Poll, header: &[u8], body: &[u8]) {
        if !self.rbuffer.is_empty() {
            log::info!("send is blocked, discard udp packet");
            return;
        }
        if !self.established {
            log::info!("connection is not ready, cache request");
            self.rbuffer.extend_from_slice(header);
            self.rbuffer.extend_from_slice(body);
            return;
        }
        self.local_to_remote(poll, header, body);
    }

    fn do_remote(&mut self, poll: &Poll, socket: &mut UdpSocket, event: &Event) {
        if event.is_writable() {
            if !self.established {
                let mut buffer = BytesMut::new();
                TrojanRequest::generate(
                    &mut buffer,
                    UDP_ASSOCIATE,
                    OPTIONS.empty_addr.as_ref().unwrap(),
                );
                log::info!("sending {} bytes handshake data", buffer.len());
                if self.remote.write(buffer.as_ref()).is_ok() {
                    self.established = true;
                    log::info!("connection is ready now");
                } else {
                    self.close_remote(poll);
                    return;
                }
            }
            self.local_to_remote(poll, &[], &[]);
        }
        if event.is_readable() {
            self.remote_to_local(socket, poll);
        }
    }

    fn is_closed(&self) -> bool {
        self.rclosed
    }

    fn flush_remote(&mut self, poll: &Poll) {
        match self.remote.flush() {
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                log::info!("remote connection send blocked");
            }
            Err(err) => {
                log::info!("flush data to remote failed:{}", err);
                self.close_remote(poll);
            }
            Ok(()) => log::info!("flush data successfully"),
        }
    }

    fn local_to_remote(&mut self, poll: &Poll, header: &[u8], body: &[u8]) {
        if !self.rbuffer.is_empty() {
            log::info!("send cached {} raw bytes to remote tls", self.rbuffer.len());
            match send_all(&mut self.remote, &mut self.rbuffer) {
                Ok(true) => {
                    log::info!("send all completed");
                }
                Ok(false) => {
                    log::info!("last request not finished, discard new request");
                    self.flush_remote(poll);
                    return;
                }
                Err(err) => {
                    log::info!("remote connection break:{:?}", err);
                    self.close_remote(poll);
                    return;
                }
            }
        }
        let mut data = header;
        let mut offset = 0;
        while !data.is_empty() {
            log::info!("send {} bytes raw data to remote now", data.len());
            match self.remote.write(data) {
                Ok(0) => {
                    log::info!("remote connection break with 0 bytes");
                    self.close_remote(poll);
                    return;
                }
                Ok(n) => {
                    log::info!("send {} byte raw data", n);
                    offset += n;
                    data = &data[n..];
                    if data.is_empty() && offset == header.len() {
                        data = body;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    log::info!("write to remote blocked");
                    break;
                }
                Err(err) => {
                    log::info!("remote connection break:{:?}", err);
                    self.close_remote(poll);
                    return;
                }
            }
        }
        let remaining = header.len() + body.len() - offset;
        if remaining != 0 {
            log::info!("sending data {} bytes left, cache now", remaining);
            self.rbuffer.extend_from_slice(data);
            if data.len() < header.len() {
                self.rbuffer.extend_from_slice(body);
            }
        }
        self.flush_remote(poll);
    }

    fn remote_to_local(&mut self, socket: &mut UdpSocket, poll: &Poll) {
        let mut socket = UdpSocketRef {
            socket,
            endpoint: Some(self.endpoint),
        };

        loop {
            let offset = self.lbuffer.len();
            unsafe {
                self.lbuffer.set_len(self.lbuffer.capacity());
            }
            let mut closed = false;
            log::info!("copy remote data from {} to {}", offset, self.lbuffer.len());
            match self.remote.read(&mut self.lbuffer.as_mut_slice()[offset..]) {
                Ok(0) => {
                    log::info!("read 0 bytes from remote, close now");
                    closed = true;
                }
                Ok(n) => unsafe {
                    log::info!("read {} bytes raw data", n);
                    self.lbuffer.set_len(offset + n);
                },
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    log::info!("read from remote blocked");

                    unsafe {
                        self.lbuffer.set_len(offset);
                    }
                    return;
                }
                Err(err) => {
                    log::info!("remote closed with error:{}", err);
                    closed = true;
                }
            }

            if closed {
                self.close_remote(poll);
                return;
            }

            let mut buffer = self.lbuffer.as_slice();
            loop {
                match UdpAssociate::parse_endpoint(buffer) {
                    UdpParseResultEndpoint::Continued => {
                        let offset = self.lbuffer.len() - buffer.len();
                        if buffer.is_empty() {
                            self.lbuffer.clear();
                        } else {
                            let len = buffer.len();
                            self.lbuffer.copy_within(offset.., 0);
                            unsafe {
                                self.lbuffer.set_len(len);
                            }
                        }
                        log::info!("continue parsing with {} bytes left", self.lbuffer.len());
                        break;
                    }
                    UdpParseResultEndpoint::Packet(packet) => {
                        let payload = &packet.payload[..packet.length];
                        let _ = socket.write(payload);
                        log::info!("get one packet with size:{}", payload.len());
                        buffer = &packet.payload[packet.length..];
                    }
                    UdpParseResultEndpoint::InvalidProtocol => {
                        log::info!("invalid protocol close now");
                        self.close_remote(poll);
                        return;
                    }
                }
            }
        }
    }

    fn close_remote(&mut self, poll: &Poll) {
        if self.rclosed {
            return;
        }
        let _ = self.remote.close(poll);
        self.rclosed = true;
    }
}

pub struct UdpServer {
    token2conns: HashMap<Token, Arc<Connection>>,
    addr2conns: HashMap<IpEndpoint, Arc<Connection>>,
    sockets: HashMap<IpEndpoint, Instant>,
    buffer: BytesMut,
}

impl UdpServer {
    pub fn new() -> Self {
        Self {
            token2conns: Default::default(),
            addr2conns: Default::default(),
            buffer: BytesMut::with_capacity(1500),
            sockets: Default::default(),
        }
    }

    pub fn new_socket(&mut self, endpoint: IpEndpoint) -> bool {
        if self.sockets.contains_key(&endpoint) {
            false
        } else {
            self.sockets.insert(endpoint, Instant::now());
            true
        }
    }

    pub fn check_timeout(&mut self, now: Instant, sockets: &mut SocketSet) {
        let conns: Vec<_> = self
            .sockets
            .iter()
            .filter_map(|(endpoint, last_active)| {
                let elapsed = now - *last_active;
                if elapsed > Duration::from_secs(600) {
                    self.addr2conns.get(endpoint).map(|c| c.clone())
                } else {
                    None
                }
            })
            .collect();

        for conn in conns {
            self.sockets.remove(&conn.endpoint);
            sockets.remove_socket(conn.local);
            self.remove(conn);
        }
    }

    pub fn do_local(
        &mut self,
        pool: &mut IdlePool,
        poll: &Poll,
        resolver: &DnsResolver,
        wakers: &mut Wakers,
        sockets: &mut SocketSet,
    ) {
        for (handle, _) in wakers.get_events().iter() {
            let socket = sockets.get_socket::<UdpSocket>(*handle);
            let (rx_waker, _) = wakers.get_wakers(*handle);
            socket.register_recv_waker(rx_waker);
            let dst_endpoint = socket.endpoint();
            *self.sockets.get_mut(&dst_endpoint).unwrap() = Instant::now();
            while let Ok((data, src_endpoint)) = socket.recv() {
                self.buffer.clear();
                UdpAssociate::generate_endpoint(&mut self.buffer, &dst_endpoint, data.len() as u16);
                //self.buffer.extend_from_slice(data);
                log::info!(
                    "got udp request from {} to {} with {} bytes",
                    src_endpoint,
                    dst_endpoint,
                    data.len()
                );
                let conn = self.addr2conns.entry(src_endpoint).or_insert_with(|| {
                    let mut tls = pool.get(poll, resolver).unwrap();
                    tls.set_token(next_token(), poll);
                    let conn = Connection {
                        token: tls.token(),
                        local: *handle,
                        remote: tls,
                        rclosed: false,
                        rbuffer: Vec::with_capacity(1500), //TODO mtu
                        lbuffer: Vec::with_capacity(1500),
                        established: false,
                        endpoint: src_endpoint,
                    };
                    Arc::new(conn)
                });
                self.token2conns
                    .entry(conn.token)
                    .or_insert_with(|| conn.clone());
                unsafe {
                    Arc::get_mut_unchecked(conn).do_local(poll, self.buffer.as_ref(), data);
                }
                if conn.is_closed() {
                    let conn = conn.clone();
                    log::info!("connection:{} closed, remove now", conn.token.0);
                    self.remove(conn.clone());
                }
            }
        }
    }

    pub fn do_remote(&mut self, event: &Event, poll: &Poll, sockets: &mut SocketSet) {
        log::debug!("remote event for token:{}", event.token().0);
        if let Some(conn) = self.token2conns.get_mut(&event.token()) {
            let socket = sockets.get_socket::<UdpSocket>(conn.local);
            unsafe {
                Arc::get_mut_unchecked(conn).do_remote(poll, socket, event);
            }
            if conn.is_closed() {
                let conn = conn.clone();
                log::info!("connection:{} closed, remove now", conn.token.0);
                self.remove(conn);
            }
        }
    }
    fn remove(&mut self, conn: Arc<Connection>) {
        self.token2conns.remove(&conn.token);
        self.addr2conns.remove(&conn.endpoint);
    }
}