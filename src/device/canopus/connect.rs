//! CanopusConnect (ADR-CAN-009): text/binary messages between a Canopus module
//! on the band and the plugins that address it by module id.
//!
//! The messages are their own CPC2 opcodes on the Canopus L2 channel, so they
//! share the stream with the Manager RPC without touching its state:
//!
//! * `ConnectSend` (phone -> module): `kind | id_len | id | data`, answered by
//!   one status byte.
//! * `ConnectListen` (phone -> device): `enable | id_len | id`; the device only
//!   pushes a module's messages while a listener is registered, and the
//!   registration lapses with the phone session.
//! * `ConnectMessage` (device -> phone EVENT): `kind | id_len | id | data`.
//!
//! One message is one frame of at most [`DATA_MAX`] bytes.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use once_cell::sync::OnceCell;
use tokio::sync::OwnedSemaphorePermit;

use super::{
    CanopusCommand, CanopusResult, CanopusSystem, ConcurrencyHandles, Cpc2Event, Cpc2Request,
    Cpc2Response, MAX_CONNECT_DATA_SLOTS, MAX_CONNECT_PLUGIN_SLOTS, PendingRequest,
    is_valid_module_id, request,
};

pub const DATA_MAX: usize = 480;
const KIND_TEXT: u8 = 1;
const KIND_BINARY: u8 = 2;
const TIMEOUT: Duration = Duration::from_secs(5);

const STATUS_MODULE: u8 = 1;
const STATUS_NOT_LISTENING: u8 = 2;
const STATUS_FULL: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPayload {
    Text(String),
    Binary(Vec<u8>),
}

impl ConnectPayload {
    fn kind(&self) -> u8 {
        match self {
            Self::Text(_) => KIND_TEXT,
            Self::Binary(_) => KIND_BINARY,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Binary(data) => data,
        }
    }
}

fn module_bytes(module: &str) -> Result<&[u8], String> {
    if !is_valid_module_id(module) {
        return Err(format!("Invalid Canopus module id '{module}'"));
    }
    Ok(module.as_bytes())
}

/// `kind-or-enable | id_len | id | data`, the layout shared by all three opcodes.
fn frame(first: u8, module: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let id = module_bytes(module)?;
    if data.len() > DATA_MAX {
        return Err(format!(
            "CanopusConnect message is {} bytes; at most {DATA_MAX} fit in one frame",
            data.len()
        ));
    }
    let mut out = Vec::with_capacity(2 + id.len() + data.len());
    out.push(first);
    out.push(id.len() as u8);
    out.extend_from_slice(id);
    out.extend_from_slice(data);
    Ok(out)
}

pub fn encode_send(module: &str, payload: &ConnectPayload) -> Result<Vec<u8>, String> {
    frame(payload.kind(), module, payload.bytes())
}

pub fn encode_listen(module: &str, enable: bool) -> Result<Vec<u8>, String> {
    frame(enable as u8, module, &[])
}

/// A message the device pushed for a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMessage {
    pub module: String,
    pub payload: ConnectPayload,
}

pub fn decode_message(payload: &[u8]) -> Result<ModuleMessage, String> {
    if payload.len() < 2 {
        return Err("CanopusConnect frame is too short".into());
    }
    let id_len = payload[1] as usize;
    if id_len == 0 || id_len > 16 || payload.len() < 2 + id_len {
        return Err("CanopusConnect frame has a bad module id length".into());
    }
    let module = std::str::from_utf8(&payload[2..2 + id_len])
        .map_err(|_| "CanopusConnect module id is not UTF-8")?
        .to_string();
    if !is_valid_module_id(&module) {
        return Err(format!("Invalid Canopus module id '{module}' in message"));
    }
    let data = &payload[2 + id_len..];
    if data.len() > DATA_MAX {
        return Err("CanopusConnect frame carries too much data".into());
    }
    let payload = match payload[0] {
        KIND_TEXT => ConnectPayload::Text(
            String::from_utf8(data.to_vec())
                .map_err(|_| "CanopusConnect text message is not UTF-8")?,
        ),
        KIND_BINARY => ConnectPayload::Binary(data.to_vec()),
        kind => return Err(format!("Unknown CanopusConnect message kind {kind}")),
    };
    Ok(ModuleMessage { module, payload })
}

/// Reads the response of a CONNECT request: `Ok` only for COMPLETED. An empty
/// refusal is the answer of a Supervisor that predates CanopusConnect.
fn interpret(module: &str, response: &Cpc2Response) -> Result<(), String> {
    if response.result == CanopusResult::Completed {
        return Ok(());
    }
    match (response.result, response.payload.first().copied()) {
        (_, None) => Err("This Canopus Supervisor does not support CanopusConnect; update it".into()),
        (CanopusResult::Disallowed, Some(STATUS_MODULE)) => {
            Err(format!("Canopus module '{module}' is not installed"))
        }
        (CanopusResult::Disallowed, Some(STATUS_NOT_LISTENING)) => Err(format!(
            "Canopus module '{module}' is not listening for messages"
        )),
        (CanopusResult::Disallowed, Some(STATUS_FULL)) => {
            Err("The Canopus device cannot take more listeners".into())
        }
        (result, status) => Err(format!(
            "Canopus device refused the CanopusConnect request ({result:?}, status {status:?})"
        )),
    }
}

