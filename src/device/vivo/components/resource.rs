use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::{
        generated::typed::{BleGetFreeStorageResp, SyncInstallInfoReq},
        response_cid,
    },
    msgpack::{MsgpackReader, write_i32},
};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::{Component, access::with_device_component_mut},
};

use super::shared::{HasVivoRequestContext, RequestSlot, VivoRequestExt};

const BID_DIAL: u8 = 1;
const CID_DIAL_LIST: u8 = 7;
const CID_DIAL_FREE_STORAGE: u8 = 21;
const BID_APP_V1: u8 = 40;
const CID_APP_LIST: u8 = 4;
const BID_APP_V2: u8 = 41;
const CID_SYNC_INSTALL_INFO: u8 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VivoWatchFaceItem {
    pub id: String,
    pub name: String,
    pub is_current: bool,
    pub can_remove: Option<bool>,
    pub version_code: Option<u64>,
    pub can_edit: Option<bool>,
    pub background_color: String,
    pub background_image: String,
    pub style: String,
    pub data_list: Vec<i32>,
    pub support_image_format: Option<i32>,
    #[serde(rename = "backgroundImage_list")]
    pub background_image_list: Vec<String>,
    pub slot_item_list: Vec<VivoWatchFaceSlotItem>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VivoWatchFaceSlotItem {
    pub slot_id: String,
    pub widget_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VivoQuickAppItem {
    pub package_name: String,
    pub fingerprint: Vec<u8>,
    pub version_code: u32,
    pub can_remove: bool,
    pub app_name: String,
}

#[derive(Debug, Clone)]
pub struct VivoDialEntry {
    pub dial_id: i64,
    pub version: i32,
    pub dial_type: Option<i32>,
    pub name: Option<String>,
    pub preview_path: Option<String>,
}

#[derive(Component)]
pub struct ResourceSystem {
    owner_id: String,
    tk_handle: Handle,
    watchface_wait: RequestSlot<Vec<VivoWatchFaceItem>>,
    quick_app_wait: RequestSlot<Vec<VivoQuickAppItem>>,
    dial_free_storage_wait: RequestSlot<u64>,
}

impl ResourceSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            watchface_wait: RequestSlot::new(),
            quick_app_wait: RequestSlot::new(),
            dial_free_storage_wait: RequestSlot::new(),
        }
    }

    pub fn request_watchface_list(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<Vec<VivoWatchFaceItem>>> {
        let (rx, should_enqueue) = self.watchface_wait.prepare();
        if should_enqueue {
            log::info!("[VivoDevice.Resource] requesting dial list");
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_DIAL, CID_DIAL_LIST, Vec::new()),
                "VivoResourceSystem::request_watchface_list",
            ) {
                self.watchface_wait.fail(err);
            }
        }
        rx
    }

    pub fn request_dial_free_storage(&mut self) -> oneshot::Receiver<anyhow::Result<u64>> {
        let (rx, should_enqueue) = self.dial_free_storage_wait.prepare();
        if should_enqueue {
            log::info!("[VivoDevice.Resource] requesting dial free storage");
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_DIAL, CID_DIAL_FREE_STORAGE, Vec::new()),
                "VivoResourceSystem::request_dial_free_storage",
            ) {
                self.dial_free_storage_wait.fail(err);
            }
        }
        rx
    }

    pub fn request_quick_app_list(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<Vec<VivoQuickAppItem>>> {
        let (rx, should_enqueue) = self.quick_app_wait.prepare();
        if should_enqueue {
            log::info!("[VivoDevice.Resource] requesting quick-app list");
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_APP_V1, CID_APP_LIST, Vec::new()),
                "VivoResourceSystem::request_quick_app_list",
            ) {
                self.quick_app_wait.fail(err);
            }
        }
        rx
    }

    fn handle_watchface_list_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let decoded = decode_watchface_list(&message.payload)?;
        log::info!(
            "[VivoDevice.Resource] dial list received count={} current={}",
            decoded.items.len(),
            decoded.current_dial_id
        );

        let items = decoded
            .items
            .into_iter()
            .map(|item| watchface_item_from_dial_entry(item, decoded.current_dial_id))
            .collect::<Vec<_>>();

        let update_items = items.clone();
        with_device_component_mut::<ResourceComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.watchfaces = update_items;
            comp.current_dial_id = Some(decoded.current_dial_id);
        })
        .map_err(|err| anyhow_site!("failed to update vivo dial list component: {err:?}"))?;

        emit_resource_changed(&self.owner_id);
        self.watchface_wait.fulfill(items);
        Ok(())
    }

    fn handle_quick_app_list_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let items = decode_quick_app_list(&message.payload)?;
        log::info!(
            "[VivoDevice.Resource] quick-app list received count={}",
            items.len()
        );

        let update_items = items.clone();
        with_device_component_mut::<ResourceComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.quick_apps = update_items;
        })
        .map_err(|err| anyhow_site!("failed to update vivo quick-app list component: {err:?}"))?;

        emit_resource_changed(&self.owner_id);
        self.quick_app_wait.fulfill(items);
        Ok(())
    }

    fn handle_sync_install_info(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let req = SyncInstallInfoReq::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo install info push: {err}"))?;
        log::info!(
            "[VivoDevice.Resource] install info push app_id={} state={} value={} from={}",
            req.app_id,
            req.state,
            req.value,
            req.e_from
        );

        let mut payload = Vec::new();
        write_i32(&mut payload, 0);
        self.send_vivo_message(
            VscpMessage::new(BID_APP_V2, response_cid(CID_SYNC_INSTALL_INFO), payload),
            "VivoResourceSystem::ack_sync_install_info",
        )
    }

    fn handle_dial_free_storage_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = BleGetFreeStorageResp::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo dial free storage: {err}"))?;
        if resp.code != 0 {
            bail_site!(
                "vivo dial free storage rejected by watch: code={}",
                resp.code
            );
        }
        let free = u64::try_from(resp.freesize.max(0))
            .map_err(|_| anyhow_site!("vivo dial free storage does not fit u64"))?;
        log::info!("[VivoDevice.Resource] dial free storage={free}");
        with_device_component_mut::<ResourceComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.dial_free_storage = Some(free);
        })
        .map_err(|err| anyhow_site!("failed to update vivo dial free storage: {err:?}"))?;
        emit_resource_changed(&self.owner_id);
        self.dial_free_storage_wait.fulfill(free);
        Ok(())
    }
}

