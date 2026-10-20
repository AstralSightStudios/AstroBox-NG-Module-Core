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

#[derive(Debug, Clone)]
pub enum CoreEvent {
    InterconnectMessage(InterconnectMessage),
    CanopusConnectMessage(CanopusConnectMessage),
    DeviceStateChanged(DeviceStateChanged),
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