/// Sends `payload` to the module and returns once the device handed it to the
/// module's listener.
///
/// A plugin that issues several sends at once (an import streams a thousand frames) keeps up to
/// [`MAX_CONNECT_PLUGIN_SLOTS`] of them on their way -- more on a device that reports a deeper
/// request ring (see [`learn_window`]): each is queued to the transport in the order it was asked
/// for, and holds a device RPC slot until the device has answered it, so the device's queue cannot
/// overflow and a Manager request always has a slot left. The window is the plugins' own: it
/// neither draws on the CRPack data window nor is starved by it. A caller that awaits each send
/// before the next still sees strictly one message at a time.
pub async fn send(addr: &str, module: &str, payload: ConnectPayload) -> Result<(), String> {
    // A locally registered listener can outlive a failed reconnect replay. Do not send a
    // request whose reply the device would discard as NOT_LISTENING.
    if count(addr, module) != 0 && resync_needed(addr) {
        let _guard = subscription_lock().lock().await;
        if resync_needed(addr) {
            resync_locked(addr).await?;
        }
    }
    ConnectWindowSender::plugin(addr, module)?
        .submit(Lane::Plugin, payload, TIMEOUT)
        .await?
        .await
}

/// Which budget a send draws on, besides the device's 4 RPC slots.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Session control (HELLO/OPEN/QUERY/FINISH/CANCEL): the slot kept free of any data window.
    Control,
    /// A windowed session's DATA: the session's window and the device's data slots.
    Data,
    /// A plugin's `canopusconnect.send`: the device's plugin window.
    Plugin,
}

pub type InflightConnectFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

pub trait ConnectEnqueueBackend: Send + Sync {
    fn enqueue_connect_send<'a>(
        &'a self,
        req: Cpc2Request,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

struct DeviceSarBackend {
    addr: String,
}

impl ConnectEnqueueBackend for DeviceSarBackend {
    fn enqueue_connect_send<'a>(
        &'a self,
        req: Cpc2Request,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            use crate::device::xiaomi::XiaomiDevice;

            let addr_for_ver = self.addr.clone();
            let sar_version = crate::ecs::with_rt_mut(move |rt| {
                rt.component_ref::<XiaomiDevice>(&addr_for_ver)
                    .map(|dev| dev.sar_version)
                    .ok_or_else(|| "device_not_connected: XiaomiDevice not found".to_string())
            })
            .await?;
            let cipher =
                crate::device::xiaomi::packet::cipher::ensure_l2_cipher(&self.addr, sar_version)
                    .await
                    .ok_or("Canopus RPC requires an authenticated encrypted L2 session")?;
            let l2_pkt = req
                .to_l2_packet(Some(cipher.as_ref()))
                .map_err(|e| format!("L2 packet build failed: {e:?}"))?;
            let addr_for_enq = self.addr.clone();
            crate::ecs::with_rt_mut(move |rt| {
                let dev = rt
                    .component_mut::<XiaomiDevice>(&addr_for_enq)
                    .ok_or_else(|| "device_not_connected: XiaomiDevice not found".to_string())?;
                dev.sar.lock().enqueue(l2_pkt.to_bytes());
                Ok::<_, String>(())
            })
            .await
        })
    }
}

/// Bounded concurrent CanopusConnect sender for session-level protocols (such as Corona).
///
/// Separate from [`send`] so plugin calls keep their conservative serial contract while
/// built-in windowed sessions can keep up to `max_rpc` (capped at [`MAX_CONNECT_DATA_SLOTS`], i.e. 3)
/// DATA requests in flight simultaneously, leaving at least 1 device RPC slot for control/Manager commands.
#[derive(Clone)]
pub struct ConnectWindowSender {
    addr: String,
    module: String,
    session_sem: Arc<tokio::sync::Semaphore>,
    max_rpc: usize,
    backend: Option<Arc<dyn ConnectEnqueueBackend>>,
}

impl ConnectWindowSender {
    pub fn new(addr: impl Into<String>, module: impl Into<String>, max_rpc: usize) -> Result<Self, String> {
        let module = module.into();
        module_bytes(&module)?;
        let capped = max_rpc.clamp(1, MAX_CONNECT_DATA_SLOTS);
        Ok(Self {
            addr: addr.into(),
            module,
            session_sem: Arc::new(tokio::sync::Semaphore::new(capped)),
            max_rpc: capped,
            backend: None,
        })
    }

    #[cfg(test)]
    pub fn with_mock_backend(
        addr: impl Into<String>,
        module: impl Into<String>,
        max_rpc: usize,
        backend: Arc<dyn ConnectEnqueueBackend>,
    ) -> Result<Self, String> {
        let mut sender = Self::new(addr, module, max_rpc)?;
        sender.backend = Some(backend);
        Ok(sender)
    }

    pub fn max_rpc(&self) -> usize {
        self.max_rpc
    }

    /// Sends a single control payload (HELLO/OPEN/QUERY/FINISH/CANCEL/LIST) using the reserved
    /// control slot without consuming the DATA window semaphore, while still coordinating with
    /// the device's global 4-slot CPC2 RPC semaphore and ordered enqueue lock.
    pub async fn send_control(
        &self,
        payload: ConnectPayload,
        timeout: Duration,
    ) -> Result<(), String> {
        let fut = self.enqueue_internal(payload, timeout, Lane::Control).await?;
        fut.await
    }

