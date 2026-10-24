//! Canopus modules on a band (refactor-plan P5 / ADR-CAN-008).
//!
//! Canopus modules are managed directly via native .canopus packages and
//! WearPacket Layer 2 (0x43 / 'C') CPC2 RPC. Quick-app wrappers (.rpk) are
//! completely eradicated.

pub mod connect;
pub mod package;

/// ADR-CAN-003 module id syntax.
pub fn is_valid_module_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 16
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

// -----------------------------------------------------------------------------
// WearPacket Layer 2 Communication & CPC2 Protocol (ADR-CAN-008)
// -----------------------------------------------------------------------------

use crate::device::xiaomi::packet::v2::layer2::{L2Channel, L2Cipher, L2Error, L2OpCode, L2Packet};
use crate::device::xiaomi::system::XiaomiSystemExt;
use bevy_ecs::component::Component;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

pub const CANOPUS_L2_CHANNEL: u8 = 0x43;
pub const CANOPUS_TRANSPORT_V2_MAGIC: u32 = 0x43504332; // "CPC2"
pub const CANOPUS_TRANSPORT_V2_HEADER_SIZE: usize = 36;
/// The Canopus public ABI the Supervisor speaks. Every CPC2 header carries it and the Supervisor
/// drops (without answering) a request whose major differs from its own, so this follows the
/// framework's `CANOPUS_ABI_MAJOR`: 2.0 with the native UI ABI 2.0 and its module lifecycle.
pub const CANOPUS_ABI_MAJOR: u16 = 2;
pub const CANOPUS_ABI_MINOR: u16 = 0;
pub const CANOPUS_PROTO_MAX_PAYLOAD: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CanopusCommand {
    Echo = 1,
    Install = 2,
    Enable = 3,
    Disable = 4,
    Remove = 5,
    Rollback = 7,
    QueryModule = 8,
    QueryDevice = 9,
    EnterSafeMode = 10,
    Activate = 11,
    Scan = 12,
    NativeStage = 13,
    RestoreAfterBoot = 14,
    StageBegin = 15,
    StageWrite = 16,
    StageFinish = 17,
    StageAbort = 18,
    /// CanopusConnect (ADR-CAN-009), handled apart from the Manager commands.
    ConnectSend = 19,
    ConnectListen = 20,
    /// Device -> phone EVENT only; never sent as a request.
    ConnectMessage = 21,
    /// Leave safe mode at the next restart (payload: u32 `SafeModeExit`).
    ExitSafeMode = 22,
    QueryBoot = 23,
    SetBoot = 24,
    // 25 is the device-side CANOPUS_CMD_START_BOOT (loader-internal; the
    // phone never sends it).
    /// Native .canopus delivery: one resource stream per package resource,
    /// staged inside the same staging transaction after the artifact and
    /// extracted on the device into `/canopus/m.<module_id>/<name>` (flat
    /// basename). `StageResBegin` payload: `u8 name_len | name | u32 size |
    /// 32-byte SHA-256`; `StageResWrite` payload: `u32 offset | 1..508 data`
    /// (the same ordered chunk framing as the ELF stream); `StageResFinish`
    /// is empty and completes one resource. `StageAbort` cancels both streams.
    StageResBegin = 26,
    StageResWrite = 27,
    StageResFinish = 28,
}

