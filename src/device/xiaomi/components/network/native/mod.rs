use std::{
    fs::{self, File},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use chrono::Local;
use etherparse::{Icmpv4Header, Icmpv4Type};
use ipstack::{IpNumber, IpStack, IpStackConfig, IpStackStream};
use pb::xiaomi::protocol;
use pcap_file::pcap::PcapWriter;
use tokio::sync::mpsc::error::TrySendError;
use tokio::{
    io::{self, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::{mpsc, watch},
    time,
};
use tokio_util::sync::PollSender;
use udp_stream::UdpStream;

use crate::{
    anyhow_site,
    device::xiaomi::{
        XiaomiDevice,
        config::NetworkConfig,
        packet::{
            self,
            v2::layer2::{L2Channel, L2OpCode, L2Packet},
        },
        system::{XiaomiSystemExt, register_xiaomi_system_ext_on_l2packet},
    },
    ecs::{
        entity::EntityExt,
        fastlane::FastLane,
        logic_component::LogicCompMeta,
        system::{SysMeta, System},
    },
    impl_has_sys_meta, impl_logic_component,
};

mod dhcp;
mod meter;
mod tun;

use dhcp::maybe_build_reply;
use meter::BandwidthMeter;
use tun::MiWearTunDevice;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct NetWorkSpeed {
    pub write: f64,
    pub read: f64,
}

#[derive(serde::Serialize)]
pub struct NetworkComponent {
    #[serde(skip_serializing)]
    meta: LogicCompMeta,
    #[serde(skip_serializing)]
    config: NetworkConfig,
    pub last_speed: NetWorkSpeed,
}

impl NetworkComponent {
    pub const ID: &'static str = "MiWearDeviceNetworkLogicComponent";

    pub fn new(config: NetworkConfig) -> Self {
        Self {
            meta: LogicCompMeta::new::<NetworkSystem>(Self::ID),
            config,
            last_speed: NetWorkSpeed::default(),
        }
    }

    pub fn config(&self) -> &NetworkConfig {
        &self.config
    }

    pub fn ensure_stack(&mut self, handle: Handle) -> Result<()> {
        let system = self
            .meta
            .system
            .as_any_mut()
            .downcast_mut::<NetworkSystem>()
            .expect("NetworkComponent misconfigured");
        system.ensure_runtime(handle, self.config.clone())
    }
}

impl_logic_component!(NetworkComponent, meta);

pub struct NetworkSystem {
    meta: SysMeta,
    runtime: Option<NetworkRuntime>,
}

impl Default for NetworkSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
            runtime: None,
        }
    }
}

impl NetworkSystem {
    fn ensure_runtime(&mut self, handle: Handle, config: NetworkConfig) -> Result<()> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let owner = self
            .owner()
            .ok_or_else(|| anyhow_site!("NetworkSystem missing owner"))?
            .to_string();
        let runtime = NetworkRuntime::new(owner, config, handle)?;
        self.runtime = Some(runtime);
        Ok(())
    }

    pub fn sync_network_status(&mut self) -> Result<()> {
        let this: &mut dyn System = self;

        FastLane::with_entity_mut::<(), _>(this, |ent| {
            let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
            packet::cipher::enqueue_pb_packet(
                dev,
                build_sync_network_status(),
                "NetworkComponent::sync_network_status",
            );

            Ok(())
        })
        .map_err(|err| anyhow_site!("failed to access resource config: {:?}", err))?;

        Ok(())
    }
}

impl XiaomiSystemExt for NetworkSystem {
    fn on_layer2_packet(&mut self, channel: L2Channel, _opcode: L2OpCode, payload: &[u8]) {
        if channel != L2Channel::Network {
            return;
        }

        log::info!(
            "Received network packet: {}",
            crate::tools::to_hex_string(payload)
        );

        if let Some(runtime) = self.runtime.as_ref() {
            if let Err(err) = runtime.push_inbound(payload.to_vec()) {
                match err {
                    IngressError::Closed => {
                        log::debug!("[NetworkSystem] ingress closed, dropping packet");
                    }
                    IngressError::Backpressure => {
                        log::warn!("[NetworkSystem] ingress overwhelmed, dropping packet");
                    }
                }
            }
        } else {
            log::trace!("[NetworkSystem] runtime not ready; dropping network packet");
        }
    }
}

