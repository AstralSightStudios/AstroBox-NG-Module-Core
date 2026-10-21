use once_cell::sync::OnceCell;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct InterconnectMessage {
    pub device_addr: String,
    pub pkg_name: String,
    pub payload: Vec<u8>,
}

/// A message a Canopus module on the device sent to its listeners.
#[derive(Debug, Clone)]
pub struct CanopusConnectMessage {
    pub device_addr: String,
    pub module: String,
    pub payload: crate::device::canopus::connect::ConnectPayload,
}

#[derive(Debug, Clone)]
pub struct DeviceStateChanged {
    pub device_addr: String,
}

/// 手机里另有 App 在同一条 BLE 链路上和设备走同一套协议。iOS 上一台外设只有一条物理连接、
/// 所有 App 共用（后台的小米运动健康一直连着），设备分不清是谁写的，只认最后一次握手和认证，
/// 本端的会话已经作废。
#[derive(Debug, Clone)]
pub struct SharedLinkDetected {
    pub device_addr: String,
}

#[derive(Debug, Clone)]
pub enum CoreEvent {
    InterconnectMessage(InterconnectMessage),
    CanopusConnectMessage(CanopusConnectMessage),
    DeviceStateChanged(DeviceStateChanged),
    SharedLinkDetected(SharedLinkDetected),
}

const EVENT_CHANNEL_CAPACITY: usize = 64;

static EVENT_BUS: OnceCell<broadcast::Sender<CoreEvent>> = OnceCell::new();

fn event_sender() -> broadcast::Sender<CoreEvent> {
    EVENT_BUS
        .get_or_init(|| {
            let (tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
            tx
        })
        .clone()
}

pub fn subscribe() -> broadcast::Receiver<CoreEvent> {
    event_sender().subscribe()
}

pub fn emit(event: CoreEvent) {
    let _ = event_sender().send(event);
}
