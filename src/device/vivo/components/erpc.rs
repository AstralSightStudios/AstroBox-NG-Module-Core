use tokio::runtime::Handle;
use vivo_msgpack::{
    messages::generated::typed::{ERpcAskConnRequest, ERpcSetupConnRequest},
    msgpack::{MsgpackReader, write_bin},
};

use crate::{
    anyhow_site,
    device::vivo::{
        VivoConnectType, VivoDevice,
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::{Component, access::with_device_component_mut},
};

use super::shared::{HasVivoRequestContext, VivoRequestExt};

const BID_ERPC: u8 = 38;
const CID_ASK_CONN: u8 = 1;
const CID_SETUP_CONN: u8 = 2;
const CID_BUSINESS: u8 = 3;
const CID_ASK_CONN_RESPONSE: u8 = 0x81;
const CID_SETUP_CONN_RESPONSE: u8 = 0x82;
const CID_BUSINESS_RESPONSE: u8 = 0x83;

#[derive(Component)]
pub struct ErpcSystem {
    owner_id: String,
    tk_handle: Handle,
    next_client_sequence: u32,
    #[cfg(not(target_arch = "wasm32"))]
    socket_rpc: socket_rpc::ErpcSocketRpc,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ErpcNetworkSpeed {
    pub write: f64,
    pub read: f64,
}

impl ErpcSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            next_client_sequence: 1,
            #[cfg(not(target_arch = "wasm32"))]
            socket_rpc: socket_rpc::ErpcSocketRpc::new(),
        }
    }

    pub fn send_business_bytes(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let connect_type =
            with_device_component_mut::<VivoDevice, _, _>(self.owner_id.clone(), |dev| {
                dev.connect_type
            })
            .map_err(|err| anyhow_site!("failed to read vivo ERPC connect type: {err:?}"))?;
        let payload = match connect_type {
            VivoConnectType::SPP => data,
            VivoConnectType::BLE => {
                let mut payload = Vec::new();
                write_bin(&mut payload, &data)
                    .map_err(|err| anyhow_site!("failed to encode BLE ERPC payload: {err}"))?;
                payload
            }
        };
        self.send_vivo_message(
            VscpMessage::new(BID_ERPC, CID_BUSINESS, payload),
            "VivoErpcSystem::send_business_bytes",
        )
    }

    pub fn get_speed(&self) -> ErpcNetworkSpeed {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.socket_rpc.get_speed()
        }
        #[cfg(target_arch = "wasm32")]
        {
            ErpcNetworkSpeed::default()
        }
    }

    fn handle_ask_conn(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let req = ERpcAskConnRequest::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo ERPC ask-conn: {err}"))?;
        log::info!(
            "[VivoDevice.ERPC] ask-conn version={} channel_type={}",
            req.version,
            req.channel_type
        );
        update_erpc_component(&self.owner_id, move |comp| {
            comp.last_version = Some(req.version);
            comp.last_channel_type = Some(req.channel_type);
        })?;
        self.send_vivo_message(
            VscpMessage::new(BID_ERPC, CID_ASK_CONN_RESPONSE, Vec::new()),
            "VivoErpcSystem::ack_ask_conn",
        )?;
        if let Err(err) = self.send_network_status_notify() {
            log::warn!("[VivoDevice.ERPC] failed to send network status notify: {err:?}");
        }
        Ok(())
    }

    fn handle_setup_conn(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        ERpcSetupConnRequest::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo ERPC setup-conn: {err}"))?;
        log::info!("[VivoDevice.ERPC] setup-conn");
        #[cfg(not(target_arch = "wasm32"))]
        self.socket_rpc.set_connected(true);
        update_erpc_component(&self.owner_id, |comp| {
            comp.connected = true;
        })?;
        self.send_vivo_message(
            VscpMessage::new(BID_ERPC, CID_SETUP_CONN_RESPONSE, Vec::new()),
            "VivoErpcSystem::ack_setup_conn",
        )?;
        if let Err(err) = self.send_network_status_notify() {
            log::warn!("[VivoDevice.ERPC] failed to send network status notify: {err:?}");
        }
        Ok(())
    }

    fn handle_business(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let data = decode_business_payload(&message.payload);
        let len = data.len();
        log::debug!("[VivoDevice.ERPC] received business bytes={len}");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let responses = self.socket_rpc.handle_business_data(&data);
            for response in responses {
                self.send_business_bytes(response)?;
            }
        }
        update_erpc_component(&self.owner_id, move |comp| {
            comp.received_bytes = comp.received_bytes.saturating_add(len as u64);
            comp.last_business_payload = Some(data);
        })?;
        self.send_vivo_message(
            VscpMessage::new(BID_ERPC, CID_BUSINESS_RESPONSE, Vec::new()),
            "VivoErpcSystem::ack_business",
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn send_network_status_notify(&mut self) -> anyhow::Result<()> {
        let network_state = socket_rpc::detect_network_state();
        let sequence = self.next_client_sequence;
        self.next_client_sequence = self.next_client_sequence.wrapping_add(1);
        if self.next_client_sequence == 0 {
            self.next_client_sequence = 1;
        }
        let frame = socket_rpc::encode_network_status_notify(sequence, network_state);
        log::info!(
            "[VivoDevice.ERPC] notifying native network status state={} sequence={}",
            network_state,
            sequence
        );
        self.send_business_bytes(frame)?;
        update_erpc_component(&self.owner_id, move |comp| {
            comp.last_network_state = Some(network_state);
            comp.last_network_status_sequence = Some(sequence);
        })
    }

    #[cfg(target_arch = "wasm32")]
    fn send_network_status_notify(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl HasVivoRequestContext for ErpcSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for ErpcSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_ERPC {
            return;
        }

        let result = match message.cid {
            CID_ASK_CONN => self.handle_ask_conn(message),
            CID_SETUP_CONN => self.handle_setup_conn(message),
            CID_BUSINESS => self.handle_business(message),
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice.ERPC] message handling failed: {err:?}");
        }
    }
}