impl_has_sys_meta!(NetworkSystem, meta);

fn build_sync_network_status() -> protocol::WearPacket {
    let network_status = protocol::NetworkStatus { capability: 2 };

    let pkt_payload = protocol::System {
        payload: Some(protocol::system::Payload::NetworkStatus(network_status)),
    };

    let pkt = protocol::WearPacket {
        r#type: protocol::wear_packet::Type::System as i32,
        id: protocol::system::SystemId::SyncNetworkStatus as u32,
        payload: Some(protocol::wear_packet::Payload::System(pkt_payload)),
    };

    pkt
}

struct NetworkRuntime {
    ingress_tx: mpsc::Sender<Vec<u8>>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<crate::asyncrt::TaskHandle>,
}

impl NetworkRuntime {
    fn new(owner: String, config: NetworkConfig, handle: Handle) -> Result<Self> {
        let ingress_capacity = config.ingress_buffer.max(1);
        let tun_capacity = config.tun_buffer.max(1);
        let outbound_capacity = config.outbound_buffer.max(1);

        let (ingress_tx, mut ingress_rx) = mpsc::channel::<Vec<u8>>(ingress_capacity);
        let (tun_tx, tun_rx) = mpsc::channel::<Vec<u8>>(tun_capacity);
        let (send_tx, mut send_rx) = mpsc::channel::<Vec<u8>>(outbound_capacity);
        let capture = prepare_capture_writer(&owner, &config);
        let meter_window = Duration::from_secs(config.meter_window_secs.max(1));
        let meter = BandwidthMeter::new(meter_window);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let mut tasks = Vec::new();

        // 入口循环：设备 -> 协议栈（带 DHCP 处理）
        {
            let owner_clone = owner.clone();
            let mut shutdown = shutdown_rx.clone();
            tasks.push(crate::asyncrt::spawn_with_handle(
                async move {
                    loop {
                        tokio::select! {
                            packet = ingress_rx.recv() => {
                                match packet {
                                    Some(data) => {
                                        match maybe_build_reply(&data) {
                                            Ok(Some(reply)) => {
                                                if let Err(err) = enqueue_network_payload(&owner_clone, reply).await {
                                                    log::error!("[NetworkRuntime] failed to send DHCP reply: {err:?}");
                                                }
                                                continue;
                                            }
                                            Ok(None) => {}
                                            Err(err) => log::warn!("[NetworkRuntime] DHCP parse error: {err:?}"),
                                        }

                                        if let Err(err) = tun_tx.send(data).await {
                                            log::warn!("[NetworkRuntime] tun channel closed: {err}");
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            changed = shutdown.changed() => {
                                if changed.is_ok() {
                                    break;
                                }
                            }
                        }
                    }
                },
                handle.clone(),
            ));
        }

        // 发送循环：协议栈 -> 设备
        {
            let owner_clone = owner.clone();
            let mut shutdown = shutdown_rx.clone();
            tasks.push(crate::asyncrt::spawn_with_handle(
                async move {
                    loop {
                        tokio::select! {
                            packet = send_rx.recv() => {
                                match packet {
                                    Some(payload) => {
                                        if let Err(err) = enqueue_network_payload(&owner_clone, payload).await {
                                            log::error!("[NetworkRuntime] failed to send network payload: {err:?}");
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            changed = shutdown.changed() => {
                                if changed.is_ok() {
                                    break;
                                }
                            }
                        }
                    }
                },
                handle.clone(),
            ));
        }

        // 流量计数器循环
        {
            let owner_clone = owner.clone();
            let meter_clone = meter.clone();
            let mut shutdown = shutdown_rx.clone();
            tasks.push(crate::asyncrt::spawn_with_handle(
                async move {
                    let mut ticker = time::interval(Duration::from_secs(1));
                    loop {
                        tokio::select! {
                            _ = ticker.tick() => {
                                let speed = NetWorkSpeed {
                                    write: meter_clone.write_speed(),
                                    read: meter_clone.read_speed(),
                                };
                                update_speed(&owner_clone, speed).await;
                            }
                            changed = shutdown.changed() => {
                                if changed.is_ok() {
                                    break;
                                }
                            }
                        }
                    }
                },
                handle.clone(),
            ));
        }

        // IpStack 处理循环
        {
            let owner_clone = owner.clone();
            let mut shutdown = shutdown_rx.clone();
            let meter_for_stack = meter.clone();
            let capture = capture;
            let send_tx_clone = send_tx.clone();
            let config_for_stack = config.clone();
            tasks.push(crate::asyncrt::spawn_with_handle(
                async move {
                    let session_count = Arc::new(AtomicUsize::new(0));
                    let serial = Arc::new(AtomicUsize::new(0));
                    let poll_sender = PollSender::new(send_tx_clone);
                    let tun_device = MiWearTunDevice {
                        rx: tun_rx,
                        tx_send: poll_sender,
                        capture,
                        meter: meter_for_stack,
                    };
                    let mut stack_cfg = IpStackConfig::default();
                    stack_cfg.mtu(config_for_stack.mtu);
                    let mut ip_stack = IpStack::new(stack_cfg, tun_device);
                    log::info!(
                        "[NetworkRuntime] network stack started for {} (mtu={})",
                        owner_clone,
                        config_for_stack.mtu
                    );
                    loop {
                        tokio::select! {
                            accept_res = ip_stack.accept() => {
                                match accept_res {
                                    Ok(stream) => {
                                        let id = serial.fetch_add(1, Ordering::Relaxed);
                                        match stream {
                                            IpStackStream::Tcp(mut tcp) => {
                                                let mut peer = match TcpStream::connect(tcp.peer_addr()).await {
                                                    Ok(stream) => stream,
                                                    Err(err) => {
                                                        log::warn!("[NetworkRuntime] TCP connect failed: {err}");
                                                        continue;
                                                    }
                                                };
                                                let count = session_count.fetch_add(1, Ordering::Relaxed) + 1;
                                                log::info!("[NetworkRuntime] TCP#{id} established, sessions={count}");
                                                let counter = session_count.clone();
                                                crate::asyncrt::spawn(async move {
                                                    if let Err(err) = io::copy_bidirectional(&mut tcp, &mut peer).await {
                                                        log::info!("[NetworkRuntime] TCP#{id} ended with error: {err}");
                                                    }
                                                    let _ = peer.shutdown().await;
                                                    let _ = tcp.shutdown().await;
                                                    let remaining = counter.fetch_sub(1, Ordering::Relaxed) - 1;
                                                    log::info!("[NetworkRuntime] TCP#{id} closed, sessions={remaining}");
                                                });
                                            }
                                            IpStackStream::Udp(mut udp) => {
                                                let local_addr = udp.local_addr();
                                                let remote_addr = udp.peer_addr();
                                                let mut peer = match UdpStream::connect(remote_addr).await {
                                                    Ok(stream) => stream,
                                                    Err(err) => {
                                                        log::warn!("[NetworkRuntime] UDP connect failed {local_addr} -> {remote_addr}: {err}");
                                                        continue;
                                                    }
                                                };
                                                let count = session_count.fetch_add(1, Ordering::Relaxed) + 1;
                                                log::info!(
                                                    "[NetworkRuntime] UDP#{id} established {} -> {}, sessions={count}",
                                                    local_addr,
                                                    remote_addr
                                                );
                                                let counter = session_count.clone();
                                                crate::asyncrt::spawn({
                                                    let local_addr = local_addr;
                                                    let remote_addr = remote_addr;
                                                    async move {
                                                        if let Err(err) = io::copy_bidirectional(&mut udp, &mut peer).await {
                                                            log::info!(
                                                                "[NetworkRuntime] UDP#{id} ended with error: {err} ({} -> {})",
                                                                local_addr,
                                                                remote_addr
                                                            );
                                                        }
                                                        peer.shutdown();
                                                        let _ = udp.shutdown().await;
                                                        let remaining = counter.fetch_sub(1, Ordering::Relaxed) - 1;
                                                        log::info!(
                                                            "[NetworkRuntime] UDP#{id} closed, sessions={remaining} ({} -> {})",
                                                            local_addr,
                                                            remote_addr
                                                        );
                                                    }
                                                });
                                            }
                                            IpStackStream::UnknownTransport(pkt) => {
                                                if pkt.src_addr().is_ipv4()
                                                    && pkt.ip_protocol() == IpNumber::ICMP
                                                {
                                                    if let Ok((header, payload)) =
                                                        Icmpv4Header::from_slice(pkt.payload())
                                                    {
                                                        if let Icmpv4Type::EchoRequest(echo) =
                                                            header.icmp_type
                                                        {
                                                            let mut response = Icmpv4Header::new(
                                                                Icmpv4Type::EchoReply(echo),
                                                            );
                                                            response.update_checksum(payload);
                                                            let mut bytes =
                                                                response.to_bytes().to_vec();
                                                            bytes.extend_from_slice(payload);
                                                            if let Err(err) = pkt.send(bytes) {
                                                                log::warn!(
                                                                    "[NetworkRuntime] ICMP send failed: {err}"
                                                                );
                                                            } else {
                                                                log::info!(
                                                                    "[NetworkRuntime] ICMP echo replied"
                                                                );
                                                            }
                                                            continue;
                                                        }
                                                    }
                                                }
                                                log::debug!(
                                                    "[NetworkRuntime] unknown transport {:?}",
                                                    pkt.ip_protocol()
                                                );
                                            }
                                            IpStackStream::UnknownNetwork(pkt) => {
                                                log::debug!("[NetworkRuntime] unknown network payload ({} bytes)", pkt.len());
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        log::error!("[NetworkRuntime] IpStack halted with error: {err}");
                                        break;
                                    }
                                }
                            }
                            changed = shutdown.changed() => {
                                if changed.is_ok() {
                                    log::info!("[NetworkRuntime] shutting down network stack for {}", owner_clone);
                                    break;
                                }
                            }
                        }
                    }
                },
                handle,
            ));
        }

        Ok(Self {
            ingress_tx,
            shutdown: shutdown_tx,
            tasks,
        })
    }

    fn push_inbound(&self, packet: Vec<u8>) -> Result<(), IngressError> {
        self.ingress_tx.try_send(packet).map_err(|err| match err {
            TrySendError::Full(_) => IngressError::Backpressure,
            TrySendError::Closed(_) => IngressError::Closed,
        })
    }
}

impl Drop for NetworkRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks.drain(..) {
            #[cfg(not(target_arch = "wasm32"))]
            task.abort();
        }
    }
}

#[derive(Debug)]
enum IngressError {
    Backpressure,
    Closed,
}

async fn enqueue_network_payload(owner: &str, payload: Vec<u8>) -> Result<()> {
    let owner_id = owner.to_string();
    crate::ecs::with_rt_mut(move |rt| {
        let dev = rt
            .find_entity_by_id_mut::<XiaomiDevice>(&owner_id)
            .ok_or_else(|| anyhow_site!("device {} not found for network send", owner_id))?;
        dev.sar
            .enqueue(L2Packet::new(L2Channel::Network, L2OpCode::Write, payload).to_bytes());
        Ok(())
    })
    .await
}

async fn update_speed(owner: &str, speed: NetWorkSpeed) {
    let owner_id = owner.to_string();
    let _ = crate::ecs::with_rt_mut(move |rt| {
        if let Some(dev) = rt.find_entity_by_id_mut::<XiaomiDevice>(&owner_id) {
            if let Ok(comp) = dev.get_component_as_mut::<NetworkComponent>(NetworkComponent::ID) {
                comp.last_speed = speed;
            }
        }
    })
    .await;
}

fn prepare_capture_writer(owner: &str, config: &NetworkConfig) -> Option<PcapWriter<File>> {
    if !config.enable_capture {
        return None;
    }
    let base_dir: PathBuf = config
        .capture_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("astrobox_rslogs"));
    if let Err(err) = fs::create_dir_all(&base_dir) {
        log::warn!(
            "[NetworkRuntime] failed to create pcap dir {}: {}",
            base_dir.display(),
            err
        );
        return None;
    }
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let sanitized_owner = owner.replace(':', "_");
    let file_path = base_dir.join(format!("{sanitized_owner}_{timestamp}.pcap"));
    match File::create(&file_path) {
        Ok(file) => match PcapWriter::new(file) {
            Ok(writer) => Some(writer),
            Err(err) => {
                log::warn!(
                    "[NetworkRuntime] failed to initialise pcap writer {}: {}",
                    file_path.display(),
                    err
                );
                None
            }
        },
        Err(err) => {
            log::warn!(
                "[NetworkRuntime] failed to create pcap file {}: {}",
                file_path.display(),
                err
            );
            None
        }
    }
}