impl TryFrom<u32> for CanopusCommand {
    type Error = Cpc2CodecError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        Ok(match v {
            1 => CanopusCommand::Echo,
            2 => CanopusCommand::Install,
            3 => CanopusCommand::Enable,
            4 => CanopusCommand::Disable,
            5 => CanopusCommand::Remove,
            7 => CanopusCommand::Rollback,
            8 => CanopusCommand::QueryModule,
            9 => CanopusCommand::QueryDevice,
            10 => CanopusCommand::EnterSafeMode,
            11 => CanopusCommand::Activate,
            12 => CanopusCommand::Scan,
            13 => CanopusCommand::NativeStage,
            14 => CanopusCommand::RestoreAfterBoot,
            15 => CanopusCommand::StageBegin,
            16 => CanopusCommand::StageWrite,
            17 => CanopusCommand::StageFinish,
            18 => CanopusCommand::StageAbort,
            19 => CanopusCommand::ConnectSend,
            20 => CanopusCommand::ConnectListen,
            21 => CanopusCommand::ConnectMessage,
            22 => CanopusCommand::ExitSafeMode,
            23 => CanopusCommand::QueryBoot,
            24 => CanopusCommand::SetBoot,
            26 => CanopusCommand::StageResBegin,
            27 => CanopusCommand::StageResWrite,
            28 => CanopusCommand::StageResFinish,
            _ => return Err(Cpc2CodecError::UnknownCommand(v)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CanopusResult {
    Rejected = 1,
    Accepted = 2,
    Queued = 3,
    Running = 4,
    Completed = 5,
    Failed = 6,
    Disallowed = 7,
    RebootRequired = 8,
}

impl TryFrom<u32> for CanopusResult {
    type Error = Cpc2CodecError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        Ok(match v {
            1 => CanopusResult::Rejected,
            2 => CanopusResult::Accepted,
            3 => CanopusResult::Queued,
            4 => CanopusResult::Running,
            5 => CanopusResult::Completed,
            6 => CanopusResult::Failed,
            7 => CanopusResult::Disallowed,
            8 => CanopusResult::RebootRequired,
            _ => return Err(Cpc2CodecError::InvalidResult(v)),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cpc2CodecError {
    TooShort(usize),
    InvalidMagic(u32),
    HeaderTooSmall(usize),
    UnsupportedAbiMajor(u16),
    UnsupportedAbiMinor(u16),
    PayloadTooLarge(usize),
    LengthMismatch(usize, usize),
    UnknownCommand(u32),
    InvalidResult(u32),
    InvalidKind(u16),
}

impl std::fmt::Display for Cpc2CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort(len) => write!(f, "Frame too short: {len} bytes"),
            Self::InvalidMagic(m) => write!(f, "Invalid magic: 0x{m:08x}"),
            Self::HeaderTooSmall(sz) => write!(f, "Header size too small: {sz}"),
            Self::UnsupportedAbiMajor(maj) => write!(f, "Unsupported ABI major: {maj}"),
            Self::UnsupportedAbiMinor(min) => write!(f, "Unsupported ABI minor: {min}"),
            Self::PayloadTooLarge(sz) => write!(f, "Payload too large: {sz} bytes"),
            Self::LengthMismatch(expected, actual) => {
                write!(f, "Length mismatch: expected {expected}, actual {actual}")
            }
            Self::UnknownCommand(cmd) => write!(f, "Unknown command opcode: {cmd}"),
            Self::InvalidResult(res) => write!(f, "Invalid result code: {res}"),
            Self::InvalidKind(k) => write!(f, "Invalid message kind: {k}"),
        }
    }
}

impl std::error::Error for Cpc2CodecError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpc2Request {
    pub command: CanopusCommand,
    pub request_id: u32,
    pub payload: Vec<u8>,
}

impl Cpc2Request {
    pub fn new(command: CanopusCommand, request_id: u32, payload: Vec<u8>) -> Self {
        Self {
            command,
            request_id,
            payload,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, Cpc2CodecError> {
        if self.payload.len() > CANOPUS_PROTO_MAX_PAYLOAD {
            return Err(Cpc2CodecError::PayloadTooLarge(self.payload.len()));
        }
        let total = CANOPUS_TRANSPORT_V2_HEADER_SIZE + self.payload.len();
        let mut buf = vec![0u8; total];
        buf[0..4].copy_from_slice(&CANOPUS_TRANSPORT_V2_MAGIC.to_le_bytes());
        buf[4..6].copy_from_slice(&(CANOPUS_TRANSPORT_V2_HEADER_SIZE as u16).to_le_bytes());
        buf[6..8].copy_from_slice(&1u16.to_le_bytes()); // REQUEST = 1
        buf[8..10].copy_from_slice(&CANOPUS_ABI_MAJOR.to_le_bytes());
        buf[10..12].copy_from_slice(&CANOPUS_ABI_MINOR.to_le_bytes());
        buf[12..16].copy_from_slice(&(total as u32).to_le_bytes());
        buf[16..20].copy_from_slice(&(self.command as u32).to_le_bytes());
        buf[20..24].copy_from_slice(&self.request_id.to_le_bytes());
        buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // flags
        buf[28..32].copy_from_slice(&0u32.to_le_bytes()); // result = 0
        buf[32..36].copy_from_slice(&(self.payload.len() as u32).to_le_bytes());
        if !self.payload.is_empty() {
            buf[CANOPUS_TRANSPORT_V2_HEADER_SIZE..].copy_from_slice(&self.payload);
        }
        Ok(buf)
    }

    pub fn to_l2_packet(&self, cipher: Option<&dyn L2Cipher>) -> Result<L2Packet, L2Error> {
        let wire = self.encode().map_err(|_| L2Error::TooShort)?;
        match cipher {
            Some(c) => L2Packet::canopus_write_enc(wire, c),
            None => Ok(L2Packet::canopus_write(wire)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpc2Response {
    pub opcode: u32,
    pub request_id: u32,
    pub result: CanopusResult,
    pub payload: Vec<u8>,
}

impl Cpc2Response {
    pub fn decode(buf: &[u8]) -> Result<Self, Cpc2CodecError> {
        if buf.len() < CANOPUS_TRANSPORT_V2_HEADER_SIZE {
            return Err(Cpc2CodecError::TooShort(buf.len()));
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != CANOPUS_TRANSPORT_V2_MAGIC {
            return Err(Cpc2CodecError::InvalidMagic(magic));
        }
        let header_size = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
        if header_size < CANOPUS_TRANSPORT_V2_HEADER_SIZE {
            return Err(Cpc2CodecError::HeaderTooSmall(header_size));
        }
        let kind = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        if kind != CPC2_KIND_RESPONSE {
            return Err(Cpc2CodecError::InvalidKind(kind));
        }
        let major = u16::from_le_bytes(buf[8..10].try_into().unwrap());
        if major != CANOPUS_ABI_MAJOR {
            return Err(Cpc2CodecError::UnsupportedAbiMajor(major));
        }
        let minor = u16::from_le_bytes(buf[10..12].try_into().unwrap());
        if minor > CANOPUS_ABI_MINOR {
            return Err(Cpc2CodecError::UnsupportedAbiMinor(minor));
        }
        let total_size = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        if total_size != buf.len() || total_size < header_size {
            return Err(Cpc2CodecError::LengthMismatch(total_size, buf.len()));
        }
        let opcode = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let request_id = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        let result_code = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        let result = CanopusResult::try_from(result_code)?;
        let payload_size = u32::from_le_bytes(buf[32..36].try_into().unwrap()) as usize;
        if payload_size > CANOPUS_PROTO_MAX_PAYLOAD || header_size + payload_size != total_size {
            return Err(Cpc2CodecError::LengthMismatch(
                header_size + payload_size,
                total_size,
            ));
        }
        let payload = buf[header_size..header_size + payload_size].to_vec();
        Ok(Self {
            opcode,
            request_id,
            result,
            payload,
        })
    }
}

/// An unsolicited device -> phone CPC2 record (message kind 3, request id 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpc2Event {
    pub opcode: u32,
    pub payload: Vec<u8>,
}

impl Cpc2Event {
    pub fn decode(buf: &[u8]) -> Result<Self, Cpc2CodecError> {
        if buf.len() < CANOPUS_TRANSPORT_V2_HEADER_SIZE {
            return Err(Cpc2CodecError::TooShort(buf.len()));
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != CANOPUS_TRANSPORT_V2_MAGIC {
            return Err(Cpc2CodecError::InvalidMagic(magic));
        }
        let header_size = u16::from_le_bytes(buf[4..6].try_into().unwrap()) as usize;
        if header_size < CANOPUS_TRANSPORT_V2_HEADER_SIZE {
            return Err(Cpc2CodecError::HeaderTooSmall(header_size));
        }
        let kind = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        if kind != CPC2_KIND_EVENT {
            return Err(Cpc2CodecError::InvalidKind(kind));
        }
        let major = u16::from_le_bytes(buf[8..10].try_into().unwrap());
        if major != CANOPUS_ABI_MAJOR {
            return Err(Cpc2CodecError::UnsupportedAbiMajor(major));
        }
        let minor = u16::from_le_bytes(buf[10..12].try_into().unwrap());
        if minor > CANOPUS_ABI_MINOR {
            return Err(Cpc2CodecError::UnsupportedAbiMinor(minor));
        }
        let total_size = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        let payload_size = u32::from_le_bytes(buf[32..36].try_into().unwrap()) as usize;
        if total_size != buf.len()
            || payload_size > CANOPUS_PROTO_MAX_PAYLOAD
            || header_size + payload_size != total_size
        {
            return Err(Cpc2CodecError::LengthMismatch(
                header_size + payload_size,
                buf.len(),
            ));
        }
        Ok(Self {
            opcode: u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            payload: buf[header_size..total_size].to_vec(),
        })
    }
}

const CPC2_KIND_RESPONSE: u16 = 2;
const CPC2_KIND_EVENT: u16 = 3;

/// Requests a device's L2 transport holds when it does not say otherwise: Supervisors before the
/// request depth in QUERY_DEVICE had a ring of 4. At most 3 slots may be used by concurrent
/// Connect DATA requests, leaving at least 1 slot for control/Manager RPCs.
pub const MAX_DEVICE_RPC_SLOTS: usize = 4;
pub const MAX_CONNECT_DATA_SLOTS: usize = 3;
/// Plugin `canopusconnect.send` calls in flight at once. Their own window, apart from the CRPack
/// data window, so a long resource-pack upload cannot starve a plugin (or the other way round);
/// both still draw on the device's RPC slots.
pub const MAX_CONNECT_PLUGIN_SLOTS: usize = 3;
/// The most request slots taken from a device's report (QUERY_DEVICE +84). A device with a
/// deeper ring gets wider windows -- every message waits a Bluetooth round trip for its answer,
/// so the window, not the link, sets the speed of a stream -- always one slot short of the
/// ring, for the Manager.
pub const MAX_NEGOTIATED_RPC_SLOTS: usize = 32;

/// ECS component that listens on `L2Channel::Canopus` and dispatches CPC2 responses
/// to pending futures and CPC2 events to the core event bus.
#[derive(Component)]
pub struct CanopusSystem {
    owner_id: String,
    next_request_id: AtomicU32,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Cpc2Response>>>>,
    install_lock: Arc<tokio::sync::Mutex<()>>,
    /// Plugin CanopusConnect messages in flight ([`MAX_CONNECT_PLUGIN_SLOTS`]).
    connect_plugin_sem: Arc<tokio::sync::Semaphore>,
    /// Global CPC2 RPC budget across the device (4 slots total).
    rpc_sem: Arc<tokio::sync::Semaphore>,
    /// Connect DATA in-flight budget (at most 3 slots so 1 slot remains for control/Manager).
    connect_data_sem: Arc<tokio::sync::Semaphore>,
    /// Ordered writer lock for windowed Connect sessions so frames enter SAR in offset order.
    connect_enqueue_lock: Arc<tokio::sync::Mutex<()>>,
    /// Requests the device holds, as the budgets above are sized: [`MAX_DEVICE_RPC_SLOTS`] until
    /// the device reports more (see [`Self::widen_rpc_window`]).
    rpc_slots: usize,
    /// The device has been asked for its request depth in this session.
    pub(crate) window_learned: bool,
}

impl Default for CanopusSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl CanopusSystem {
    pub fn new(owner_id: String) -> Self {
        crate::device::xiaomi::system::register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            owner_id,
            next_request_id: AtomicU32::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            install_lock: Arc::new(tokio::sync::Mutex::new(())),
            connect_plugin_sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECT_PLUGIN_SLOTS)),
            rpc_sem: Arc::new(tokio::sync::Semaphore::new(MAX_DEVICE_RPC_SLOTS)),
            connect_data_sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECT_DATA_SLOTS)),
            connect_enqueue_lock: Arc::new(tokio::sync::Mutex::new(())),
            rpc_slots: MAX_DEVICE_RPC_SLOTS,
            window_learned: false,
        }
    }

    /// Sizes the budgets for a device that holds `slots` requests (capped at
    /// [`MAX_NEGOTIATED_RPC_SLOTS`]): the device budget takes all of them, the Connect windows all
    /// but the one kept for the Manager. Only ever widens -- permits can be added to a budget that
    /// is in use, not taken back -- and returns the size in effect.
    pub fn widen_rpc_window(&mut self, slots: usize) -> usize {
        let slots = slots.min(MAX_NEGOTIATED_RPC_SLOTS);
        if slots > self.rpc_slots {
            let extra = slots - self.rpc_slots;
            self.rpc_sem.add_permits(extra);
            self.connect_data_sem.add_permits(extra);
            self.connect_plugin_sem.add_permits(extra);
            self.rpc_slots = slots;
        }
        self.rpc_slots
    }

    /// Allocates a unique non-zero request_id that is not currently in `pending`.
    pub fn next_id(&self) -> u32 {
        let map = self.pending.lock().unwrap();
        self.alloc_id_locked(&map)
    }

    fn alloc_id_locked(&self, map: &HashMap<u32, oneshot::Sender<Cpc2Response>>) -> u32 {
        for _ in 0..=u16::MAX as usize {
            let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 && !map.contains_key(&id) {
                return id;
            }
        }
        panic!("Canopus pending request_id space exhausted");
    }

    pub fn register_waiter(&self, request_id: u32) -> Result<oneshot::Receiver<Cpc2Response>, String> {
        if request_id == 0 {
            return Err("Canopus request_id must be non-zero".into());
        }
        let mut map = self.pending.lock().unwrap();
        if map.contains_key(&request_id) {
            return Err(format!("Canopus request_id {request_id} is already pending"));
        }
        let (tx, rx) = oneshot::channel();
        map.insert(request_id, tx);
        Ok(rx)
    }

    /// Atomically allocates a free non-zero `request_id` and registers its waiter.
    pub fn allocate_waiter(&self) -> (u32, oneshot::Receiver<Cpc2Response>) {
        let mut map = self.pending.lock().unwrap();
        let request_id = self.alloc_id_locked(&map);
        let (tx, rx) = oneshot::channel();
        map.insert(request_id, tx);
        (request_id, rx)
    }

    /// Fails all pending CPC2 waiters on disconnect and closes session semaphores.
    /// Fails all in-flight waiters without disabling the component. SAR aborts are
    /// recoverable in place (the L1 session restarts on the next enqueue), so the
    /// permit semaphores must stay open for subsequent requests on the same entity.
    pub fn fail_waiters(&self) {
        self.pending.lock().unwrap().clear();
    }

    pub(crate) fn concurrency_handles(&self) -> ConcurrencyHandles {
        ConcurrencyHandles {
            rpc_sem: Arc::clone(&self.rpc_sem),
            data_sem: Arc::clone(&self.connect_data_sem),
            plugin_sem: Arc::clone(&self.connect_plugin_sem),
            enqueue_lock: Arc::clone(&self.connect_enqueue_lock),
        }
    }
}

/// The shared budgets a Connect send draws on, cloned out of the component so they can be awaited
/// without holding the ECS.
pub(crate) struct ConcurrencyHandles {
    pub(crate) rpc_sem: Arc<tokio::sync::Semaphore>,
    pub(crate) data_sem: Arc<tokio::sync::Semaphore>,
    pub(crate) plugin_sem: Arc<tokio::sync::Semaphore>,
    pub(crate) enqueue_lock: Arc<tokio::sync::Mutex<()>>,
}

pub(crate) struct PendingRequest {
    request_id: u32,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Cpc2Response>>>>,
}

impl PendingRequest {
    pub(crate) fn new(
        request_id: u32,
        pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Cpc2Response>>>>,
    ) -> Self {
        Self {
            request_id,
            pending,
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.request_id);
    }
}

pub async fn send_cpc2_request(
    addr: &str,
    req: Cpc2Request,
    timeout: std::time::Duration,
) -> Result<Cpc2Response, String> {
    use crate::device::xiaomi::XiaomiDevice;
    use crate::ecs::access::with_device_component_mut;

    let request_id = req.request_id;
    let (rx, _pending, rpc_sem) =
        with_device_component_mut::<CanopusSystem, _, _>(addr.to_string(), move |sys| {
            let rx = sys.register_waiter(request_id)?;
            Ok::<_, String>((
                rx,
                PendingRequest::new(request_id, Arc::clone(&sys.pending)),
                Arc::clone(&sys.rpc_sem),
            ))
        })
        .map_err(|e| format!("Device component access failed: {:?}", e))??;

    let _rpc_permit = tokio::time::timeout(timeout, rpc_sem.acquire_owned())
        .await
        .map_err(|_| "Canopus request timed out".to_string())?
        .map_err(|_| "device_not_connected: Canopus session closed".to_string())?;

    let sar_version = with_device_component_mut::<XiaomiDevice, _, _>(addr.to_string(), |dev| dev.sar_version)
        .map_err(|e| format!("Device component access failed: {e:?}"))?;
    let cipher = crate::device::xiaomi::packet::cipher::ensure_l2_cipher(addr, sar_version)
        .await
        .ok_or("Canopus RPC requires an authenticated encrypted L2 session")?;
    let l2_pkt = req
        .to_l2_packet(Some(cipher.as_ref()))
        .map_err(|e| format!("L2 packet build failed: {:?}", e))?;

    with_device_component_mut::<XiaomiDevice, _, _>(addr.to_string(), move |dev| {
        dev.sar.lock().enqueue(l2_pkt.to_bytes());
    })
    .map_err(|e| format!("Failed to enqueue packet: {:?}", e))?;

    let response = tokio::time::timeout(timeout, rx)
        .await
        .map_err(|_| "Canopus request timed out".to_string())?
        .map_err(|_| "Response channel closed".to_string())?;
    if response.opcode != req.command as u32 {
        return Err("Canopus response opcode does not match the request".into());
    }
    Ok(response)
}

pub async fn request(
    addr: &str,
    command: CanopusCommand,
    payload: Vec<u8>,
    timeout: std::time::Duration,
) -> Result<Cpc2Response, String> {
    use crate::ecs::access::with_device_component_mut;
    let id =
        with_device_component_mut::<CanopusSystem, _, _>(addr.to_string(), |sys| sys.next_id())
            .map_err(|e| format!("Canopus component access failed: {e:?}"))?;
    send_cpc2_request(addr, Cpc2Request::new(command, id, payload), timeout).await
}

pub async fn query_device(addr: &str) -> Result<Cpc2Response, String> {
    request(
        addr,
        CanopusCommand::QueryDevice,
        vec![],
        std::time::Duration::from_secs(5),
    )
    .await
}

/// What to do with the installed modules when safe mode is left. It is applied as
/// next-boot intents, never as a load; the device then tries one normal start and
/// returns to safe mode if that start does not stay up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u32)]
pub enum SafeModeExit {
    /// Keep every module as it is.
    Keep = 0,
    /// Turn every module off.
    DisableAll = 1,
    /// Ask every module that has an older generation to go back to it.
    RollbackAll = 2,
}

pub async fn exit_safe_mode(addr: &str, action: SafeModeExit) -> Result<Cpc2Response, String> {
    request(
        addr,
        CanopusCommand::ExitSafeMode,
        (action as u32).to_le_bytes().to_vec(),
        std::time::Duration::from_secs(5),
    )
    .await
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BootPersistence {
    pub supported: bool,
    pub pinned: bool,
}

fn parse_boot_persistence(payload: &[u8]) -> Result<BootPersistence, String> {
    let flags = u32::from_le_bytes(
        payload.try_into().map_err(|_| "Invalid Canopus boot persistence response")?,
    );
    Ok(BootPersistence {
        supported: flags & 1 != 0,
        pinned: flags & 2 != 0,
    })
}

pub async fn query_boot_persistence(addr: &str) -> Result<BootPersistence, String> {
    let response = request(
        addr,
        CanopusCommand::QueryBoot,
        vec![],
        std::time::Duration::from_secs(5),
    )
    .await?;
    if response.result == CanopusResult::Rejected {
        return Err("This Supervisor does not support boot persistence; install a newer Canopus framework".into());
    }
    if response.result != CanopusResult::Completed {
        return Err(format!("Canopus boot persistence query returned {:?}", response.result));
    }
    parse_boot_persistence(&response.payload)
}

pub async fn set_boot_persistence(addr: &str, pinned: bool) -> Result<Cpc2Response, String> {
    let response = request(
        addr,
        CanopusCommand::SetBoot,
        u32::from(pinned).to_le_bytes().to_vec(),
        std::time::Duration::from_secs(30),
    )
    .await?;
    if response.result == CanopusResult::Rejected {
        return Err("This Supervisor does not support boot persistence; install a newer Canopus framework".into());
    }
    Ok(response)
}

pub async fn trigger_scan(addr: &str) -> Result<Cpc2Response, String> {
    request(
        addr,
        CanopusCommand::Scan,
        vec![],
        std::time::Duration::from_secs(10),
    )
    .await
}

fn module_id_payload(module_id: &str) -> Result<Vec<u8>, String> {
    if !is_valid_module_id(module_id) {
        return Err("Invalid Canopus module id".into());
    }
    let mut payload = module_id.as_bytes().to_vec();
    payload.push(0);
    Ok(payload)
}

pub async fn set_module_enabled(
    addr: &str,
    module_id: &str,
    enable: bool,
) -> Result<Cpc2Response, String> {
    let cmd = if enable {
        CanopusCommand::Enable
    } else {
        CanopusCommand::Disable
    };
    request(
        addr,
        cmd,
        module_id_payload(module_id)?,
        std::time::Duration::from_secs(5),
    )
    .await
}

pub async fn remove_module_cpc2(addr: &str, module_id: &str) -> Result<Cpc2Response, String> {
    request(
        addr,
        CanopusCommand::Remove,
        module_id_payload(module_id)?,
        std::time::Duration::from_secs(5),
    )
    .await
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CanopusDeviceInfo {
    pub framework_revision: u32,
    pub safe_mode: bool,
    pub module_count: u32,
    pub safe_mode_reason: u32,
    pub error_code: u32,
    pub flags: u32,
    pub scan_summary: u32,
    /// Absent on older supervisors; never guess a target from the device name.
    pub target_id: Option<String>,
    /// Requests the device's L2 transport holds at once. Absent on older supervisors (and on
    /// devices without the transport), which hold [`MAX_DEVICE_RPC_SLOTS`].
    #[serde(default)]
    pub request_slots: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CanopusDeviceModule {
    pub slot: u32,
    pub module_id: String,
    pub version: u32,
    pub state: u32,
    pub lifecycle_class: u32,
    pub flags: u32,
    pub activation_error: u32,
}

pub fn parse_device_info(payload: &[u8]) -> Result<CanopusDeviceInfo, String> {
    if payload.len() < 36 {
        return Err(format!(
            "Payload too short for device info: {} bytes",
            payload.len()
        ));
    }
    let magic = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if magic != 0x43514431 {
        return Err(format!("Invalid CQD1 magic: 0x{magic:08x}"));
    }
    let size = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
    if size < 36 || size != payload.len() || (size > 36 && size < 84) {
        return Err("Invalid Canopus device info size".into());
    }
    let target_id = if size >= 84 {
        Some(package::fixed_string(&payload[36..84])?.to_string())
    } else {
        None
    };
    let framework_revision = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    let safe_mode = u32::from_le_bytes(payload[12..16].try_into().unwrap()) != 0;
    let module_count = u32::from_le_bytes(payload[16..20].try_into().unwrap());
    let safe_mode_reason = u32::from_le_bytes(payload[20..24].try_into().unwrap());
    let error_code = u32::from_le_bytes(payload[24..28].try_into().unwrap());
    let flags = u32::from_le_bytes(payload[28..32].try_into().unwrap());
    let scan_summary = u32::from_le_bytes(payload[32..36].try_into().unwrap());
    let request_slots = payload
        .get(84..88)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .filter(|slots| *slots != 0);

    Ok(CanopusDeviceInfo {
        framework_revision,
        safe_mode,
        module_count,
        safe_mode_reason,
        error_code,
        flags,
        scan_summary,
        target_id,
        request_slots,
    })
}

pub fn parse_module_info(payload: &[u8]) -> Result<CanopusDeviceModule, String> {
    if payload.len() < 64 {
        return Err(format!(
            "Payload too short for module info: {} bytes",
            payload.len()
        ));
    }
    let magic = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if magic != 0x43514D31 {
        return Err(format!("Invalid CQM1 magic: 0x{magic:08x}"));
    }
    let size = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
    if size < 64 || size != payload.len() {
        return Err("Invalid Canopus module info size".into());
    }
    let slot = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    let state = u32::from_le_bytes(payload[12..16].try_into().unwrap());
    let lifecycle_class = u32::from_le_bytes(payload[16..20].try_into().unwrap());
    let version = u32::from_le_bytes(payload[20..24].try_into().unwrap());
    let flags = u32::from_le_bytes(payload[24..28].try_into().unwrap());
    let module_id = package::fixed_string(&payload[28..60])?.to_string();
    if !is_valid_module_id(&module_id) {
        return Err("Invalid Canopus module identity in query response".into());
    }
    let activation_error = u32::from_le_bytes(payload[60..64].try_into().unwrap());

    Ok(CanopusDeviceModule {
        slot,
        module_id,
        version,
        state,
        lifecycle_class,
        flags,
        activation_error,
    })
}

pub async fn get_device_info(addr: &str) -> Result<CanopusDeviceInfo, String> {
    let resp = query_device(addr).await?;
    if resp.result != CanopusResult::Completed {
        return Err(format!("Device query returned {:?}", resp.result));
    }
    parse_device_info(&resp.payload)
}

pub async fn query_module(addr: &str, slot: u32) -> Result<Option<CanopusDeviceModule>, String> {
    let resp = request(
        addr,
        CanopusCommand::QueryModule,
        slot.to_le_bytes().to_vec(),
        std::time::Duration::from_secs(5),
    )
    .await?;
    // The supervisor returns DISALLOWED for an unused slot, not an empty record.
    if resp.result == CanopusResult::Disallowed && resp.payload.is_empty() {
        return Ok(None);
    }
    require_completed(&resp)?;
    let module = parse_module_info(&resp.payload)?;
    if module.slot != slot || module.state == 0 {
        return Err("Canopus query returned a different or empty module slot".into());
    }
    Ok(Some(module))
}

pub async fn list_device_modules(addr: &str) -> Result<Vec<CanopusDeviceModule>, String> {
    let info = get_device_info(addr).await?;
    let mut modules = Vec::new();
    for slot in 0..16 {
        if modules.len() as u32 >= info.module_count {
            break;
        }
        if let Some(module) = query_module(addr, slot).await? {
            modules.push(module);
        }
    }
    if modules.len() as u32 != info.module_count {
        return Err("Canopus module list changed or is incomplete; refresh it".into());
    }
    Ok(modules)
}

fn require_completed(response: &Cpc2Response) -> Result<(), String> {
    if response.result != CanopusResult::Completed {
        return Err(format!(
            "Canopus command {} returned {:?}",
            response.opcode, response.result
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanopusInstallOutcome {
    pub module_id: String,
    pub target_id: String,
    pub reboot_required: bool,
}

/// Coarse install phase reported alongside progress. The device streams no
/// finer progress, so the UI labels the commit tail with this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanopusInstallPhase {
    Transfer,
    Commit,
}

/// Progress covers the byte stream (artifact + resources, weighted by bytes);
/// the device-side verify/commit tail keeps the last slice.
const TRANSFER_SHARE: f32 = 0.97;

/// `StageResBegin` payload: `u8 name_len | name | u32 size | 32-byte SHA-256`.
/// `name` is the flat extraction name; the device writes the verified stream
/// to `/canopus/m.<module_id>/<name>` in the module data directory.
fn resource_begin_payload(resource: &package::InstallResource) -> Result<Vec<u8>, String> {
    if resource.name.is_empty() || resource.name.len() > u8::MAX as usize {
        return Err(format!("Invalid Canopus resource name: {:?}", resource.name));
    }
    if resource.sha256.len() != 32 {
        return Err("Invalid Canopus resource SHA-256 length".into());
    }
    let mut payload = Vec::with_capacity(37 + resource.name.len());
    payload.push(resource.name.len() as u8);
    payload.extend_from_slice(resource.name.as_bytes());
    payload.extend_from_slice(&(resource.data.len() as u32).to_le_bytes());
    payload.extend_from_slice(&resource.sha256);
    Ok(payload)
}

/// One staging transaction at a time per ECS device. Each chunk is acknowledged
/// before the next; no Mass/quick-app transfer and no optimistic success.
pub async fn install_native_package(
    addr: &str,
    data: &[u8],
    on_progress: Arc<dyn Fn(f32, CanopusInstallPhase) + Send + Sync>,
) -> Result<CanopusInstallOutcome, String> {
    use crate::ecs::access::with_device_component_mut;
    let lock = with_device_component_mut::<CanopusSystem, _, _>(addr.to_string(), |sys| {
        Arc::clone(&sys.install_lock)
    })
    .map_err(|e| format!("Canopus component access failed: {e:?}"))?;
    let _guard = lock.lock().await;
    on_progress(0.0, CanopusInstallPhase::Transfer);
    let info = get_device_info(addr).await?;
    let target_id = info
        .target_id
        .ok_or("Canopus native install unsupported: supervisor has no exact target_id")?;
    let package = package::prepare_install(data, &target_id)?;
    let staged = stage_package_with(
        &package,
        |command, payload| request(addr, command, payload, std::time::Duration::from_secs(10)),
        on_progress.as_ref(),
    )
    .await;
    if let Err(error) = staged {
        // Best effort cleanup also covers a Begin/Write whose ACK was lost.
        if let Err(abort_error) = request(
            addr,
            CanopusCommand::StageAbort,
            vec![],
            std::time::Duration::from_secs(5),
        )
        .await
        .and_then(|r| require_completed(&r))
        {
            log::warn!("Canopus staging cleanup failed: {abort_error}");
        }
        return Err(error);
    }
    let response = request(
        addr,
        CanopusCommand::Install,
        module_id_payload(&package.module_id)?,
        std::time::Duration::from_secs(30),
    )
    .await?;
    if !matches!(
        response.result,
        CanopusResult::Completed | CanopusResult::RebootRequired
    ) {
        return Err(format!("Canopus install returned {:?}", response.result));
    }
    on_progress(1.0, CanopusInstallPhase::Commit);
    Ok(CanopusInstallOutcome {
        module_id: package.module_id,
        target_id,
        reboot_required: response.result == CanopusResult::RebootRequired,
    })
}

async fn stage_package_with<F, Fut>(
    package: &package::InstallPackage,
    mut send: F,
    on_progress: &(dyn Fn(f32, CanopusInstallPhase) + Send + Sync),
) -> Result<(), String>
where
    F: FnMut(CanopusCommand, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Cpc2Response, String>>,
{
    require_completed(&send(CanopusCommand::StageBegin, package.receipt.clone()).await?)?;
    let resource_total: usize = package.resources.iter().map(|r| r.data.len()).sum();
    let total = package.artifact.len() + resource_total;
    let mut transferred = 0usize;
    for (index, chunk) in package
        .artifact
        .chunks(CANOPUS_PROTO_MAX_PAYLOAD - 4)
        .enumerate()
    {
        let offset = index * (CANOPUS_PROTO_MAX_PAYLOAD - 4);
        let mut payload = (offset as u32).to_le_bytes().to_vec();
        payload.extend_from_slice(chunk);
        require_completed(&send(CanopusCommand::StageWrite, payload).await?)?;
        transferred += chunk.len();
        on_progress(
            TRANSFER_SHARE * transferred as f32 / total as f32,
            CanopusInstallPhase::Transfer,
        );
    }
    require_completed(&send(CanopusCommand::StageFinish, vec![]).await?)?;
    // One stream per resource inside the same staging transaction: ordered
    // chunks, per-resource completion, and flat extraction on the device. A
    // device that cannot deliver them rejects the stream instead of dropping
    // the files, so nothing is ever installed without its resources.
    for resource in &package.resources {
        let begin = resource_begin_payload(resource)?;
        require_completed(&send(CanopusCommand::StageResBegin, begin).await?)?;
        for (index, chunk) in resource
            .data
            .chunks(CANOPUS_PROTO_MAX_PAYLOAD - 4)
            .enumerate()
        {
            let offset = index * (CANOPUS_PROTO_MAX_PAYLOAD - 4);
            let mut payload = (offset as u32).to_le_bytes().to_vec();
            payload.extend_from_slice(chunk);
            require_completed(&send(CanopusCommand::StageResWrite, payload).await?)?;
            transferred += chunk.len();
            on_progress(
                TRANSFER_SHARE * transferred as f32 / total as f32,
                CanopusInstallPhase::Transfer,
            );
        }
        require_completed(&send(CanopusCommand::StageResFinish, vec![]).await?)?;
    }
    on_progress(TRANSFER_SHARE, CanopusInstallPhase::Commit);
    Ok(())
}

impl XiaomiSystemExt for CanopusSystem {
    fn on_layer2_packet(&mut self, channel: L2Channel, _opcode: L2OpCode, payload: &[u8]) {
        if channel != L2Channel::Canopus {
            return;
        }

        if payload.len() >= 8
            && u16::from_le_bytes([payload[6], payload[7]]) == CPC2_KIND_EVENT
        {
            match Cpc2Event::decode(payload) {
                Ok(event) => connect::on_event(&self.owner_id, event),
                Err(err) => log::warn!(
                    "[CanopusSystem] failed to decode CPC2 event ({} bytes): {}",
                    payload.len(),
                    err
                ),
            }
            return;
        }

        match Cpc2Response::decode(payload) {
            Ok(resp) => {
                log::debug!(
                    "[CanopusSystem] received CPC2 response: opcode={}, request_id={}, result={:?}",
                    resp.opcode,
                    resp.request_id,
                    resp.result
                );
                if let Some(tx) = self.pending.lock().unwrap().remove(&resp.request_id) {
                    let _ = tx.send(resp);
                }
            }
            Err(err) => {
                log::warn!(
                    "[CanopusSystem] failed to decode CPC2 response ({} bytes): {}",
                    payload.len(),
                    err
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_safe_mode_is_command_22_with_a_u32_action() {
        assert_eq!(
            CanopusCommand::try_from(22).unwrap(),
            CanopusCommand::ExitSafeMode
        );
        let request = Cpc2Request::new(
            CanopusCommand::ExitSafeMode,
            7,
            (SafeModeExit::DisableAll as u32).to_le_bytes().to_vec(),
        )
        .encode()
        .unwrap();
        assert_eq!(&request[16..20], &22u32.to_le_bytes());
        assert_eq!(&request[CANOPUS_TRANSPORT_V2_HEADER_SIZE..], &1u32.to_le_bytes());
    }

    #[test]
    fn ids_follow_adr_syntax() {
        assert!(is_valid_module_id("bluetooth_audio"));
        assert!(!is_valid_module_id("Bluetooth"));
        assert!(!is_valid_module_id("org.example.hello"));
        assert!(!is_valid_module_id("a_very_long_module_id"));
    }

    #[test]
    fn test_cpc2_request_encode_response_decode() {
        let req = Cpc2Request::new(CanopusCommand::QueryDevice, 0x4301, vec![1, 2, 3, 4]);
        let enc = req.encode().unwrap();
        assert_eq!(enc.len(), CANOPUS_TRANSPORT_V2_HEADER_SIZE + 4);
        assert_eq!(&enc[0..4], &CANOPUS_TRANSPORT_V2_MAGIC.to_le_bytes());
        assert_eq!(&enc[8..12], &[2, 0, 0, 0]); // ABI 2.0, little endian.

        // Construct response wire frame
        let mut resp_wire = vec![0u8; CANOPUS_TRANSPORT_V2_HEADER_SIZE + 2];
        resp_wire[0..4].copy_from_slice(&CANOPUS_TRANSPORT_V2_MAGIC.to_le_bytes());
        resp_wire[4..6].copy_from_slice(&(CANOPUS_TRANSPORT_V2_HEADER_SIZE as u16).to_le_bytes());
        resp_wire[6..8].copy_from_slice(&2u16.to_le_bytes()); // RESPONSE = 2
        resp_wire[8..10].copy_from_slice(&CANOPUS_ABI_MAJOR.to_le_bytes());
        resp_wire[10..12].copy_from_slice(&CANOPUS_ABI_MINOR.to_le_bytes());
        resp_wire[12..16]
            .copy_from_slice(&((CANOPUS_TRANSPORT_V2_HEADER_SIZE + 2) as u32).to_le_bytes());
        resp_wire[16..20].copy_from_slice(&(CanopusCommand::QueryDevice as u32).to_le_bytes());
        resp_wire[20..24].copy_from_slice(&0x4301u32.to_le_bytes());
        resp_wire[28..32].copy_from_slice(&5u32.to_le_bytes()); // C ABI COMPLETED, not a self-roundtrip
        resp_wire[32..36].copy_from_slice(&2u32.to_le_bytes());
        resp_wire[36..38].copy_from_slice(&[0xaa, 0xbb]);

        let decoded = Cpc2Response::decode(&resp_wire).unwrap();
        assert_eq!(decoded.opcode, CanopusCommand::QueryDevice as u32);
        assert_eq!(decoded.request_id, 0x4301);
        assert_eq!(decoded.result, CanopusResult::Completed);
        assert_eq!(decoded.payload, vec![0xaa, 0xbb]);

        resp_wire[8..10].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            Cpc2Response::decode(&resp_wire),
            Err(Cpc2CodecError::UnsupportedAbiMajor(1))
        );
        resp_wire[8..10].copy_from_slice(&2u16.to_le_bytes());
        resp_wire[10..12].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            Cpc2Response::decode(&resp_wire),
            Err(Cpc2CodecError::UnsupportedAbiMinor(1))
        );
    }

    #[test]
    fn widening_the_window_grows_every_budget_and_never_narrows_it() {
        let mut sys = CanopusSystem::new("cc:widen-test".to_string());
        assert_eq!(sys.rpc_sem.available_permits(), MAX_DEVICE_RPC_SLOTS);
        assert_eq!(sys.widen_rpc_window(16), 16);
        assert_eq!(sys.rpc_sem.available_permits(), 16);
        // One slot stays the Manager's.
        assert_eq!(sys.connect_plugin_sem.available_permits(), 15);
        assert_eq!(sys.connect_data_sem.available_permits(), 15);
        // A smaller report (an older Supervisor after a reconnect) cannot take permits back.
        assert_eq!(sys.widen_rpc_window(4), 16);
        assert_eq!(sys.rpc_sem.available_permits(), 16);
        assert_eq!(sys.widen_rpc_window(1_000), MAX_NEGOTIATED_RPC_SLOTS);
        assert_eq!(
            sys.connect_plugin_sem.available_permits(),
            MAX_NEGOTIATED_RPC_SLOTS - 1
        );
    }

    #[test]
    fn test_parse_device_and_module_info() {
        // Construct mock CQD1 payload (36 bytes)
        let mut dev_payload = vec![0u8; 36];
        dev_payload[0..4].copy_from_slice(&0x43514431u32.to_le_bytes()); // "CQD1"
        dev_payload[4..8].copy_from_slice(&36u32.to_le_bytes());
        dev_payload[8..12].copy_from_slice(&2u32.to_le_bytes()); // revision
        dev_payload[12..16].copy_from_slice(&0u32.to_le_bytes()); // safe_mode = false
        dev_payload[16..20].copy_from_slice(&3u32.to_le_bytes()); // module_count = 3
        dev_payload[20..24].copy_from_slice(&0u32.to_le_bytes()); // safe_mode_reason
        dev_payload[24..28].copy_from_slice(&0u32.to_le_bytes()); // error_code
        dev_payload[28..32].copy_from_slice(&0u32.to_le_bytes()); // flags
        dev_payload[32..36].copy_from_slice(&0x10u32.to_le_bytes()); // scan_summary

        let dev_info = parse_device_info(&dev_payload).unwrap();
        assert_eq!(dev_info.framework_revision, 2);
        assert!(!dev_info.safe_mode);
        assert_eq!(dev_info.module_count, 3);
        assert_eq!(dev_info.scan_summary, 0x10);
        assert_eq!(dev_info.target_id, None);
        dev_payload.resize(84, 0);
        dev_payload[4..8].copy_from_slice(&84u32.to_le_bytes());
        let target = b"xiaomi-band-11-1.1.139";
        dev_payload[36..36 + target.len()].copy_from_slice(target);
        assert_eq!(
            parse_device_info(&dev_payload)
                .unwrap()
                .target_id
                .as_deref(),
            Some("xiaomi-band-11-1.1.139")
        );
        assert_eq!(parse_device_info(&dev_payload).unwrap().request_slots, None);
        // A Supervisor with the deep L2 ring says how many requests it holds.
        dev_payload.resize(88, 0);
        dev_payload[4..8].copy_from_slice(&88u32.to_le_bytes());
        dev_payload[84..88].copy_from_slice(&16u32.to_le_bytes());
        let deep = parse_device_info(&dev_payload).unwrap();
        assert_eq!(deep.request_slots, Some(16));
        assert_eq!(deep.target_id.as_deref(), Some("xiaomi-band-11-1.1.139"));
        // Zero is "unknown", the same as absent.
        dev_payload[84..88].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(parse_device_info(&dev_payload).unwrap().request_slots, None);

        // Construct mock CQM1 payload (64 bytes)
        let mut mod_payload = vec![0u8; 64];
        mod_payload[0..4].copy_from_slice(&0x43514D31u32.to_le_bytes()); // "CQM1"
        mod_payload[4..8].copy_from_slice(&64u32.to_le_bytes());
        mod_payload[8..12].copy_from_slice(&1u32.to_le_bytes()); // slot = 1
        mod_payload[12..16].copy_from_slice(&3u32.to_le_bytes()); // state = 3 (ENABLED)
        mod_payload[16..20].copy_from_slice(&1u32.to_le_bytes()); // lifecycle = 1
        mod_payload[20..24].copy_from_slice(&100u32.to_le_bytes()); // version = 100
        mod_payload[24..28].copy_from_slice(&0u32.to_le_bytes()); // flags
        mod_payload[28..40].copy_from_slice(b"lyra_player\0");
        mod_payload[60..64].copy_from_slice(&0u32.to_le_bytes()); // activation_error

        let mod_info = parse_module_info(&mod_payload).unwrap();
        assert_eq!(mod_info.slot, 1);
        assert_eq!(mod_info.module_id, "lyra_player");
        assert_eq!(mod_info.version, 100);
        assert_eq!(mod_info.state, 3);
    }

    #[tokio::test]
    async fn native_stage_chunks_are_sequential_and_require_completed() {
        let package = package::InstallPackage {
            module_id: "hello".into(),
            receipt: vec![0; 176],
            artifact: vec![42; 1020],
            resources: vec![],
        };
        let calls = Mutex::new(Vec::new());
        stage_package_with(
            &package,
            |command, payload| {
                calls.lock().unwrap().push((command, payload));
                std::future::ready(Ok(Cpc2Response {
                    opcode: command as u32,
                    request_id: 1,
                    result: CanopusResult::Completed,
                    payload: vec![],
                }))
            },
            &|_, _| {},
        )
        .await
        .unwrap();
        let calls = calls.into_inner().unwrap();
        assert_eq!(
            calls.iter().map(|(cmd, _)| *cmd as u32).collect::<Vec<_>>(),
            vec![15, 16, 16, 16, 17]
        );
        assert_eq!(calls[0].1.len(), 176);
        for (index, (_, payload)) in calls[1..4].iter().enumerate() {
            assert_eq!(
                u32::from_le_bytes(payload[..4].try_into().unwrap()),
                (index * 508) as u32
            );
            assert!(payload.len() <= 512);
        }
        let mut attempts = 0;
        let result = stage_package_with(
            &package,
            |command, _| {
                attempts += 1;
                std::future::ready(Ok(Cpc2Response {
                    opcode: command as u32,
                    request_id: 1,
                    result: CanopusResult::Queued,
                    payload: vec![],
                }))
            },
            &|_, _| {},
        )
        .await;
        assert!(result.is_err());
        assert_eq!(attempts, 1);
        assert_eq!(CanopusResult::try_from(1).unwrap(), CanopusResult::Rejected);
        assert_eq!(CanopusResult::try_from(4).unwrap(), CanopusResult::Running);
        assert_eq!(module_id_payload("hello").unwrap(), b"hello\0");
    }

    #[tokio::test]
    async fn resource_staging_is_one_stream_per_resource_after_the_artifact() {
        let sha = vec![7u8; 32];
        let package = package::InstallPackage {
            module_id: "hello".into(),
            receipt: vec![0; 176],
            artifact: vec![42; 1020],
            resources: vec![
                package::InstallResource {
                    name: "logo.bin".into(),
                    sha256: sha.clone(),
                    data: vec![9; 600],
                },
                package::InstallResource {
                    name: "small.bin".into(),
                    sha256: sha.clone(),
                    data: vec![1; 3],
                },
            ],
        };
        let calls = Mutex::new(Vec::new());
        stage_package_with(
            &package,
            |command, payload| {
                calls.lock().unwrap().push((command, payload));
                std::future::ready(Ok(Cpc2Response {
                    opcode: command as u32,
                    request_id: 1,
                    result: CanopusResult::Completed,
                    payload: vec![],
                }))
            },
            &|_, _| {},
        )
        .await
        .unwrap();
        let calls = calls.into_inner().unwrap();
        assert_eq!(
            calls.iter().map(|(cmd, _)| *cmd as u32).collect::<Vec<_>>(),
            vec![15, 16, 16, 16, 17, 26, 27, 27, 28, 26, 27, 28]
        );
        assert_eq!(CanopusCommand::try_from(26).unwrap(), CanopusCommand::StageResBegin);
        assert_eq!(CanopusCommand::try_from(27).unwrap(), CanopusCommand::StageResWrite);
        assert_eq!(CanopusCommand::try_from(28).unwrap(), CanopusCommand::StageResFinish);
        assert_eq!(CanopusCommand::try_from(25), Err(Cpc2CodecError::UnknownCommand(25)));
        // StageResBegin: u8 name_len | name | u32 size | 32-byte SHA-256.
        let begin = &calls[5].1;
        assert_eq!(begin[0] as usize, "logo.bin".len());
        assert_eq!(&begin[1..9], b"logo.bin");
        assert_eq!(u32::from_le_bytes(begin[9..13].try_into().unwrap()), 600);
        assert_eq!(begin[13..45], sha[..]);
        assert_eq!(begin.len(), 45);
        assert_eq!(calls[9].1.len(), 1 + "small.bin".len() + 4 + 32);
        assert_eq!(&calls[9].1[1..10], b"small.bin");
        assert_eq!(u32::from_le_bytes(calls[9].1[10..14].try_into().unwrap()), 3);
        // StageResWrite reuses the ELF stream's u32 offset chunk framing.
        for (index, (_, payload)) in calls[6..8].iter().enumerate() {
            assert_eq!(
                u32::from_le_bytes(payload[..4].try_into().unwrap()),
                (index * 508) as u32
            );
            assert!(payload.len() <= 512);
        }
        assert!(calls[8].1.is_empty()); // StageResFinish
        assert!(calls[11].1.is_empty());
    }
}