#[derive(Component, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErpcComponent {
    pub connected: bool,
    pub last_version: Option<i32>,
    pub last_channel_type: Option<i32>,
    pub last_network_state: Option<i32>,
    pub last_network_status_sequence: Option<u32>,
    pub received_bytes: u64,
    #[serde(skip_serializing)]
    pub last_business_payload: Option<Vec<u8>>,
}

impl ErpcComponent {
    pub fn new() -> Self {
        Self {
            connected: false,
            last_version: None,
            last_channel_type: None,
            last_network_state: None,
            last_network_status_sequence: None,
            received_bytes: 0,
            last_business_payload: None,
        }
    }
}

fn decode_business_payload(payload: &[u8]) -> Vec<u8> {
    let mut reader = MsgpackReader::new(payload);
    if matches!(reader.peek_marker(), Some(0xc4 | 0xc5 | 0xc6)) {
        match reader.read_bin() {
            Ok(data) if !reader.has_next() => return data,
            Ok(_) | Err(_) => {}
        }
    }
    payload.to_vec()
}

fn update_erpc_component<F>(owner_id: &str, f: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut ErpcComponent) + Send + 'static,
{
    with_device_component_mut::<ErpcComponent, _, _>(owner_id.to_string(), f)
        .map_err(|err| anyhow_site!("failed to update vivo ERPC component: {err:?}"))
}

#[cfg(not(target_arch = "wasm32"))]
mod socket_rpc {
    use std::{
        collections::{HashMap, VecDeque},
        io::{Read, Write},
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use super::ErpcNetworkSpeed;

    const ERPC_FRAME_MAGIC: [u8; 2] = [0x9f, 0xf9];
    const ERPC_FRAME_TRAILER: u8 = 0xa1;
    const ERPC_VERSION: u8 = 1;
    const ERPC_SERVICE_SOCKET: u8 = 1;
    const ERPC_SERVICE_NOTIFY: u8 = 2;
    const ERPC_MESSAGE_REQUEST: u8 = 0;
    const ERPC_MESSAGE_REPLY: u8 = 2;

    const METHOD_NOTIFY_NETWORK: u8 = 2;
    const METHOD_ACCEPT: u8 = 3;
    const METHOD_BIND: u8 = 4;
    const METHOD_SHUTDOWN: u8 = 5;
    const METHOD_GETPEERNAME: u8 = 6;
    const METHOD_GETSOCKNAME: u8 = 7;
    const METHOD_GETSOCKOPT: u8 = 8;
    const METHOD_SETSOCKOPT: u8 = 9;
    const METHOD_CONNECT: u8 = 10;
    const METHOD_LISTEN: u8 = 11;
    const METHOD_RECV: u8 = 12;
    const METHOD_RECVFROM: u8 = 13;
    const METHOD_SEND: u8 = 14;
    const METHOD_SENDTO: u8 = 15;
    const METHOD_SOCKET: u8 = 16;
    const METHOD_CLOSE: u8 = 17;
    const METHOD_IOCTL: u8 = 18;
    const METHOD_FCNTL: u8 = 19;
    const METHOD_GETHOSTBYNAME: u8 = 20;
    const METHOD_GETHOSTBYNAME_R: u8 = 21;
    const METHOD_GETRADDRINFO: u8 = 22;
    const METHOD_SELECT: u8 = 23;
    const METHOD_POLL: u8 = 24;

    const AF_INET: i32 = 2;
    const SOCK_STREAM: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    const POLLIN: u16 = 0x0001;
    const POLLOUT: u16 = 0x0004;
    const EAGAIN: i32 = 11;
    const EINVAL: i32 = 22;
    const EOPNOTSUPP: i32 = 95;
    const NETWORK_STATUS_PAYLOAD_LEN: usize = 0x84;
    const VIVO_NETWORK_NONE: i32 = 0;
    const VIVO_NETWORK_WIFI: i32 = 1;

    pub fn detect_network_state() -> i32 {
        if has_network_route() {
            VIVO_NETWORK_WIFI
        } else {
            VIVO_NETWORK_NONE
        }
    }

    pub fn encode_network_status_notify(sequence: u32, network_state: i32) -> Vec<u8> {
        let mut status = vec![0_u8; NETWORK_STATUS_PAYLOAD_LEN];
        status[0..4].copy_from_slice(&(network_state as u32).to_le_bytes());

        let mut writer = ErpcWriter::request(METHOD_NOTIFY_NETWORK, ERPC_SERVICE_NOTIFY, sequence);
        writer.write_binary(&status);
        encode_erpc_frame(writer.into_inner())
    }

    fn has_network_route() -> bool {
        std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|socket| socket.connect((Ipv4Addr::new(223, 5, 5, 5), 53)))
            .is_ok()
    }

    pub struct ErpcSocketRpc {
        connected: bool,
        next_fd: i32,
        sockets: HashMap<i32, RpcSocket>,
        frame_rx: ErpcFrameBuffer,
        meter: BandwidthMeter,
    }

    impl ErpcSocketRpc {
        pub fn new() -> Self {
            Self {
                connected: false,
                next_fd: 10_000,
                sockets: HashMap::new(),
                frame_rx: ErpcFrameBuffer::new(),
                meter: BandwidthMeter::new(Duration::from_secs(5)),
            }
        }

        pub fn set_connected(&mut self, connected: bool) {
            self.connected = connected;
        }