impl HasVivoRequestContext for ResourceSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for ResourceSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        let result = match (message.bid, message.cid) {
            (BID_DIAL, cid) if cid == response_cid(CID_DIAL_LIST) => {
                self.handle_watchface_list_response(message)
            }
            (BID_DIAL, cid) if cid == response_cid(CID_DIAL_FREE_STORAGE) => {
                self.handle_dial_free_storage_response(message)
            }
            (BID_APP_V1, cid) if cid == response_cid(CID_APP_LIST) => {
                self.handle_quick_app_list_response(message)
            }
            (BID_APP_V2, CID_SYNC_INSTALL_INFO) => self.handle_sync_install_info(message),
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice.Resource] message handling failed: {err:?}");
            match (message.bid, message.cid) {
                (BID_DIAL, cid) if cid == response_cid(CID_DIAL_LIST) => {
                    self.watchface_wait.fail(anyhow_site!("{err:#}"));
                }
                (BID_DIAL, cid) if cid == response_cid(CID_DIAL_FREE_STORAGE) => {
                    self.dial_free_storage_wait.fail(anyhow_site!("{err:#}"));
                }
                (BID_APP_V1, cid) if cid == response_cid(CID_APP_LIST) => {
                    self.quick_app_wait.fail(anyhow_site!("{err:#}"));
                }
                _ => {}
            }
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct ResourceComponent {
    pub watchfaces: Vec<VivoWatchFaceItem>,
    pub quick_apps: Vec<VivoQuickAppItem>,
    pub current_dial_id: Option<i64>,
    pub dial_free_storage: Option<u64>,
}

impl ResourceComponent {
    pub fn new() -> Self {
        Self {
            watchfaces: Vec::new(),
            quick_apps: Vec::new(),
            current_dial_id: None,
            dial_free_storage: None,
        }
    }
}

struct WatchfaceListPayload {
    items: Vec<VivoDialEntry>,
    current_dial_id: i64,
}

fn decode_watchface_list(payload: &[u8]) -> anyhow::Result<WatchfaceListPayload> {
    let mut reader = MsgpackReader::new(payload);
    let code = reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo dial list code: {err}"))?;
    if code != 0 {
        bail_site!("vivo dial list rejected by watch: code={code}");
    }

    let count = reader
        .read_array_len()
        .map_err(|err| anyhow_site!("failed to decode vivo dial list array: {err}"))?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let entry_payload = reader
            .read_bin()
            .map_err(|err| anyhow_site!("failed to decode vivo dial list entry bin: {err}"))?;
        items.push(decode_dial_entry(&entry_payload)?);
    }

    let current_dial_id = reader
        .read_i64()
        .map_err(|err| anyhow_site!("failed to decode vivo current dial id: {err}"))?;
    Ok(WatchfaceListPayload {
        items,
        current_dial_id,
    })
}

