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
}

impl ErpcSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
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
        )
    }

    fn handle_setup_conn(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        ERpcSetupConnRequest::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo ERPC setup-conn: {err}"))?;
        log::info!("[VivoDevice.ERPC] setup-conn");
        update_erpc_component(&self.owner_id, |comp| {
            comp.connected = true;
        })?;
        self.send_vivo_message(
            VscpMessage::new(BID_ERPC, CID_SETUP_CONN_RESPONSE, Vec::new()),
            "VivoErpcSystem::ack_setup_conn",
        )
    }

    fn handle_business(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let data = decode_business_payload(&message.payload);
        let len = data.len();
        log::debug!("[VivoDevice.ERPC] received business bytes={len}");
        update_erpc_component(&self.owner_id, move |comp| {
            comp.received_bytes = comp.received_bytes.saturating_add(len as u64);
            comp.last_business_payload = Some(data);
        })?;
        self.send_vivo_message(
            VscpMessage::new(BID_ERPC, CID_BUSINESS_RESPONSE, Vec::new()),
            "VivoErpcSystem::ack_business",
        )
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