        pub fn get_speed(&self) -> ErpcNetworkSpeed {
            ErpcNetworkSpeed {
                write: self.meter.write_speed(),
                read: self.meter.read_speed(),
            }
        }

        pub fn handle_business_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
            let frames = self.frame_rx.push(data);
            let mut responses = Vec::new();
            for frame in frames {
                match ErpcMessage::decode(&frame) {
                    Ok(message) => {
                        if let Some(response) = self.handle_message(message) {
                            responses.push(response);
                        }
                    }
                    Err(err) => {
                        log::warn!(
                            "[VivoDevice.ERPC.Socket] failed to decode eRPC frame len={}: {}",
                            frame.len(),
                            err
                        );
                    }
                }
            }
            responses
        }

        fn handle_message(&mut self, message: ErpcMessage) -> Option<Vec<u8>> {
            if message.service != ERPC_SERVICE_SOCKET {
                log::trace!(
                    "[VivoDevice.ERPC.Socket] ignoring service={} method={} seq={}",
                    message.service,
                    message.method,
                    message.sequence
                );
                return None;
            }

            let method = message.method;
            let seq = message.sequence;
            let mut reader = ErpcReader::new(&message.body, seq);
            let response = match method {
                METHOD_ACCEPT => self.rpc_accept(&mut reader),
                METHOD_BIND => self.rpc_bind(&mut reader),
                METHOD_GETPEERNAME => self.rpc_getname(&mut reader, method),
                METHOD_GETSOCKNAME => self.rpc_getname(&mut reader, method),
                METHOD_LISTEN => self.rpc_listen(&mut reader),
                METHOD_SOCKET => self.rpc_socket(&mut reader),
                METHOD_CONNECT => self.rpc_connect(&mut reader),
                METHOD_SEND => self.rpc_send(&mut reader, method),
                METHOD_SENDTO => self.rpc_send(&mut reader, method),
                METHOD_RECV => self.rpc_recv(&mut reader, method),
                METHOD_RECVFROM => self.rpc_recv(&mut reader, method),
                METHOD_CLOSE => self.rpc_close(&mut reader),
                METHOD_SHUTDOWN => self.rpc_shutdown(&mut reader),
                METHOD_GETSOCKOPT => self.rpc_getsockopt(&mut reader),
                METHOD_SETSOCKOPT => self.rpc_setsockopt(&mut reader),
                METHOD_IOCTL => self.rpc_ioctl(&mut reader),
                METHOD_FCNTL => self.rpc_fcntl(&mut reader),
                METHOD_GETHOSTBYNAME => self.rpc_gethostbyname(&mut reader),
                METHOD_GETHOSTBYNAME_R => self.rpc_gethostbyname_r(&mut reader),
                METHOD_GETRADDRINFO => self.rpc_getraddrinfo(&mut reader),
                METHOD_SELECT => self.rpc_select(&mut reader),
                METHOD_POLL => self.rpc_poll(&mut reader),
                _ => {
                    log::debug!(
                        "[VivoDevice.ERPC.Socket] unsupported method={} seq={}",
                        method,
                        seq
                    );
                    Ok(simple_i32_response(method, seq, -1))
                }
            };

            match response {
                Ok(payload) => Some(encode_erpc_frame(payload)),
                Err(err) => {
                    log::warn!(
                        "[VivoDevice.ERPC.Socket] method={} seq={} failed: {}",
                        method,
                        seq,
                        err
                    );
                    Some(encode_erpc_frame(simple_i32_response(
                        method,
                        seq,
                        errno_result(EINVAL),
                    )))
                }
            }
        }

        fn rpc_socket(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let domain = reader.read_i32()?;
            let socket_type = reader.read_i32()?;
            let protocol = reader.read_i32()?;
            let fd = self.next_fd;
            self.next_fd = self.next_fd.saturating_add(1);
            self.sockets.insert(
                fd,
                RpcSocket {
                    domain,
                    socket_type,
                    protocol,
                    stream: None,
                    send_count: 0,
                    recv_count: 0,
                    nonblocking: false,
                },
            );
            log::info!(
                "[VivoDevice.ERPC.Socket] socket fd={} domain={} type={} protocol={}",
                fd,
                domain,
                socket_type,
                protocol
            );
            Ok(simple_i32_response(METHOD_SOCKET, reader.sequence(), fd))
        }

        fn rpc_accept(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let _fd = reader.read_i32()?;
            let _sockaddr = reader.read_sockaddr16()?;
            let mut writer = ErpcWriter::response(METHOD_ACCEPT, reader.sequence());
            writer.write_i32(16);
            writer.write_i32(errno_result(EOPNOTSUPP));
            Ok(writer.into_inner())
        }

        fn rpc_bind(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let _sockaddr = reader.read_sockaddr16()?;
            let _addr_len = reader.read_i32()?;
            let result = if self.sockets.contains_key(&fd) {
                0
            } else {
                errno_result(EINVAL)
            };
            Ok(simple_i32_response(METHOD_BIND, reader.sequence(), result))
        }