fn decode_dial_entry(payload: &[u8]) -> anyhow::Result<VivoDialEntry> {
    let mut reader = MsgpackReader::new(payload);
    let dial_id = reader
        .read_i64()
        .map_err(|err| anyhow_site!("failed to decode vivo dial id: {err}"))?;
    let version = reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo dial version: {err}"))?;

    if reader.has_next() {
        let _ = reader.read_i32();
    }

    let (dial_type, name, preview_path) = if reader.has_next() {
        let dial_type = reader
            .read_i32()
            .map_err(|err| anyhow_site!("failed to decode vivo dial type: {err}"))?;
        let name = reader
            .read_str()
            .map_err(|err| anyhow_site!("failed to decode vivo dial name: {err}"))?;
        let preview_path = reader
            .read_str()
            .map_err(|err| anyhow_site!("failed to decode vivo dial preview path: {err}"))?;
        (Some(dial_type), Some(name), Some(preview_path))
    } else {
        (None, None, None)
    };

    Ok(VivoDialEntry {
        dial_id,
        version,
        dial_type,
        name,
        preview_path,
    })
}

fn decode_quick_app_list(payload: &[u8]) -> anyhow::Result<Vec<VivoQuickAppItem>> {
    let mut reader = MsgpackReader::new(payload);
    let code = reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo quick-app list code: {err}"))?;
    if code != 0 {
        bail_site!("vivo quick-app list rejected by watch: code={code}");
    }

    if !reader.has_next() {
        return Ok(Vec::new());
    }

    let count = reader
        .read_array_len()
        .map_err(|err| anyhow_site!("failed to decode vivo quick-app array: {err}"))?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(decode_quick_app_entry(&mut reader)?);
    }
    Ok(items)
}

fn decode_quick_app_entry(reader: &mut MsgpackReader<'_>) -> anyhow::Result<VivoQuickAppItem> {
    let (app_id, version_code, app_version) = if is_bin_marker(reader.peek_marker()) {
        let payload = reader
            .read_bin()
            .map_err(|err| anyhow_site!("failed to decode vivo quick-app entry bin: {err}"))?;
        let mut inner = MsgpackReader::new(&payload);
        read_quick_app_entry_fields(&mut inner)?
    } else {
        read_quick_app_entry_fields(reader)?
    };

    let app_name = if app_version.trim().is_empty() {
        app_id.clone()
    } else {
        format!("{app_id} {app_version}")
    };

    Ok(VivoQuickAppItem {
        package_name: app_id,
        fingerprint: Vec::new(),
        version_code: version_code.max(0) as u32,
        can_remove: true,
        app_name,
    })
}

fn read_quick_app_entry_fields(
    reader: &mut MsgpackReader<'_>,
) -> anyhow::Result<(String, i32, String)> {
    let app_id = reader
        .read_str()
        .map_err(|err| anyhow_site!("failed to decode vivo quick-app id: {err}"))?;
    let version_code = reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo quick-app version code: {err}"))?;
    let app_version = reader
        .read_str()
        .map_err(|err| anyhow_site!("failed to decode vivo quick-app version: {err}"))?;
    Ok((app_id, version_code, app_version))
}

fn watchface_item_from_dial_entry(item: VivoDialEntry, current_dial_id: i64) -> VivoWatchFaceItem {
    let id = item.dial_id.to_string();
    VivoWatchFaceItem {
        id: id.clone(),
        name: item.name.unwrap_or_else(|| id.clone()),
        is_current: item.dial_id == current_dial_id,
        can_remove: Some(item.dial_id != current_dial_id),
        version_code: Some(item.version.max(0) as u64),
        can_edit: Some(item.dial_type.is_some()),
        background_color: String::new(),
        background_image: item.preview_path.unwrap_or_default(),
        style: String::new(),
        data_list: Vec::new(),
        support_image_format: None,
        background_image_list: Vec::new(),
        slot_item_list: Vec::new(),
    }
}

fn is_bin_marker(marker: Option<u8>) -> bool {
    matches!(marker, Some(0xc4 | 0xc5 | 0xc6))
}

fn emit_resource_changed(owner_id: &str) {
    crate::events::emit(crate::events::CoreEvent::DeviceStateChanged(
        crate::events::DeviceStateChanged {
            device_addr: owner_id.to_string(),
        },
    ));
}