    /// Acquires a DATA window permit (bounded by both session `max_rpc` and device
    /// `MAX_CONNECT_DATA_SLOTS` + global 4-slot RPC budget), registers the CPC2 waiter,
    /// and enqueues the packet in strict caller order before returning the response Future.
    ///
    /// Because waiter registration and SAR enqueue complete before this method returns,
    /// sequential calls to `submit_ordered` are guaranteed to enter the transport queue
    /// in exact offset order without racing spawned tasks.
    pub async fn submit_ordered(
        &self,
        payload: ConnectPayload,
        timeout: Duration,
    ) -> Result<InflightConnectFuture, String> {
        self.enqueue_internal(payload, timeout, Lane::Data).await
    }

    /// A sender for plugin messages to `module`: they draw on the device's plugin window, not on a
    /// session window of their own (see [`send`]).
    fn plugin(addr: &str, module: &str) -> Result<Self, String> {
        Self::new(addr, module, MAX_CONNECT_PLUGIN_SLOTS)
    }

    #[cfg(test)]
    fn plugin_with_mock_backend(
        addr: &str,
        module: &str,
        backend: Arc<dyn ConnectEnqueueBackend>,
    ) -> Result<Self, String> {
        let mut sender = Self::plugin(addr, module)?;
        sender.backend = Some(backend);
        Ok(sender)
    }

    /// Queues `payload` on `lane` in call order and returns the future of the device's answer.
    async fn submit(
        &self,
        lane: Lane,
        payload: ConnectPayload,
        timeout: Duration,
    ) -> Result<InflightConnectFuture, String> {
        self.enqueue_internal(payload, timeout, lane).await
    }

    /// Enqueues a batch of payloads in strict slice order, bounding concurrency to `max_rpc`
    /// and associating each response by `request_id`.
    pub async fn send_many_ordered(
        &self,
        payloads: Vec<ConnectPayload>,
        timeout: Duration,
    ) -> Result<(), String> {
        use tokio::task::JoinSet;
        let mut inflight = JoinSet::new();
        for payload in payloads {
            while inflight.len() >= self.max_rpc {
                match inflight.join_next().await {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(err))) => return Err(err),
                    Some(Err(join_err)) => return Err(format!("Connect task panicked: {join_err}")),
                    None => break,
                }
            }
            let fut = self.submit_ordered(payload, timeout).await?;
            inflight.spawn(fut);
        }
        while let Some(res) = inflight.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(join_err) => return Err(format!("Connect task panicked: {join_err}")),
            }
        }
        Ok(())
    }

    async fn enqueue_internal(
        &self,
        payload: ConnectPayload,
        timeout: Duration,
        lane: Lane,
    ) -> Result<InflightConnectFuture, String> {
        let body = encode_send(&self.module, &payload)?;
        let deadline = tokio::time::Instant::now() + timeout;

        let session_permit: Option<OwnedSemaphorePermit> = if lane == Lane::Data {
            Some(
                tokio::time::timeout_at(deadline, Arc::clone(&self.session_sem).acquire_owned())
                    .await
                    .map_err(|_| "Canopus request timed out".to_string())?
                    .map_err(|_| "device_not_connected: Connect session closed".to_string())?,
            )
        } else {
            None
        };

        let addr_for_handles = self.addr.clone();
        let ConcurrencyHandles {
            rpc_sem,
            data_sem,
            plugin_sem,
            enqueue_lock,
        } = crate::ecs::with_rt_mut(move |rt| {
            rt.component_ref::<CanopusSystem>(&addr_for_handles)
                .map(|sys| sys.concurrency_handles())
                .ok_or_else(|| "device_not_connected: Canopus component not found".to_string())
        })
        .await?;

        // The lane's own window comes before the device RPC slot, so a lane that is waiting for
        // its window never holds a slot another lane could use.
        let window = match lane {
            Lane::Control => None,
            Lane::Data => Some(data_sem),
            Lane::Plugin => Some(plugin_sem),
        };
        let data_permit: Option<OwnedSemaphorePermit> = match window {
            Some(window) => Some(
                tokio::time::timeout_at(deadline, window.acquire_owned())
                    .await
                    .map_err(|_| "Canopus request timed out".to_string())?
                    .map_err(|_| "device_not_connected: Canopus session closed".to_string())?,
            ),
            None => None,
        };

        let rpc_permit = tokio::time::timeout_at(deadline, rpc_sem.acquire_owned())
            .await
            .map_err(|_| "Canopus request timed out".to_string())?
            .map_err(|_| "device_not_connected: Canopus session closed".to_string())?;

        // Serialize request_id allocation + waiter registration + transport enqueue so
        // packets are queued in the exact order callers invoke submit_ordered / send.
        let _enqueue_guard = enqueue_lock.lock().await;
        let addr_for_waiter = self.addr.clone();
        let (request_id, rx, pending_guard) = crate::ecs::with_rt_mut(move |rt| {
            let sys = rt
                .component_mut::<CanopusSystem>(&addr_for_waiter)
                .ok_or_else(|| "device_not_connected: Canopus component not found".to_string())?;
            let (id, rx) = sys.allocate_waiter();
            Ok::<_, String>((id, rx, PendingRequest::new(id, Arc::clone(&sys.pending))))
        })
        .await?;

        let req = Cpc2Request::new(CanopusCommand::ConnectSend, request_id, body);
        let backend: Arc<dyn ConnectEnqueueBackend> = match &self.backend {
            Some(b) => Arc::clone(b),
            None => Arc::new(DeviceSarBackend {
                addr: self.addr.clone(),
            }),
        };
        backend.enqueue_connect_send(req).await?;
        drop(_enqueue_guard);

        let module = self.module.clone();
        Ok(Box::pin(async move {
            let _session_permit = session_permit;
            let _data_permit = data_permit;
            let _rpc_permit = rpc_permit;
            let _pending_guard = pending_guard;

            let response = tokio::time::timeout_at(deadline, rx)
                .await
                .map_err(|_| "Canopus request timed out".to_string())?
                .map_err(|_| "device_not_connected: Response channel closed".to_string())?;
            if response.opcode != CanopusCommand::ConnectSend as u32 {
                return Err("Canopus response opcode does not match the request".into());
            }
            interpret(&module, &response)
        }))
    }
}

