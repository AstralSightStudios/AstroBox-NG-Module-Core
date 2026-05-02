use std::{future::Future, pin::Pin, sync::Arc};

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::{
    asyncrt::universal_block_on,
    device::{
        Device, DeviceKind,
        xiaomi::{
            config::XiaomiDeviceConfig,
            packet::{cipher, dispatcher},
            r#type::ConnectType,
        },
    },
    ecs::Component,
};
use parking_lot::Mutex as ParkingMutex;
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use transport_profiler::TransportProfilerHandle;

pub mod components;
pub mod config;
pub mod packet;
pub mod resutils;
pub mod sar;
pub mod system;
pub mod transport_profiler;
pub mod r#type;

#[derive(Debug)]
pub enum SendError {
    Disconnected,
    Io(String),
}

type SendFuture = Pin<Box<dyn Future<Output = Result<(), SendError>> + Send>>;
type SendFn = Arc<dyn Fn(Vec<Vec<u8>>) -> SendFuture + Send + Sync>;
const SPP_STREAM_SEND_COALESCE_CAP: usize = 60 * 1024;

#[derive(Component, serde::Serialize)]
pub struct XiaomiDevice {
    #[serde(flatten)]
    device: Device,
    pub sar_version: u32,          // SAR 协议版本，对应SPP v?
    pub connect_type: ConnectType, // 连接类型，SPP or BLE
    pub force_android: bool, // 安卓人安卓代码安卓生态安卓手表安卓设备安卓pb 在连接设备时强制使用ANDROID作为设备类型
    #[serde(skip_serializing)]
    sender: SendFn,
    #[serde(skip_serializing)]
    pub transport_profiler: TransportProfilerHandle,
    #[serde(skip_serializing)]
    pub sar: ParkingMutex<sar::SarController>,
    pub config: XiaomiDeviceConfig,
}

pub fn cleanup_cached_state(device_id: &str) {
    cipher::remove_l2_cipher(device_id);
    dispatcher::clear_recv_buffer(device_id);
}

impl XiaomiDevice {
    pub fn new<F, Fut>(
        tk_handle: Handle,
        name: String,
        addr: String,
        _authkey: String,
        sar_version: u32,
        connect_type: ConnectType,
        force_android: bool,
        config: XiaomiDeviceConfig,
        sender: F,
    ) -> Self
    where
        F: Fn(Vec<Vec<u8>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), SendError>> + Send + 'static,
    {
        let transport_profiler = TransportProfilerHandle::new();
        // 包装线程安全Sender
        let raw_sender: SendFn = Arc::new(move |data: Vec<Vec<u8>>| Box::pin(sender(data)));
        // 上锁防止串串包
        let send_lock = Arc::new(AsyncMutex::new(()));
        let transport_config = config.transport.clone();
        let sender: SendFn = {
            let raw_sender = raw_sender.clone();
            let send_lock = send_lock.clone();
            let profiler = transport_profiler.clone();
            Arc::new(move |data: Vec<Vec<u8>>| {
                let raw_sender = raw_sender.clone();
                let send_lock = send_lock.clone();
                let profiler = profiler.clone();
                let chunk_size_ble = transport_config.chunk_size_ble;
                let chunk_size_spp = transport_config.chunk_size_spp;
                Box::pin(async move {
                    let _guard = send_lock.lock().await;

                    let chunk_size_max = if connect_type == ConnectType::SPP {
                        chunk_size_spp.max(SPP_STREAM_SEND_COALESCE_CAP)
                    } else {
                        chunk_size_ble
                    };

                    let mut chunks = Vec::new();
                    for packet in data {
                        if packet.len() <= chunk_size_max {
                            chunks.push(packet);
                        } else {
                            chunks
                                .extend(packet.chunks(chunk_size_max).map(|chunk| chunk.to_vec()));
                        }
                    }
                    let packet_count = chunks.len() as u32;
                    let total_bytes = chunks.iter().map(|chunk| chunk.len() as u64).sum::<u64>();
                    let started_at = Instant::now();
                    let result = raw_sender(chunks).await;
                    profiler.record(
                        "transport",
                        if connect_type == ConnectType::BLE {
                            "send_batch_ble"
                        } else {
                            "send_batch_spp"
                        },
                        Some(
                            started_at
                                .elapsed()
                                .as_millis()
                                .try_into()
                                .unwrap_or(u64::MAX),
                        ),
                        Some(packet_count),
                        Some(total_bytes),
                        None,
                        Some(result.is_ok()),
                        Some(format!(
                            "chunk_size_max={},connect_type={:?}",
                            chunk_size_max, connect_type
                        )),
                    );
                    result
                })
            })
        };

        // 不知道为什么傻逼小米针对SPP连接要发这么一个神秘Hello
        if connect_type == ConnectType::SPP {
            universal_block_on(|| async {
                sender(vec![
                    crate::tools::hex_stream_to_bytes("badcfe00c00300000100ef").unwrap(),
                ])
                .await
                .unwrap();
            });
        }

        let base = Device::new(name, addr, DeviceKind::Xiaomi);
        // 创建 SAR 控制器，并传入设备名以便定时任务访问
        let sar = sar::SarController::new(
            tk_handle.clone(),
            sender.clone(),
            base.addr().to_string(),
            transport_profiler.clone(),
            config.sar.clone(),
        );

        let dev = Self {
            device: base,
            sar_version,
            connect_type,
            force_android,
            sender,
            transport_profiler,
            sar: ParkingMutex::new(sar),
            config,
        };
        dev
    }

    pub async fn send_data(&self, data: Vec<u8>) -> Result<(), SendError> {
        (self.sender)(vec![data]).await
    }

    pub fn base(&self) -> &Device {
        &self.device
    }

    pub fn base_mut(&mut self) -> &mut Device {
        &mut self.device
    }

    pub fn name(&self) -> &str {
        self.device.name()
    }

    pub fn addr(&self) -> &str {
        self.device.addr()
    }
}
