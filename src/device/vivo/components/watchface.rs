use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::{generated::typed::DialManageBleReq, response_cid},
    msgpack::MsgpackReader,
};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        components::resource::ResourceComponent,
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::{Component, access::with_device_component_mut},
};

use super::shared::{HasVivoRequestContext, RequestSlot, VivoRequestExt};

const BID_DIAL: u8 = 1;
const CID_MANAGE_DIAL: u8 = 4;
const CID_SET_CURRENT_DIAL: u8 = 5;

#[derive(Component)]
pub struct WatchfaceSystem {
    owner_id: String,
    tk_handle: Handle,
    set_current_wait: RequestSlot<()>,
    uninstall_wait: RequestSlot<()>,
}

impl WatchfaceSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            set_current_wait: RequestSlot::new(),
            uninstall_wait: RequestSlot::new(),
        }
    }

    pub fn set_watchface(
        &mut self,
        watchface_id: &str,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        let dial_id = parse_dial_id(watchface_id)?;
        let order = current_dial_order(&self.owner_id)?;
        let payload = build_set_current_dial_payload(order, dial_id)?;
        let (rx, should_enqueue) = self.set_current_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.Watchface] setting current dial id={} order={}",
                dial_id,
                order
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_DIAL, CID_SET_CURRENT_DIAL, payload),
                "VivoWatchfaceSystem::set_watchface",
            ) {
                self.set_current_wait.fail(err);
            }
        }
        Ok(rx)
    }

    pub fn uninstall_watchface(
        &mut self,
        watchface_id: &str,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        let delete_dial_id = parse_dial_id(watchface_id)?;
        let (current_dial_id, remaining_dial_ids) =
            build_uninstall_context(&self.owner_id, delete_dial_id)?;
        let payload = DialManageBleReq {
            dial_id_list: remaining_dial_ids.clone(),
            count: i32::try_from(remaining_dial_ids.len())
                .map_err(|_| anyhow_site!("vivo dial list length does not fit i32"))?,
            current_dial_id,
            type_: 1,
            delete_dial_id,
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo dial manage request: {err}"))?;

        let (rx, should_enqueue) = self.uninstall_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.Watchface] uninstalling dial id={} current={} remaining={}",
                delete_dial_id,
                current_dial_id,
                remaining_dial_ids.len()
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_DIAL, CID_MANAGE_DIAL, payload),
                "VivoWatchfaceSystem::uninstall_watchface",
            ) {
                self.uninstall_wait.fail(err);
            }
        }
        Ok(rx)
    }

    fn handle_set_current_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_common_response_code(&message.payload)?;
        if code != 0 {
            bail_site!("vivo set-current dial rejected by watch: code={code}");
        }

        log::info!("[VivoDevice.Watchface] set-current dial succeeded");
        self.set_current_wait.fulfill(());
        Ok(())
    }

    fn handle_manage_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_common_response_code(&message.payload)?;
        if code != 0 {
            bail_site!("vivo dial manage rejected by watch: code={code}");
        }

        log::info!("[VivoDevice.Watchface] dial manage succeeded");
        self.uninstall_wait.fulfill(());
        Ok(())
    }
}

impl HasVivoRequestContext for WatchfaceSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for WatchfaceSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_DIAL {
            return;
        }

        let result = match message.cid {
            cid if cid == response_cid(CID_SET_CURRENT_DIAL) => {
                self.handle_set_current_response(message)
            }
            cid if cid == response_cid(CID_MANAGE_DIAL) => self.handle_manage_response(message),
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice.Watchface] message handling failed: {err:?}");
            match message.cid {
                cid if cid == response_cid(CID_SET_CURRENT_DIAL) => {
                    self.set_current_wait.fail(anyhow_site!("{err:#}"));
                }
                cid if cid == response_cid(CID_MANAGE_DIAL) => {
                    self.uninstall_wait.fail(anyhow_site!("{err:#}"));
                }
                _ => {}
            }
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct WatchfaceComponent {}

impl WatchfaceComponent {
    pub fn new() -> Self {
        Self {}
    }
}

fn parse_dial_id(value: &str) -> anyhow::Result<i64> {
    value
        .trim()
        .parse::<i64>()
        .map_err(|err| anyhow_site!("invalid vivo dial id `{value}`: {err}"))
}

fn current_dial_order(owner_id: &str) -> anyhow::Result<u8> {
    let owner = owner_id.to_string();
    let count =
        with_device_component_mut::<ResourceComponent, _, _>(owner, |comp| comp.watchfaces.len())
            .map_err(|err| anyhow_site!("failed to read vivo dial list length: {err:?}"))?;
    u8::try_from(count).map_err(|_| anyhow_site!("vivo dial count does not fit u8: {count}"))
}

fn build_uninstall_context(owner_id: &str, delete_dial_id: i64) -> anyhow::Result<(i64, Vec<i64>)> {
    let owner = owner_id.to_string();
    with_device_component_mut::<ResourceComponent, _, _>(owner, move |comp| {
        let current_dial_id = comp.current_dial_id.ok_or_else(|| {
            anyhow_site!("vivo current dial id is unknown; refresh dial list first")
        })?;
        if current_dial_id == delete_dial_id {
            bail_site!("vivo refuses to delete the current dial; switch away first");
        }

        let remaining = comp
            .watchfaces
            .iter()
            .filter_map(|item| item.id.parse::<i64>().ok())
            .filter(|id| *id != delete_dial_id)
            .collect::<Vec<_>>();
        Ok((current_dial_id, remaining))
    })
    .map_err(|err| anyhow_site!("failed to read vivo dial uninstall context: {err:?}"))?
}

fn build_set_current_dial_payload(order: u8, dial_id: i64) -> anyhow::Result<Vec<u8>> {
    let dial_id = u32::try_from(dial_id)
        .map_err(|_| anyhow_site!("vivo dial id does not fit u32: {dial_id}"))?;
    let mut out = Vec::with_capacity(6);
    write_u8(&mut out, order);
    write_u32(&mut out, dial_id);
    Ok(out)
}

fn write_u8(out: &mut Vec<u8>, value: u8) {
    if value <= 0x7f {
        out.push(value);
    } else {
        out.push(0xcc);
        out.push(value);
    }
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.push(0xce);
    out.extend_from_slice(&value.to_be_bytes());
}

fn decode_common_response_code(payload: &[u8]) -> anyhow::Result<i32> {
    let mut reader = MsgpackReader::new(payload);
    reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo common response code: {err}"))
}