async fn request_listen(addr: &str, module: &str, enable: bool) -> Result<(), String> {
    let response = request(
        addr,
        CanopusCommand::ConnectListen,
        encode_listen(module, enable)?,
        TIMEOUT,
    )
    .await?;
    interpret(module, &response)
}

/// Listeners the plugins asked for, per device and module. They outlive a device
/// entity (a reconnect builds a new one), which is why they are not a component:
/// [`resync`] replays them to the new session.
type Listeners = HashMap<String, HashMap<String, usize>>;

fn listeners() -> &'static Mutex<Listeners> {
    static LISTENERS: OnceCell<Mutex<Listeners>> = OnceCell::new();
    LISTENERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Serializes subscription changes together with their device request, so two
/// plugins listening to the same module cannot see each other half done.
fn subscription_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceCell<tokio::sync::Mutex<()>> = OnceCell::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn count(addr: &str, module: &str) -> usize {
    listeners()
        .lock()
        .unwrap()
        .get(addr)
        .and_then(|modules| modules.get(module))
        .copied()
        .unwrap_or(0)
}

fn adjust(addr: &str, module: &str, up: bool) {
    let mut table = listeners().lock().unwrap();
    let modules = table.entry(addr.to_string()).or_default();
    let entry = modules.entry(module.to_string()).or_insert(0);
    *entry = if up { *entry + 1 } else { entry.saturating_sub(1) };
    if *entry == 0 {
        modules.remove(module);
    }
    if modules.is_empty() {
        table.remove(addr);
    }
}

/// Starts delivering the module's messages to the core event bus. The device is
/// told on the first listener of a module; later ones only count.
pub async fn subscribe(addr: &str, module: &str) -> Result<(), String> {
    module_bytes(module)?;
    let _guard = subscription_lock().lock().await;
    if count(addr, module) == 0 {
        request_listen(addr, module, true).await?;
    }
    adjust(addr, module, true);
    // Listening precedes sending, so the windows are sized before the first message of a stream:
    // widening them while frames queue could let a later frame overtake an earlier one.
    learn_window(addr).await;
    Ok(())
}

/// Asks the device, once per session, how many requests its L2 transport holds, and widens this
/// device's windows to match (see [`CanopusSystem::widen_rpc_window`]). Each message waits a
/// Bluetooth round trip for its answer, so the window, not the link, sets the speed of a stream.
/// A Supervisor that does not report it holds four requests, and the windows stay as they are.
pub(crate) async fn learn_window(addr: &str) {
    let first = crate::ecs::access::with_device_component_mut::<CanopusSystem, _, _>(
        addr.to_string(),
        |sys| !std::mem::replace(&mut sys.window_learned, true),
    )
    .unwrap_or(false);
    if !first {
        return;
    }
    match super::get_device_info(addr).await {
        Ok(info) => {
            let Some(slots) = info.request_slots else {
                return;
            };
            let window = crate::ecs::access::with_device_component_mut::<CanopusSystem, _, _>(
                addr.to_string(),
                move |sys| sys.widen_rpc_window(slots as usize),
            );
            log::info!("[CanopusConnect] {addr} holds {slots} requests; window {window:?}");
        }
        Err(err) => {
            log::warn!("[CanopusConnect] request depth of {addr} unknown, keeping 4: {err}")
        }
    }
}

/// Drops one listener; the device stops pushing when the last one is gone. The
/// listener is gone even when the device cannot be told (it lapses with the
/// session anyway).
pub async fn unsubscribe(addr: &str, module: &str) -> Result<(), String> {
    module_bytes(module)?;
    let _guard = subscription_lock().lock().await;
    if count(addr, module) == 0 {
        return Ok(());
    }
    adjust(addr, module, false);
    if count(addr, module) == 0 {
        request_listen(addr, module, false).await?;
    }
    Ok(())
}

fn resync_needed(addr: &str) -> bool {
    crate::ecs::access::with_device_component_mut::<CanopusSystem, _, _>(
        addr.to_string(),
        |sys| sys.connect_resync_needed,
    )
    .unwrap_or(false)
}

/// Replays the local registrations while holding subscription_lock. Only a successful replay
/// clears the per-session dirty bit; a failure is retried by the next plugin send, not a timer.
async fn resync_locked(addr: &str) -> Result<(), String> {
    let modules: Vec<String> = listeners()
        .lock()
        .unwrap()
        .get(addr)
        .map(|modules| modules.keys().cloned().collect())
        .unwrap_or_default();
    for module in &modules {
        request_listen(addr, module, true)
            .await
            .map_err(|err| format!("Cannot restore CanopusConnect listener '{module}': {err}"))?;
    }
    if !modules.is_empty() {
        // A reconnect builds a new device entity with the default windows.
        learn_window(addr).await;
    }
    let _ = crate::ecs::access::with_device_component_mut::<CanopusSystem, _, _>(
        addr.to_string(),
        |sys| sys.connect_resync_needed = false,
    );
    Ok(())
}

