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
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::OnceCell;

use super::{
    CanopusCommand, CanopusResult, CanopusSystem, Cpc2Event, Cpc2Response, is_valid_module_id,
    request,
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
pub async fn send(addr: &str, module: &str, payload: ConnectPayload) -> Result<(), String> {
    use crate::ecs::access::with_device_component_mut;
    let body = encode_send(module, &payload)?;
    let lock = with_device_component_mut::<CanopusSystem, _, _>(addr.to_string(), |sys| {
        std::sync::Arc::clone(&sys.connect_lock)
    })
    .map_err(|e| format!("Canopus component access failed: {e:?}"))?;
    let _guard = lock.lock().await;
    let response = request(addr, CanopusCommand::ConnectSend, body, TIMEOUT).await?;
    interpret(module, &response)
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
    Ok(())
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

/// Tells a freshly authenticated device what the plugins listen to; a device
/// forgets its listeners with the phone session. Best effort: a device without
/// CanopusConnect only logs.
pub async fn resync(addr: &str) {
    let modules: Vec<String> = listeners()
        .lock()
        .unwrap()
        .get(addr)
        .map(|modules| modules.keys().cloned().collect())
        .unwrap_or_default();
    if modules.is_empty() {
        return;
    }
    let _guard = subscription_lock().lock().await;
    for module in modules {
        if let Err(err) = request_listen(addr, &module, true).await {
            log::warn!("[CanopusConnect] could not re-listen to '{module}' on {addr}: {err}");
        }
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
        wire[8..10].copy_from_slice(&1u16.to_le_bytes());
        wire[10..12].copy_from_slice(&2u16.to_le_bytes());
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
}
