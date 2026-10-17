//! Canopus modules on a band (refactor-plan P5 / ADR-CAN-008).
//!
//! Canopus modules are managed directly via native .canopus packages and
//! WearPacket Layer 2 (0x43 / 'C') CPC2 RPC. Quick-app wrappers (.rpk) are
//! completely eradicated.

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
pub const CANOPUS_ABI_MAJOR: u16 = 1;
pub const CANOPUS_ABI_MINOR: u16 = 2;
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
        if kind != 2 {
            // RESPONSE = 2
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

/// ECS component that listens on `L2Channel::Canopus` and dispatches CPC2 responses
/// to pending futures.
#[derive(Component, Default)]
pub struct CanopusSystem {
    next_request_id: AtomicU32,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Cpc2Response>>>>,
    install_lock: Arc<tokio::sync::Mutex<()>>,
}

impl CanopusSystem {
    pub fn new() -> Self {
        crate::device::xiaomi::system::register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            next_request_id: AtomicU32::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            install_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn next_id(&self) -> u32 {
        let mut id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        }
        id
    }

    pub fn register_waiter(&self, request_id: u32) -> oneshot::Receiver<Cpc2Response> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id, tx);
        rx
    }
}

struct PendingRequest {
    request_id: u32,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Cpc2Response>>>>,
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
    let (rx, _pending) =
        with_device_component_mut::<CanopusSystem, _, _>(addr.to_string(), move |sys| {
            (
                sys.register_waiter(request_id),
                PendingRequest {
                    request_id,
                    pending: Arc::clone(&sys.pending),
                },
            )
        })
        .map_err(|e| format!("Device component access failed: {:?}", e))?;

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

    Ok(CanopusDeviceInfo {
        framework_revision,
        safe_mode,
        module_count,
        safe_mode_reason,
        error_code,
        flags,
        scan_summary,
        target_id,
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

/// One staging transaction at a time per ECS device. Each chunk is acknowledged
/// before the next; no Mass/quick-app transfer and no optimistic success.
pub async fn install_native_package(
    addr: &str,
    data: &[u8],
    on_progress: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<CanopusInstallOutcome, String> {
    use crate::ecs::access::with_device_component_mut;
    let lock = with_device_component_mut::<CanopusSystem, _, _>(addr.to_string(), |sys| {
        Arc::clone(&sys.install_lock)
    })
    .map_err(|e| format!("Canopus component access failed: {e:?}"))?;
    let _guard = lock.lock().await;
    on_progress(0.0);
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
    on_progress(1.0);
    Ok(CanopusInstallOutcome {
        module_id: package.module_id,
        target_id,
        reboot_required: response.result == CanopusResult::RebootRequired,
    })
}

async fn stage_package_with<F, Fut>(
    package: &package::InstallPackage,
    mut send: F,
    on_progress: &(dyn Fn(f32) + Send + Sync),
) -> Result<(), String>
where
    F: FnMut(CanopusCommand, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Cpc2Response, String>>,
{
    require_completed(&send(CanopusCommand::StageBegin, package.receipt.clone()).await?)?;
    for (index, chunk) in package
        .artifact
        .chunks(CANOPUS_PROTO_MAX_PAYLOAD - 4)
        .enumerate()
    {
        let offset = index * (CANOPUS_PROTO_MAX_PAYLOAD - 4);
        let mut payload = (offset as u32).to_le_bytes().to_vec();
        payload.extend_from_slice(chunk);
        require_completed(&send(CanopusCommand::StageWrite, payload).await?)?;
        on_progress(0.95 * (offset + chunk.len()) as f32 / package.artifact.len() as f32);
    }
    require_completed(&send(CanopusCommand::StageFinish, vec![]).await?)?;
    on_progress(0.97);
    Ok(())
}

impl XiaomiSystemExt for CanopusSystem {
    fn on_layer2_packet(&mut self, channel: L2Channel, _opcode: L2OpCode, payload: &[u8]) {
        if channel != L2Channel::Canopus {
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
            &|_| {},
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
            &|_| {},
        )
        .await;
        assert!(result.is_err());
        assert_eq!(attempts, 1);
        assert_eq!(CanopusResult::try_from(1).unwrap(), CanopusResult::Rejected);
        assert_eq!(CanopusResult::try_from(4).unwrap(), CanopusResult::Running);
        assert_eq!(module_id_payload("hello").unwrap(), b"hello\0");
    }
}