/// Tells a freshly authenticated device what the plugins listen to. Authentication can finish
/// before the Supervisor starts: keep an unsuccessful replay pending for the next plugin send.
pub async fn resync(addr: &str) {
    let _guard = subscription_lock().lock().await;
    if let Err(err) = resync_locked(addr).await {
        log::warn!("[CanopusConnect] listener replay on {addr} pending: {err}");
    }
}

/// The device pushed a CPC2 event.
pub(super) fn on_event(owner_id: &str, event: Cpc2Event) {
    if event.opcode != CanopusCommand::ConnectMessage as u32 {
        log::debug!("[CanopusConnect] ignored CPC2 event opcode {}", event.opcode);
        return;
    }
    if owner_id.is_empty() {
        log::warn!("[CanopusConnect] message from a device without an owner dropped");
        return;
    }
    match decode_message(&event.payload) {
        Ok(message) => crate::events::emit(crate::events::CoreEvent::CanopusConnectMessage(
            crate::events::CanopusConnectMessage {
                device_addr: owner_id.to_string(),
                module: message.module,
                payload: message.payload,
            },
        )),
        Err(err) => log::warn!("[CanopusConnect] bad message from {owner_id}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(result: CanopusResult, payload: &[u8]) -> Cpc2Response {
        Cpc2Response {
            opcode: CanopusCommand::ConnectSend as u32,
            request_id: 1,
            result,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn send_and_listen_frames_have_the_documented_layout() {
        assert_eq!(
            encode_send("echo", &ConnectPayload::Text("hi".into())).unwrap(),
            [1, 4, b'e', b'c', b'h', b'o', b'h', b'i']
        );
        assert_eq!(
            encode_send("echo", &ConnectPayload::Binary(vec![0, 255])).unwrap(),
            [2, 4, b'e', b'c', b'h', b'o', 0, 255]
        );
        assert_eq!(encode_listen("echo", true).unwrap(), [1, 4, b'e', b'c', b'h', b'o']);
        assert_eq!(encode_listen("echo", false).unwrap(), [0, 4, b'e', b'c', b'h', b'o']);
    }

    #[test]
    fn frames_refuse_bad_ids_and_oversized_data() {
        assert!(encode_send("Echo", &ConnectPayload::Text(String::new())).is_err());
        assert!(encode_send("", &ConnectPayload::Text(String::new())).is_err());
        assert!(encode_send("a_very_long_module_id", &ConnectPayload::Text(String::new())).is_err());
        assert!(encode_send("echo", &ConnectPayload::Binary(vec![0; DATA_MAX])).is_ok());
        assert!(encode_send("echo", &ConnectPayload::Binary(vec![0; DATA_MAX + 1])).is_err());
        // The limit is bytes, not characters.
        assert!(encode_send("echo", &ConnectPayload::Text("é".repeat(DATA_MAX / 2 + 1))).is_err());
    }

    #[test]
    fn device_messages_decode_to_text_or_binary() {
        let text = decode_message(&[1, 4, b'e', b'c', b'h', b'o', 0xe4, 0xbd, 0xa0]).unwrap();
        assert_eq!(text.module, "echo");
        assert_eq!(text.payload, ConnectPayload::Text("你".into()));
        let binary = decode_message(&[2, 4, b'e', b'c', b'h', b'o', 0, 255]).unwrap();
        assert_eq!(binary.payload, ConnectPayload::Binary(vec![0, 255]));
        assert_eq!(
            decode_message(&[2, 4, b'e', b'c', b'h', b'o']).unwrap().payload,
            ConnectPayload::Binary(vec![])
        );
    }

    #[test]
    fn malformed_device_messages_are_refused() {
        assert!(decode_message(&[]).is_err());
        assert!(decode_message(&[1]).is_err());
        assert!(decode_message(&[1, 0]).is_err());
        assert!(decode_message(&[1, 17]).is_err());
        assert!(decode_message(&[1, 4, b'e', b'c']).is_err());
        assert!(decode_message(&[3, 4, b'e', b'c', b'h', b'o']).is_err());
        assert!(decode_message(&[1, 4, b'E', b'c', b'h', b'o']).is_err());
        assert!(decode_message(&[1, 4, b'e', b'c', b'h', b'o', 0xff]).is_err()); // not UTF-8 text
        let mut too_long = vec![2, 4, b'e', b'c', b'h', b'o'];
        too_long.extend(vec![0; DATA_MAX + 1]);
        assert!(decode_message(&too_long).is_err());
    }

    #[test]
    fn responses_explain_why_the_device_refused() {
        assert!(interpret("echo", &response(CanopusResult::Completed, &[0])).is_ok());
        let missing = interpret("echo", &response(CanopusResult::Disallowed, &[STATUS_MODULE]));
        assert!(missing.unwrap_err().contains("not installed"));
        let deaf = interpret("echo", &response(CanopusResult::Disallowed, &[STATUS_NOT_LISTENING]));
        assert!(deaf.unwrap_err().contains("not listening"));
        let full = interpret("echo", &response(CanopusResult::Disallowed, &[STATUS_FULL]));
        assert!(full.unwrap_err().contains("more listeners"));
        let old = interpret("echo", &response(CanopusResult::Rejected, &[]));
        assert!(old.unwrap_err().contains("does not support CanopusConnect"));
        let malformed = interpret("echo", &response(CanopusResult::Rejected, &[4]));
        assert!(malformed.unwrap_err().contains("refused"));
    }

    #[test]
    fn listener_counts_follow_subscriptions_per_device_and_module() {
        let (a, b) = ("aa:test", "bb:test");
        assert_eq!(count(a, "echo"), 0);
        adjust(a, "echo", true);
        adjust(a, "echo", true);
        adjust(a, "meter", true);
        adjust(b, "echo", true);
        assert_eq!((count(a, "echo"), count(a, "meter"), count(b, "echo")), (2, 1, 1));
        adjust(a, "echo", false);
        assert_eq!(count(a, "echo"), 1);
        adjust(a, "echo", false);
        adjust(a, "echo", false); // never below zero
        assert_eq!(count(a, "echo"), 0);
        adjust(a, "meter", false);
        adjust(b, "echo", false);
        assert!(listeners().lock().unwrap().get(a).is_none());
        assert!(listeners().lock().unwrap().get(b).is_none());
    }

    fn event_wire(opcode: u32, payload: &[u8]) -> Vec<u8> {
        let mut wire = vec![0u8; 36 + payload.len()];
        wire[0..4].copy_from_slice(&0x43504332u32.to_le_bytes());
        wire[4..6].copy_from_slice(&36u16.to_le_bytes());
        wire[6..8].copy_from_slice(&3u16.to_le_bytes());
        wire[8..10].copy_from_slice(&crate::device::canopus::CANOPUS_ABI_MAJOR.to_le_bytes());
        wire[10..12].copy_from_slice(&crate::device::canopus::CANOPUS_ABI_MINOR.to_le_bytes());
        wire[12..16].copy_from_slice(&((36 + payload.len()) as u32).to_le_bytes());
        wire[16..20].copy_from_slice(&opcode.to_le_bytes());
        wire[32..36].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        wire[36..].copy_from_slice(payload);
        wire
    }

    #[test]
    fn cpc2_events_decode_and_reject_responses() {
        let wire = event_wire(21, &[1, 4, b'e', b'c', b'h', b'o', b'x']);
        let event = Cpc2Event::decode(&wire).unwrap();
        assert_eq!(event.opcode, 21);
        assert_eq!(event.payload.len(), 7);
        let mut response = wire.clone();
        response[6] = 2;
        assert!(Cpc2Event::decode(&response).is_err());
        assert!(Cpc2Event::decode(&wire[..wire.len() - 1]).is_err());
        assert!(Cpc2Response::decode(&wire).is_err());
    }

    #[test]
    fn a_device_event_reaches_the_core_event_bus() {
        use crate::device::xiaomi::packet::v2::layer2::{L2Channel, L2OpCode};
        use crate::device::xiaomi::system::XiaomiSystemExt;
        let mut rx = crate::events::subscribe();
        let mut system = CanopusSystem::new("cc:event-test".into());
        let payload = [1, 4, b'e', b'c', b'h', b'o', b'h', b'i'];
        system.on_layer2_packet(L2Channel::Canopus, L2OpCode::WriteEnc, &event_wire(21, &payload));
        // Other opcodes, other channels and malformed frames publish nothing.
        system.on_layer2_packet(L2Channel::Canopus, L2OpCode::WriteEnc, &event_wire(99, &payload));
        system.on_layer2_packet(L2Channel::Pb, L2OpCode::WriteEnc, &event_wire(21, &payload));
        system.on_layer2_packet(L2Channel::Canopus, L2OpCode::WriteEnc, &event_wire(21, &[9, 1, b'x']));
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::events::CoreEvent::CanopusConnectMessage(message) = event {
                if message.device_addr == "cc:event-test" {
                    seen.push(message);
                }
            }
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].module, "echo");
        assert_eq!(seen[0].payload, ConnectPayload::Text("hi".into()));
    }

    struct RecordingBackend {
        enqueued: Mutex<Vec<Cpc2Request>>,
    }

    impl RecordingBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                enqueued: Mutex::new(Vec::new()),
            })
        }

        fn take(&self) -> Vec<Cpc2Request> {
            std::mem::take(&mut *self.enqueued.lock().unwrap())
        }
    }

    impl ConnectEnqueueBackend for RecordingBackend {
        fn enqueue_connect_send<'a>(
            &'a self,
            req: Cpc2Request,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            self.enqueued.lock().unwrap().push(req);
            Box::pin(async { Ok(()) })
        }
    }

    fn resp_wire(request_id: u32, result: CanopusResult, payload: &[u8]) -> Vec<u8> {
        let total = 36 + payload.len();
        let mut wire = vec![0u8; total];
        wire[0..4].copy_from_slice(&0x43504332u32.to_le_bytes());
        wire[4..6].copy_from_slice(&36u16.to_le_bytes());
        wire[6..8].copy_from_slice(&2u16.to_le_bytes());
        wire[8..10].copy_from_slice(&crate::device::canopus::CANOPUS_ABI_MAJOR.to_le_bytes());
        wire[10..12].copy_from_slice(&crate::device::canopus::CANOPUS_ABI_MINOR.to_le_bytes());
        wire[12..16].copy_from_slice(&(total as u32).to_le_bytes());
        wire[16..20].copy_from_slice(&(CanopusCommand::ConnectSend as u32).to_le_bytes());
        wire[20..24].copy_from_slice(&request_id.to_le_bytes());
        wire[28..32].copy_from_slice(&(result as u32).to_le_bytes());
        wire[32..36].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        wire[36..].copy_from_slice(payload);
        wire
    }

    fn ensure_test_ecs() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            crate::ecs::init_runtime_default();
        });
    }

    #[tokio::test]
    async fn window_sender_orders_enqueues_correlates_out_of_order_and_reserves_control_slot() {
        use crate::device::xiaomi::packet::v2::layer2::{L2Channel, L2OpCode};
        use crate::device::xiaomi::system::XiaomiSystemExt;

        ensure_test_ecs();
        let addr = "cc:win-order-test";
        crate::ecs::with_rt_mut(move |rt| {
            rt.spawn_device(addr.into(), (CanopusSystem::new(addr.into()),));
        })
        .await;

        let backend = RecordingBackend::new();
        let sender = ConnectWindowSender::with_mock_backend(addr, "corona", 3, backend.clone()).unwrap();
        assert_eq!(sender.max_rpc(), 3);

        // Submit 3 DATA payloads sequentially; they must enter the backend in exact offset order.
        let fut0 = sender
            .submit_ordered(ConnectPayload::Binary(vec![3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]), Duration::from_secs(2))
            .await
            .unwrap();
        let fut1 = sender
            .submit_ordered(ConnectPayload::Binary(vec![3, 0, 0, 0, 1, 0, 0, 0, 212, 1, 0, 0]), Duration::from_secs(2))
            .await
            .unwrap();
        let fut2 = sender
            .submit_ordered(ConnectPayload::Binary(vec![3, 0, 0, 0, 1, 0, 0, 0, 168, 3, 0, 0]), Duration::from_secs(2))
            .await
            .unwrap();

        // A 4th DATA request must block/time out because max_rpc = 3.
        let fourth = sender
            .submit_ordered(ConnectPayload::Binary(vec![3; 12]), Duration::from_millis(30))
            .await;
        assert!(fourth.is_err());

        // Meanwhile, the reserved 4th device RPC slot is still available for control commands!
        let ctrl_sender = sender.clone();
        let ctrl_task = tokio::spawn(async move {
            ctrl_sender
                .send_control(ConnectPayload::Binary(vec![4, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0]), Duration::from_secs(2))
                .await
        });
        for _ in 0..20 {
            if backend.enqueued.lock().unwrap().len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let enqueued = backend.take();
        assert_eq!(enqueued.len(), 4);
        // Verify exact offset order in the first 3 DATA frames: 0, 468, 936.
        let offsets: Vec<u32> = enqueued[..3]
            .iter()
            .map(|req| {
                // ConnectSend frame has `kind(1) | id_len(1) | "corona"(6)` = 8 byte prefix, then 12B Corona header (`arg` at 8..12)
                let corona_hdr = &req.payload[8..20];
                u32::from_le_bytes(corona_hdr[8..12].try_into().unwrap())
            })
            .collect();
        assert_eq!(offsets, vec![0, 468, 936]);

        // Deliver CPC2 responses out of order: req2 (refused), req0 (ok), ctrl (ok), req1 (ok).
        let id0 = enqueued[0].request_id;
        let id1 = enqueued[1].request_id;
        let id2 = enqueued[2].request_id;
        let id_ctrl = enqueued[3].request_id;

        crate::ecs::with_rt_mut(move |rt| {
            let mut sys = rt.component_mut::<CanopusSystem>(addr).unwrap();
            sys.on_layer2_packet(
                L2Channel::Canopus,
                L2OpCode::WriteEnc,
                &resp_wire(id2, CanopusResult::Disallowed, &[STATUS_NOT_LISTENING]),
            );
            sys.on_layer2_packet(
                L2Channel::Canopus,
                L2OpCode::WriteEnc,
                &resp_wire(id0, CanopusResult::Completed, &[0]),
            );
            sys.on_layer2_packet(
                L2Channel::Canopus,
                L2OpCode::WriteEnc,
                &resp_wire(id_ctrl, CanopusResult::Completed, &[0]),
            );
            sys.on_layer2_packet(
                L2Channel::Canopus,
                L2OpCode::WriteEnc,
                &resp_wire(id1, CanopusResult::Completed, &[0]),
            );
        })
        .await;

        assert!(fut0.await.is_ok());
        assert!(fut1.await.is_ok());
        assert!(fut2.await.unwrap_err().contains("not listening"));
        assert!(ctrl_task.await.unwrap().is_ok());

        crate::ecs::with_rt_mut(move |rt| {
            rt.remove_device(addr);
        })
        .await;
    }

    #[tokio::test]
    async fn plugin_sends_keep_a_window_in_flight_in_call_order_and_leave_a_slot_for_control() {
        use crate::device::xiaomi::packet::v2::layer2::{L2Channel, L2OpCode};
        use crate::device::xiaomi::system::XiaomiSystemExt;

        ensure_test_ecs();
        let addr = "cc:plugin-window-test";
        crate::ecs::with_rt_mut(move |rt| {
            rt.spawn_device(addr.into(), (CanopusSystem::new(addr.into()),));
        })
        .await;

        let backend = RecordingBackend::new();
        let plugin = || ConnectWindowSender::plugin_with_mock_backend(addr, "lyra_player", backend.clone()).unwrap();
        let data = |tag: u8| ConnectPayload::Binary(vec![0x11, tag, 0, 0, 0, 0]);

        // Three frames are queued one after the other and all three are in flight.
        let mut inflight = Vec::new();
        for tag in 0..MAX_CONNECT_PLUGIN_SLOTS as u8 {
            inflight.push(plugin().submit(Lane::Plugin, data(tag), Duration::from_secs(2)).await.unwrap());
        }
        // A fourth waits for the window instead of overflowing the device's queue.
        assert!(plugin().submit(Lane::Plugin, data(9), Duration::from_millis(30)).await.is_err());
        // The device's last RPC slot still takes a control request.
        let control = ConnectWindowSender::with_mock_backend(addr, "lyra_player", 1, backend.clone()).unwrap();
        let control_task = tokio::spawn(async move {
            control.send_control(data(0xC0), Duration::from_secs(2)).await
        });
        for _ in 0..50 {
            if backend.enqueued.lock().unwrap().len() == MAX_CONNECT_PLUGIN_SLOTS + 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let enqueued = backend.take();
        assert_eq!(enqueued.len(), MAX_CONNECT_PLUGIN_SLOTS + 1);
        // The frames reached the transport in the order they were sent: the payload follows the
        // `kind | id_len | "lyra_player"` prefix (2 + 11 bytes).
        let tags: Vec<u8> = enqueued[..3].iter().map(|req| req.payload[13 + 1]).collect();
        assert_eq!(tags, vec![0, 1, 2]);

        // Answers arrive in another order; each future gets its own request's answer.
        let ids: Vec<u32> = enqueued.iter().map(|req| req.request_id).collect();
        crate::ecs::with_rt_mut(move |rt| {
            let mut sys = rt.component_mut::<CanopusSystem>(addr).unwrap();
            for (position, id) in [(2usize, ids[2]), (3, ids[3]), (0, ids[0]), (1, ids[1])] {
                let (result, status) = if position == 1 {
                    (CanopusResult::Disallowed, STATUS_NOT_LISTENING)
                } else {
                    (CanopusResult::Completed, 0)
                };
                sys.on_layer2_packet(L2Channel::Canopus, L2OpCode::WriteEnc, &resp_wire(id, result, &[status]));
            }
        })
        .await;
        let mut inflight = inflight.into_iter();
        assert!(inflight.next().unwrap().await.is_ok());
        assert!(inflight.next().unwrap().await.unwrap_err().contains("not listening"));
        assert!(inflight.next().unwrap().await.is_ok());
        assert!(control_task.await.unwrap().is_ok());

        // Answered frames gave their slots back: the window accepts a new frame at once.
        let again = plugin().submit(Lane::Plugin, data(7), Duration::from_secs(2)).await;
        assert!(again.is_ok());

        crate::ecs::with_rt_mut(move |rt| {
            rt.remove_device(addr);
        })
        .await;
    }

    #[tokio::test]
    async fn timeout_and_disconnect_release_permits_and_avoid_waiter_wraparound_collision() {
        ensure_test_ecs();
        let addr = "cc:win-timeout-test";
        crate::ecs::with_rt_mut(move |rt| {
            let sys = CanopusSystem::new(addr.into());
            // Force next_request_id near u32::MAX to test wraparound and existing waiter skip.
            sys.next_request_id.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            rt.spawn_device(addr.into(), (sys,));
        })
        .await;

        let backend = RecordingBackend::new();
        let sender = ConnectWindowSender::with_mock_backend(addr, "corona", 2, backend.clone()).unwrap();

        // First request gets u32::MAX; hold it in-flight.
        let fut_keep = sender
            .submit_ordered(ConnectPayload::Binary(vec![1; 12]), Duration::from_secs(5))
            .await
            .unwrap();
        // Force wraparound back to u32::MAX while u32::MAX is still pending -> must skip u32::MAX and 0!
        crate::ecs::with_rt_mut(move |rt| {
            let sys = rt.component_ref::<CanopusSystem>(addr).unwrap();
            sys.next_request_id.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
        })
        .await;

        // Second request times out quickly and must clean up its waiter + permit.
        let fut_timeout = sender
            .submit_ordered(ConnectPayload::Binary(vec![2; 12]), Duration::from_millis(25))
            .await
            .unwrap();
        assert!(fut_timeout.await.unwrap_err().contains("timed out"));

        let reqs = backend.take();
        assert_eq!(reqs[0].request_id, u32::MAX);
        assert_eq!(reqs[1].request_id, 1); // Skipped u32::MAX (in-use) and 0 (reserved)!

        // Because fut_timeout released its permit on timeout, a new request can immediately acquire the 2nd slot.
        let fut_after_timeout = sender
            .submit_ordered(ConnectPayload::Binary(vec![3; 12]), Duration::from_secs(5))
            .await
            .unwrap();

        // Now simulate device disconnect: fail_waiters() must wake all in-flight futures immediately.
        crate::ecs::with_rt_mut(move |rt| {
            let sys = rt.component_ref::<CanopusSystem>(addr).unwrap();
            sys.fail_waiters();
            assert_eq!(sys.pending.lock().unwrap().len(), 0);
        })
        .await;

        assert!(fut_keep.await.is_err());
        assert!(fut_after_timeout.await.is_err());

        crate::ecs::with_rt_mut(move |rt| {
            rt.remove_device(addr);
        })
        .await;
    }
}