        fn rpc_connect(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let sockaddr = reader.read_sockaddr16()?;
            let _addr_len = reader.read_i32()?;
            let result = match self.sockets.get_mut(&fd) {
                Some(socket)
                    if socket.domain == AF_INET
                        && socket.socket_type == SOCK_STREAM
                        && matches!(socket.protocol, 0 | IPPROTO_TCP) =>
                {
                    match sockaddr_to_socket_addr(&sockaddr) {
                        Some(addr) => {
                            match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
                                Ok(stream) => {
                                    let _ = stream.set_nodelay(true);
                                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                                    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                                    socket.stream = Some(stream);
                                    log::info!(
                                        "[VivoDevice.ERPC.Socket] connect fd={} addr={}",
                                        fd,
                                        addr
                                    );
                                    0
                                }
                                Err(err) => {
                                    log::warn!(
                                        "[VivoDevice.ERPC.Socket] connect fd={} addr={} failed: {}",
                                        fd,
                                        addr,
                                        err
                                    );
                                    io_errno_result(&err)
                                }
                            }
                        }
                        None => errno_result(EINVAL),
                    }
                }
                Some(_) | None => errno_result(EINVAL),
            };
            Ok(simple_i32_response(
                METHOD_CONNECT,
                reader.sequence(),
                result,
            ))
        }

        fn rpc_getname(
            &mut self,
            reader: &mut ErpcReader<'_>,
            method: u8,
        ) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let mut sockaddr = [0_u8; 16];
            let result = match self.sockets.get(&fd) {
                Some(socket) => match socket.stream.as_ref() {
                    Some(stream) => {
                        let addr = if method == METHOD_GETPEERNAME {
                            stream.peer_addr()
                        } else {
                            stream.local_addr()
                        };
                        match addr {
                            Ok(SocketAddr::V4(addr)) => {
                                sockaddr = sockaddr_from_ipv4(*addr.ip(), addr.port());
                                0
                            }
                            Ok(SocketAddr::V6(_)) => errno_result(EINVAL),
                            Err(err) => io_errno_result(&err),
                        }
                    }
                    None => errno_result(EINVAL),
                },
                None => errno_result(EINVAL),
            };
            let mut writer = ErpcWriter::response(method, reader.sequence());
            writer.write_sockaddr16(&sockaddr);
            writer.write_i32(16);
            writer.write_i32(result);
            Ok(writer.into_inner())
        }

        fn rpc_listen(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let _backlog = reader.read_i32()?;
            let result = if self.sockets.contains_key(&fd) {
                errno_result(EOPNOTSUPP)
            } else {
                errno_result(EINVAL)
            };
            Ok(simple_i32_response(
                METHOD_LISTEN,
                reader.sequence(),
                result,
            ))
        }

        fn rpc_send(&mut self, reader: &mut ErpcReader<'_>, method: u8) -> Result<Vec<u8>, String> {
            let mut stat = SocketStat::read(reader)?;
            let payload = reader.read_binary()?;
            let _flags = reader.read_i32()?;
            if method == METHOD_SENDTO {
                let _sockaddr = reader.read_sockaddr16()?;
                let _addr_len = reader.read_i32()?;
            }

            let result = match self.sockets.get_mut(&stat.fd) {
                Some(socket) => {
                    socket.send_count = stat.send_count;
                    socket.recv_count = stat.recv_count;
                    match socket.stream.as_mut() {
                        Some(stream) => match stream.write(&payload) {
                            Ok(written) => {
                                self.meter.add_read(written);
                                socket.send_count = socket.send_count.saturating_add(1);
                                stat.send_count = socket.send_count;
                                written as i32
                            }
                            Err(err) => io_errno_result(&err),
                        },
                        None => errno_result(EINVAL),
                    }
                }
                None => errno_result(EINVAL),
            };

            let mut writer = ErpcWriter::response(method, reader.sequence());
            stat.write(&mut writer);
            writer.write_i32(result);
            Ok(writer.into_inner())
        }

        fn rpc_recv(&mut self, reader: &mut ErpcReader<'_>, method: u8) -> Result<Vec<u8>, String> {
            let mut stat = SocketStat::read(reader)?;
            let requested = reader.read_u32()?.min(64 * 1024) as usize;
            let _flags = reader.read_i32()?;

            let mut data = Vec::new();
            let result = match self.sockets.get_mut(&stat.fd) {
                Some(socket) => match socket.stream.as_mut() {
                    Some(stream) => {
                        let mut buf = vec![0_u8; requested.max(1)];
                        match stream.read(&mut buf) {
                            Ok(read) => {
                                buf.truncate(read);
                                data = buf;
                                if read > 0 {
                                    self.meter.add_written(read);
                                    socket.recv_count = socket.recv_count.saturating_add(1);
                                    stat.recv_count = socket.recv_count;
                                }
                                read as i32
                            }
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                errno_result(EAGAIN)
                            }
                            Err(err) => io_errno_result(&err),
                        }
                    }
                    None => errno_result(EINVAL),
                },
                None => errno_result(EINVAL),
            };

            let mut writer = ErpcWriter::response(method, reader.sequence());
            stat.write(&mut writer);
            writer.write_binary(&data);
            if method == METHOD_RECVFROM {
                writer.write_sockaddr16(&[0; 16]);
                writer.write_i32(16);
            }
            writer.write_i32(result);
            Ok(writer.into_inner())
        }

        fn rpc_close(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            self.sockets.remove(&fd);
            log::debug!("[VivoDevice.ERPC.Socket] close fd={fd}");
            Ok(simple_i32_response(METHOD_CLOSE, reader.sequence(), 0))
        }

        fn rpc_shutdown(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let _how = reader.read_i32()?;
            self.sockets.remove(&fd);
            Ok(simple_i32_response(METHOD_SHUTDOWN, reader.sequence(), 0))
        }

        fn rpc_getsockopt(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let _fd = reader.read_i32()?;
            let _level = reader.read_i32()?;
            let _optname = reader.read_i32()?;
            let mut writer = ErpcWriter::response(METHOD_GETSOCKOPT, reader.sequence());
            writer.write_list_len(1);
            writer.write_u64(0);
            writer.write_i32(8);
            writer.write_i32(0);
            Ok(writer.into_inner())
        }

        fn rpc_setsockopt(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let _fd = reader.read_i32()?;
            let _level = reader.read_i32()?;
            let _optname = reader.read_i32()?;
            reader.skip_u64_list()?;
            let _optlen = reader.read_i32()?;
            Ok(simple_i32_response(METHOD_SETSOCKOPT, reader.sequence(), 0))
        }

        fn rpc_ioctl(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let _fd = reader.read_i32()?;
            let _cmd = reader.read_i32()?;
            if !reader.read_null_flag()? {
                reader.skip_u64_list()?;
            }
            Ok(simple_i32_response(METHOD_IOCTL, reader.sequence(), 0))
        }

        fn rpc_fcntl(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let cmd = reader.read_i32()?;
            let mut arg0 = None;
            if !reader.read_null_flag()? {
                let args = reader.read_u64_list()?;
                arg0 = args.first().copied();
            }

            let result = match cmd {
                3 => self
                    .sockets
                    .get(&fd)
                    .map(|socket| if socket.nonblocking { 0x800 } else { 0 })
                    .unwrap_or_else(|| errno_result(EINVAL)),
                4 => {
                    if let Some(socket) = self.sockets.get_mut(&fd) {
                        socket.nonblocking =
                            arg0.is_some_and(|value| value & 0x4000 != 0 || value & 0x800 != 0);
                        if let Some(stream) = socket.stream.as_ref() {
                            let _ = stream.set_nonblocking(socket.nonblocking);
                        }
                        0
                    } else {
                        errno_result(EINVAL)
                    }
                }
                _ => 0,
            };
            Ok(simple_i32_response(METHOD_FCNTL, reader.sequence(), result))
        }

        fn rpc_gethostbyname(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let host = reader.read_string()?;
            let resolved = resolve_ipv4(&host, 0);
            let mut writer = ErpcWriter::response(METHOD_GETHOSTBYNAME, reader.sequence());
            write_hostent(&mut writer, &host, resolved);
            Ok(writer.into_inner())
        }

        fn rpc_gethostbyname_r(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let host = reader.read_string()?;
            let _buf_len = reader.read_i32()?;
            let resolved = resolve_ipv4(&host, 0);
            let mut writer = ErpcWriter::response(METHOD_GETHOSTBYNAME_R, reader.sequence());
            write_hostent(&mut writer, &host, resolved);
            write_hostent(&mut writer, &host, resolved);
            writer.write_i32(if resolved.is_some() { 0 } else { EINVAL });
            writer.write_i32(if resolved.is_some() {
                0
            } else {
                errno_result(EINVAL)
            });
            Ok(writer.into_inner())
        }

        fn rpc_getraddrinfo(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let node = reader.read_string()?;
            let service = reader.read_string()?;
            let _ai_flags = reader.read_i32()?;
            let ai_family = reader.read_i32()?;
            let ai_socktype = reader.read_i32()?;
            let ai_protocol = reader.read_i32()?;
            let _ai_addrlen = reader.read_i32()?;
            let _ai_addr = reader.read_sockaddr16()?;
            if !reader.read_null_flag()? {
                let _canon = reader.read_string()?;
            }

            let port = service.parse::<u16>().unwrap_or(0);
            let resolved = if node.trim().is_empty() {
                Some(Ipv4Addr::UNSPECIFIED)
            } else {
                resolve_ipv4(&node, port)
            };

            let mut writer = ErpcWriter::response(METHOD_GETRADDRINFO, reader.sequence());
            if let Some(ip) = resolved {
                let family = if ai_family == 0 { AF_INET } else { ai_family };
                let socktype = if ai_socktype == 0 {
                    SOCK_STREAM
                } else {
                    ai_socktype
                };
                let protocol = if ai_protocol == 0 {
                    IPPROTO_TCP
                } else {
                    ai_protocol
                };
                writer.write_list_len(1);
                writer.write_i32(0);
                writer.write_i32(family);
                writer.write_i32(socktype);
                writer.write_i32(protocol);
                writer.write_i32(16);
                writer.write_sockaddr16(&sockaddr_from_ipv4(ip, port));
                writer.write_null_flag(false);
                writer.write_string(&node);
                writer.write_i32(0);
            } else {
                writer.write_list_len(0);
                writer.write_i32(errno_result(EINVAL));
            }
            Ok(writer.into_inner())
        }

        fn rpc_select(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let _nfds = reader.read_i32()?;
            let readfds = reader.read_optional_fdset()?;
            let writefds = reader.read_optional_fdset()?;
            let exceptfds = reader.read_optional_fdset()?;
            if !reader.read_null_flag()? {
                let _sec = reader.read_i64()?;
                let _usec = reader.read_i64()?;
            }
            let mut writer = ErpcWriter::response(METHOD_SELECT, reader.sequence());
            let ready = readfds.is_some() || writefds.is_some() || exceptfds.is_some();
            if let Some(set) = readfds {
                writer.write_bytes(&set);
            }
            if let Some(set) = writefds {
                writer.write_bytes(&set);
            }
            if let Some(set) = exceptfds {
                writer.write_bytes(&set);
            }
            writer.write_i32(if ready { 1 } else { 0 });
            Ok(writer.into_inner())
        }

        fn rpc_poll(&mut self, reader: &mut ErpcReader<'_>) -> Result<Vec<u8>, String> {
            let fd = reader.read_i32()?;
            let events = reader.read_u16()?;
            let _revents = reader.read_u16()?;
            let _nfds = reader.read_i32()?;
            let _timeout = reader.read_i32()?;
            let exists = self.sockets.contains_key(&fd);
            let revents = if exists {
                events & (POLLIN | POLLOUT)
            } else {
                0
            };
            let mut writer = ErpcWriter::response(METHOD_POLL, reader.sequence());
            writer.write_i32(fd);
            writer.write_u16(events);
            writer.write_u16(revents);
            writer.write_i32(if revents == 0 { 0 } else { 1 });
            Ok(writer.into_inner())
        }
    }

    struct RpcSocket {
        domain: i32,
        socket_type: i32,
        protocol: i32,
        stream: Option<TcpStream>,
        send_count: u32,
        recv_count: u32,
        nonblocking: bool,
    }

    #[derive(Clone)]
    struct BandwidthMeter {
        window: Duration,
        write_events: Arc<Mutex<VecDeque<(Instant, u64)>>>,
        read_events: Arc<Mutex<VecDeque<(Instant, u64)>>>,
    }

    impl BandwidthMeter {
        fn new(window: Duration) -> Self {
            Self {
                window,
                write_events: Arc::new(Mutex::new(VecDeque::new())),
                read_events: Arc::new(Mutex::new(VecDeque::new())),
            }
        }

        fn evict_old(&self, queue: &mut VecDeque<(Instant, u64)>, now: Instant) {
            while let Some(&(timestamp, _)) = queue.front() {
                if now.duration_since(timestamp) > self.window {
                    queue.pop_front();
                } else {
                    break;
                }
            }
        }

        fn push_event(&self, queue: &Arc<Mutex<VecDeque<(Instant, u64)>>>, length: usize) {
            let now = Instant::now();
            let mut guard = queue.lock().expect("Vivo ERPC bandwidth meter poisoned");
            guard.push_back((now, length as u64));
            self.evict_old(&mut guard, now);
        }

        fn add_written(&self, length: usize) {
            self.push_event(&self.write_events, length);
        }

        fn add_read(&self, length: usize) {
            self.push_event(&self.read_events, length);
        }

        fn speed_inner(&self, queue: &Arc<Mutex<VecDeque<(Instant, u64)>>>) -> f64 {
            let now = Instant::now();
            let mut guard = queue.lock().expect("Vivo ERPC bandwidth meter poisoned");
            self.evict_old(&mut guard, now);
            let total: u64 = guard.iter().map(|(_, bytes)| *bytes).sum();
            if total == 0 {
                0.0
            } else if let Some(&(first, _)) = guard.front() {
                let elapsed = now
                    .checked_duration_since(first)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.001)
                    .max(0.001);
                total as f64 / elapsed
            } else {
                0.0
            }
        }

        fn write_speed(&self) -> f64 {
            self.speed_inner(&self.write_events)
        }

        fn read_speed(&self) -> f64 {
            self.speed_inner(&self.read_events)
        }
    }

    #[derive(Clone, Copy)]
    struct SocketStat {
        fd: i32,
        send_count: u32,
        recv_count: u32,
    }

    impl SocketStat {
        fn read(reader: &mut ErpcReader<'_>) -> Result<Self, String> {
            Ok(Self {
                fd: reader.read_i32()?,
                send_count: reader.read_u32()?,
                recv_count: reader.read_u32()?,
            })
        }

        fn write(self, writer: &mut ErpcWriter) {
            writer.write_i32(self.fd);
            writer.write_u32(self.send_count);
            writer.write_u32(self.recv_count);
        }
    }

    struct ErpcFrameBuffer {
        pending: Vec<u8>,
    }

    impl ErpcFrameBuffer {
        fn new() -> Self {
            Self {
                pending: Vec::new(),
            }
        }

        fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
            if data.len() >= 8
                && data.get(0..2) != Some(&ERPC_FRAME_MAGIC)
                && data.get(3) == Some(&ERPC_VERSION)
            {
                return vec![data.to_vec()];
            }

            self.pending.extend_from_slice(data);
            let mut frames = Vec::new();
            loop {
                if self.pending.len() < 4 {
                    break;
                }
                if self.pending.get(0..2) != Some(&ERPC_FRAME_MAGIC) {
                    if let Some(pos) = self
                        .pending
                        .windows(2)
                        .position(|window| window == ERPC_FRAME_MAGIC)
                    {
                        self.pending.drain(0..pos);
                    } else {
                        let raw = std::mem::take(&mut self.pending);
                        if !raw.is_empty() {
                            frames.push(raw);
                        }
                        break;
                    }
                }
                if self.pending.len() < 4 {
                    break;
                }
                let body_len = u16::from_le_bytes([self.pending[2], self.pending[3]]) as usize;
                let total = 4 + body_len;
                if self.pending.len() < total {
                    break;
                }
                let mut body = self.pending[4..total].to_vec();
                if body.last() == Some(&ERPC_FRAME_TRAILER) {
                    body.pop();
                }
                frames.push(body);
                self.pending.drain(0..total);
            }
            frames
        }
    }

    struct ErpcMessage {
        method: u8,
        service: u8,
        sequence: u32,
        body: Vec<u8>,
    }

    impl ErpcMessage {
        fn decode(frame: &[u8]) -> Result<Self, String> {
            if frame.len() < 8 {
                return Err("frame shorter than eRPC header".to_string());
            }
            if frame[3] != ERPC_VERSION {
                return Err(format!("unsupported eRPC version {}", frame[3]));
            }
            Ok(Self {
                method: frame[1],
                service: frame[2],
                sequence: u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
                body: frame[8..].to_vec(),
            })
        }
    }

    struct ErpcReader<'a> {
        data: &'a [u8],
        pos: usize,
        sequence: u32,
    }

    impl<'a> ErpcReader<'a> {
        fn new(data: &'a [u8], sequence: u32) -> Self {
            Self {
                data,
                pos: 0,
                sequence,
            }
        }

        fn sequence(&self) -> u32 {
            self.sequence
        }

        fn read_exact(&mut self, len: usize) -> Result<&'a [u8], String> {
            let end = self
                .pos
                .checked_add(len)
                .ok_or_else(|| "eRPC reader offset overflow".to_string())?;
            if end > self.data.len() {
                return Err(format!(
                    "eRPC payload underflow need={} remaining={}",
                    len,
                    self.data.len().saturating_sub(self.pos)
                ));
            }
            let out = &self.data[self.pos..end];
            self.pos = end;
            Ok(out)
        }

        fn read_u8(&mut self) -> Result<u8, String> {
            Ok(self.read_exact(1)?[0])
        }

        fn read_u16(&mut self) -> Result<u16, String> {
            let data = self.read_exact(2)?;
            Ok(u16::from_le_bytes([data[0], data[1]]))
        }

        fn read_i32(&mut self) -> Result<i32, String> {
            Ok(self.read_u32()? as i32)
        }

        fn read_u32(&mut self) -> Result<u32, String> {
            let data = self.read_exact(4)?;
            Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
        }

        fn read_i64(&mut self) -> Result<i64, String> {
            Ok(self.read_u64()? as i64)
        }

        fn read_u64(&mut self) -> Result<u64, String> {
            let data = self.read_exact(8)?;
            Ok(u64::from_le_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]))
        }

        fn read_binary(&mut self) -> Result<Vec<u8>, String> {
            let len = self.read_u32()? as usize;
            Ok(self.read_exact(len)?.to_vec())
        }

        fn read_string(&mut self) -> Result<String, String> {
            let data = self.read_binary()?;
            String::from_utf8(data).map_err(|err| format!("invalid eRPC UTF-8 string: {err}"))
        }

        fn read_sockaddr16(&mut self) -> Result<[u8; 16], String> {
            let mut out = [0_u8; 16];
            for item in &mut out {
                *item = self.read_u8()?;
            }
            Ok(out)
        }

        fn read_null_flag(&mut self) -> Result<bool, String> {
            Ok(self.read_u8()? != 0)
        }

        fn read_u64_list(&mut self) -> Result<Vec<u64>, String> {
            let len = self.read_u32()? as usize;
            let mut out = Vec::with_capacity(len);
            for _ in 0..len {
                out.push(self.read_u64()?);
            }
            Ok(out)
        }

        fn skip_u64_list(&mut self) -> Result<(), String> {
            let _ = self.read_u64_list()?;
            Ok(())
        }

        fn read_optional_fdset(&mut self) -> Result<Option<[u8; 128]>, String> {
            if self.read_null_flag()? {
                return Ok(None);
            }
            let mut out = [0_u8; 128];
            for chunk in out.chunks_exact_mut(4) {
                chunk.copy_from_slice(&self.read_u32()?.to_le_bytes());
            }
            Ok(Some(out))
        }
    }

    struct ErpcWriter {
        data: Vec<u8>,
    }

    impl ErpcWriter {
        fn request(method: u8, service: u8, sequence: u32) -> Self {
            let mut data = Vec::new();
            data.push(ERPC_MESSAGE_REQUEST);
            data.push(method);
            data.push(service);
            data.push(ERPC_VERSION);
            data.extend_from_slice(&sequence.to_le_bytes());
            Self { data }
        }

        fn response(method: u8, sequence: u32) -> Self {
            let mut data = Vec::new();
            data.push(ERPC_MESSAGE_REPLY);
            data.push(method);
            data.push(ERPC_SERVICE_SOCKET);
            data.push(ERPC_VERSION);
            data.extend_from_slice(&sequence.to_le_bytes());
            Self { data }
        }

        fn write_bytes(&mut self, bytes: &[u8]) {
            self.data.extend_from_slice(bytes);
        }

        fn write_u16(&mut self, value: u16) {
            self.data.extend_from_slice(&value.to_le_bytes());
        }

        fn write_i32(&mut self, value: i32) {
            self.write_u32(value as u32);
        }

        fn write_u32(&mut self, value: u32) {
            self.data.extend_from_slice(&value.to_le_bytes());
        }

        fn write_u64(&mut self, value: u64) {
            self.data.extend_from_slice(&value.to_le_bytes());
        }

        fn write_binary(&mut self, value: &[u8]) {
            self.write_list_len(value.len() as u32);
            self.write_bytes(value);
        }

        fn write_string(&mut self, value: &str) {
            self.write_binary(value.as_bytes());
        }

        fn write_list_len(&mut self, len: u32) {
            self.write_u32(len);
        }

        fn write_null_flag(&mut self, is_null: bool) {
            self.data.push(if is_null { 1 } else { 0 });
        }

        fn write_sockaddr16(&mut self, value: &[u8; 16]) {
            for byte in value {
                self.data.push(*byte);
            }
        }

        fn into_inner(self) -> Vec<u8> {
            self.data
        }
    }

    fn simple_i32_response(method: u8, sequence: u32, value: i32) -> Vec<u8> {
        let mut writer = ErpcWriter::response(method, sequence);
        writer.write_i32(value);
        writer.into_inner()
    }

    fn encode_erpc_frame(mut message: Vec<u8>) -> Vec<u8> {
        message.push(ERPC_FRAME_TRAILER);
        let len = message.len().min(u16::MAX as usize);
        let mut out = Vec::with_capacity(4 + len);
        out.extend_from_slice(&ERPC_FRAME_MAGIC);
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&message[..len]);
        out
    }

    fn sockaddr_to_socket_addr(sockaddr: &[u8; 16]) -> Option<SocketAddr> {
        let family = u16::from_le_bytes([sockaddr[0], sockaddr[1]]) as i32;
        if family != AF_INET {
            return None;
        }
        let port = u16::from_be_bytes([sockaddr[2], sockaddr[3]]);
        let ip = Ipv4Addr::new(sockaddr[4], sockaddr[5], sockaddr[6], sockaddr[7]);
        Some(SocketAddr::new(IpAddr::V4(ip), port))
    }

    fn sockaddr_from_ipv4(ip: Ipv4Addr, port: u16) -> [u8; 16] {
        let mut out = [0_u8; 16];
        out[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        out[2..4].copy_from_slice(&port.to_be_bytes());
        out[4..8].copy_from_slice(&ip.octets());
        out
    }

    fn write_hostent(writer: &mut ErpcWriter, host: &str, ip: Option<Ipv4Addr>) {
        match ip {
            Some(ip) => {
                writer.write_null_flag(false);
                writer.write_string(host);
                writer.write_list_len(0);
                writer.write_i32(AF_INET);
                writer.write_i32(4);
                writer.write_list_len(1);
                writer.write_binary(&ip.octets());
            }
            None => writer.write_null_flag(true),
        }
    }

    fn resolve_ipv4(host: &str, port: u16) -> Option<Ipv4Addr> {
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            return Some(ip);
        }
        let query = format!("{}:{}", host, port);
        query
            .to_socket_addrs()
            .ok()?
            .find_map(|addr| match addr.ip() {
                IpAddr::V4(ip) => Some(ip),
                IpAddr::V6(_) => None,
            })
    }

    fn io_errno_result(err: &std::io::Error) -> i32 {
        errno_result(err.raw_os_error().unwrap_or(EINVAL))
    }

    fn errno_result(errno: i32) -> i32 {
        -(errno.abs().saturating_add(1))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn request_frame(method: u8, sequence: u32, body: Vec<u8>) -> Vec<u8> {
            let mut message = vec![1, method, ERPC_SERVICE_SOCKET, ERPC_VERSION];
            message.extend_from_slice(&sequence.to_le_bytes());
            message.extend_from_slice(&body);
            encode_erpc_frame(message)
        }

        fn response_body(frame: &[u8]) -> Vec<u8> {
            assert_eq!(frame.get(0..2), Some(&ERPC_FRAME_MAGIC[..]));
            let len = u16::from_le_bytes([frame[2], frame[3]]) as usize;
            let mut body = frame[4..4 + len].to_vec();
            if body.last() == Some(&ERPC_FRAME_TRAILER) {
                body.pop();
            }
            body
        }

        fn push_i32(out: &mut Vec<u8>, value: i32) {
            out.extend_from_slice(&(value as u32).to_le_bytes());
        }

        fn push_u32(out: &mut Vec<u8>, value: u32) {
            out.extend_from_slice(&value.to_le_bytes());
        }

        #[test]
        fn socket_response_preserves_erpc_sequence() {
            let mut rpc = ErpcSocketRpc::new();
            let sequence = 0x1122_3344;
            let mut body = Vec::new();
            push_i32(&mut body, AF_INET);
            push_i32(&mut body, SOCK_STREAM);
            push_i32(&mut body, IPPROTO_TCP);

            let responses = rpc.handle_business_data(&request_frame(METHOD_SOCKET, sequence, body));

            assert_eq!(responses.len(), 1);
            let body = response_body(&responses[0]);
            assert_eq!(
                &body[0..8],
                &[
                    ERPC_MESSAGE_REPLY,
                    METHOD_SOCKET,
                    ERPC_SERVICE_SOCKET,
                    ERPC_VERSION,
                    0x44,
                    0x33,
                    0x22,
                    0x11
                ]
            );
            let fd = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            assert!(fd >= 10_000);
        }

        #[test]
        fn network_status_notify_uses_official_service_shape() {
            let sequence = 0x5566_7788;
            let frame = encode_network_status_notify(sequence, VIVO_NETWORK_WIFI);
            let body = response_body(&frame);

            assert_eq!(
                &body[0..8],
                &[
                    ERPC_MESSAGE_REQUEST,
                    METHOD_NOTIFY_NETWORK,
                    ERPC_SERVICE_NOTIFY,
                    ERPC_VERSION,
                    0x88,
                    0x77,
                    0x66,
                    0x55,
                ]
            );
            assert_eq!(
                u32::from_le_bytes([body[8], body[9], body[10], body[11]]) as usize,
                NETWORK_STATUS_PAYLOAD_LEN
            );
            assert_eq!(
                i32::from_le_bytes([body[12], body[13], body[14], body[15]]),
                VIVO_NETWORK_WIFI
            );
            assert!(body[16..].iter().all(|value| *value == 0));
        }

        #[test]
        fn recvfrom_request_does_not_require_input_sockaddr() {
            let mut rpc = ErpcSocketRpc::new();
            let sequence = 7;
            let mut body = Vec::new();
            push_i32(&mut body, 1234);
            push_u32(&mut body, 0);
            push_u32(&mut body, 0);
            push_u32(&mut body, 16);
            push_i32(&mut body, 0);

            let responses =
                rpc.handle_business_data(&request_frame(METHOD_RECVFROM, sequence, body));

            assert_eq!(responses.len(), 1);
            let body = response_body(&responses[0]);
            assert_eq!(
                &body[0..8],
                &[
                    ERPC_MESSAGE_REPLY,
                    METHOD_RECVFROM,
                    ERPC_SERVICE_SOCKET,
                    ERPC_VERSION,
                    7,
                    0,
                    0,
                    0
                ]
            );
            assert_eq!(body.len(), 48);
            let result = i32::from_le_bytes([
                body[body.len() - 4],
                body[body.len() - 3],
                body[body.len() - 2],
                body[body.len() - 1],
            ]);
            assert_eq!(result, errno_result(EINVAL));
        }
    }
}
