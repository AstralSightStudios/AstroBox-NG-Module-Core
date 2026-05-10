use std::convert::TryFrom;

use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::messages::{
    device_info::{BID_DEVICE_INFO, CID_DEVICE_INFO, DeviceInfoRequest, WatchFirstSyncResp},
    generated::typed::WatchNormalSyncResp,
    response_cid,
};

use crate::{
    anyhow_site, bail_site,
    device::{
        data::{
            BatteryData, ChargeInfoData, ChargeStatusData, DeviceInfoData, DeviceStatusData,
            StorageStatusData,
        },
        vivo::{
            system::{VivoSystemExt, register_vivo_system_ext_on_message},
            transport::vscp::VscpMessage,
        },
    },
    ecs::Component,
};

use super::shared::{HasVivoRequestContext, RequestSlot, VivoRequestExt};

const CID_NORMAL_SYNC: u8 = 4;

#[derive(Component)]
pub struct InfoSystem {
    owner_id: String,
    tk_handle: Handle,
    device_info_wait: RequestSlot<DeviceInfoData>,
    device_status_wait: RequestSlot<DeviceStatusData>,
    device_storage_wait: RequestSlot<StorageStatusData>,
}

impl InfoSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            device_info_wait: RequestSlot::new(),
            device_status_wait: RequestSlot::new(),
            device_storage_wait: RequestSlot::new(),
        }
    }

    pub fn request_device_info(&mut self) -> oneshot::Receiver<anyhow::Result<DeviceInfoData>> {
        let (rx, should_enqueue) = self.device_info_wait.prepare();
        if should_enqueue {
            match build_device_info_request_payload() {
                Ok(payload) => {
                    if let Err(err) = self.send_vivo_message(
                        VscpMessage::new(BID_DEVICE_INFO, CID_DEVICE_INFO, payload),
                        "VivoInfoSystem::request_device_info",
                    ) {
                        log::warn!("[VivoDevice.Info] failed to request first-sync info: {err:?}");
                        self.device_info_wait.fail(err);
                    }
                }
                Err(err) => {
                    log::warn!("[VivoDevice.Info] failed to build first-sync request: {err:?}");
                    self.device_info_wait.fail(err);
                }
            }
        }
        rx
    }

    pub fn request_device_status(&mut self) -> oneshot::Receiver<anyhow::Result<DeviceStatusData>> {
        let (rx, should_enqueue) = self.device_status_wait.prepare();
        if should_enqueue {
            self.enqueue_normal_sync_request("VivoInfoSystem::request_device_status");
        }
        rx
    }

    pub fn request_device_storage(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<StorageStatusData>> {
        let (rx, should_enqueue) = self.device_storage_wait.prepare();
        if should_enqueue {
            self.enqueue_normal_sync_request("VivoInfoSystem::request_device_storage");
        }
        rx
    }

    fn enqueue_normal_sync_request(&mut self, log_ctx: &'static str) {
        if let Err(err) = self.send_vivo_message(
            VscpMessage::new(BID_DEVICE_INFO, CID_NORMAL_SYNC, Vec::new()),
            log_ctx,
        ) {
            log::warn!("[VivoDevice.Info] failed to request normal sync: {err:?}");
            self.device_status_wait.fail(anyhow_site!("{err:#}"));
            self.device_storage_wait.fail(anyhow_site!("{err:#}"));
        }
    }

    fn handle_first_sync_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = WatchFirstSyncResp::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo first-sync response: {err}"))?;
        if resp.code != 0 {
            let err = anyhow_site!("vivo first-sync rejected by watch: code={}", resp.code);
            self.device_info_wait.fail(anyhow_site!("{err:#}"));
            return Err(err);
        }

        log::debug!(
            "[VivoDevice.Info] first-sync received model={} sn={} version={} total={} free={} battery={}",
            resp.model,
            resp.sn,
            resp.version,
            resp.total_storage,
            resp.free_storage,
            resp.battry
        );

        let info = device_info_from_first_sync(&self.owner_id, &resp);
        let status = device_status_from_parts(resp.battry, -1);
        let storage = storage_from_parts(resp.total_storage, resp.free_storage);

        let update_res = crate::ecs::access::with_device_component_mut::<InfoComponent, _, _>(
            self.owner_id.clone(),
            {
                let info = info.clone();
                let status = status.clone();
                let storage = storage.clone();
                move |comp| {
                    comp.info = Some(info);
                    comp.status = Some(status);
                    comp.storage = Some(storage);
                }
            },
        );

        match update_res {
            Ok(_) => {
                crate::events::emit(crate::events::CoreEvent::DeviceStateChanged(
                    crate::events::DeviceStateChanged {
                        device_addr: self.owner_id.clone(),
                    },
                ));
                self.device_info_wait.fulfill(info);
                self.device_status_wait.fulfill(status);
                self.device_storage_wait.fulfill(storage);
                Ok(())
            }
            Err(err) => {
                let anyhow_err = anyhow_site!("failed to update vivo info component: {err:?}");
                self.device_info_wait.fail(anyhow_site!("{anyhow_err:#}"));
                self.device_status_wait.fail(anyhow_site!("{anyhow_err:#}"));
                self.device_storage_wait
                    .fail(anyhow_site!("{anyhow_err:#}"));
                Err(anyhow_err)
            }
        }
    }

    fn handle_normal_sync_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = WatchNormalSyncResp::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo normal-sync response: {err}"))?;
        if resp.code != 0 {
            let err = anyhow_site!("vivo normal-sync rejected by watch: code={}", resp.code);
            self.device_status_wait.fail(anyhow_site!("{err:#}"));
            self.device_storage_wait.fail(anyhow_site!("{err:#}"));
            return Err(err);
        }

        log::debug!(
            "[VivoDevice.Info] normal-sync received total={} free={} battery={} battery_state={} wear_state={}",
            resp.total_storage,
            resp.free_storage,
            resp.battery,
            resp.battery_state,
            resp.wear_state
        );

        let status = device_status_from_parts(resp.battery, resp.battery_state);
        let storage = storage_from_parts(resp.total_storage, resp.free_storage);
        let update_res = crate::ecs::access::with_device_component_mut::<InfoComponent, _, _>(
            self.owner_id.clone(),
            {
                let status = status.clone();
                let storage = storage.clone();
                move |comp| {
                    comp.status = Some(status);
                    comp.storage = Some(storage);
                    comp.wear_state = Some(resp.wear_state);
                }
            },
        );

        match update_res {
            Ok(_) => {
                crate::events::emit(crate::events::CoreEvent::DeviceStateChanged(
                    crate::events::DeviceStateChanged {
                        device_addr: self.owner_id.clone(),
                    },
                ));
                self.device_status_wait.fulfill(status);
                self.device_storage_wait.fulfill(storage);
                Ok(())
            }
            Err(err) => {
                let anyhow_err =
                    anyhow_site!("failed to update vivo normal-sync component: {err:?}");
                self.device_status_wait.fail(anyhow_site!("{anyhow_err:#}"));
                self.device_storage_wait
                    .fail(anyhow_site!("{anyhow_err:#}"));
                Err(anyhow_err)
            }
        }
    }
}

impl HasVivoRequestContext for InfoSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for InfoSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_DEVICE_INFO {
            return;
        }

        let result = match message.cid {
            WatchFirstSyncResp::CID => self.handle_first_sync_response(message),
            cid if cid == response_cid(CID_NORMAL_SYNC) => {
                self.handle_normal_sync_response(message)
            }
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice.Info] message handling failed: {err:?}");
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct InfoComponent {
    pub info: Option<DeviceInfoData>,
    pub status: Option<DeviceStatusData>,
    pub storage: Option<StorageStatusData>,
    pub wear_state: Option<i32>,
}

impl InfoComponent {
    pub fn new() -> Self {
        Self {
            info: None,
            status: None,
            storage: None,
            wear_state: None,
        }
    }
}

pub fn update_first_sync_component(
    owner_id: String,
    resp: &WatchFirstSyncResp,
) -> anyhow::Result<()> {
    if resp.code != 0 {
        bail_site!("vivo first-sync rejected by watch: code={}", resp.code);
    }

    let info = device_info_from_first_sync(&owner_id, resp);
    let status = device_status_from_parts(resp.battry, -1);
    let storage = storage_from_parts(resp.total_storage, resp.free_storage);
    crate::ecs::access::with_device_component_mut::<InfoComponent, _, _>(owner_id, move |comp| {
        comp.info = Some(info);
        comp.status = Some(status);
        comp.storage = Some(storage);
    })
    .map_err(|err| anyhow_site!("failed to update vivo first-sync component: {err:?}"))?;
    Ok(())
}

fn device_info_from_first_sync(owner_id: &str, resp: &WatchFirstSyncResp) -> DeviceInfoData {
    let ota_device = resp.ota_device.trim();
    let product_device = if ota_device.is_empty() {
        resp.model.clone()
    } else {
        resp.ota_device.clone()
    };
    let hard_version = if ota_device.is_empty() {
        vivo_hard_version(&resp.version)
    } else {
        resp.ota_device.clone()
    };

    DeviceInfoData {
        serial_number: resp.sn.clone(),
        firmware_version: resp.version.clone(),
        imei: resp.imei.clone(),
        model: resp.model.clone(),
        product_device,
        mac_address: Some(owner_id.to_string()),
        version_type: Some(resp.version_type),
        hard_version: Some(hard_version).filter(|value| !value.trim().is_empty()),
        os_version: Some(vivo_os_version(&resp.version)).filter(|value| !value.trim().is_empty()),
    }
}

fn vivo_os_version(version: &str) -> String {
    let version = version.trim();
    if version.is_empty() || !version.contains('_') {
        return version.to_string();
    }

    let parts: Vec<&str> = version.split('_').collect();
    if parts.len() < 3 {
        return version.to_string();
    }
    parts.get(2).copied().unwrap_or(version).to_string()
}

fn vivo_hard_version(version: &str) -> String {
    let version = version.trim();
    if version.is_empty() || !version.contains('_') {
        return String::new();
    }

    let parts: Vec<&str> = version.split('_').collect();
    if parts.len() < 3 {
        return String::new();
    }
    parts[..2].join("_")
}

fn device_status_from_parts(battery: i32, battery_state: i32) -> DeviceStatusData {
    DeviceStatusData {
        battery: BatteryData {
            capacity: battery.clamp(0, 100),
            charge_status: Some(charge_status_from_vivo(battery_state)),
            charge_info: Some(ChargeInfoData {
                state: battery_state,
                timestamp: None,
            }),
        },
    }
}

fn charge_status_from_vivo(battery_state: i32) -> ChargeStatusData {
    match battery_state {
        0 => ChargeStatusData::Charging,
        1 => ChargeStatusData::NotCharging,
        15 => ChargeStatusData::Full,
        _ => ChargeStatusData::Unknown,
    }
}

fn storage_from_parts(total_storage: i64, free_storage: i64) -> StorageStatusData {
    let total = u64::try_from(total_storage.max(0)).unwrap_or_default();
    let free = u64::try_from(free_storage.max(0)).unwrap_or_default();
    StorageStatusData {
        used: total.saturating_sub(free),
        total,
    }
}

fn build_device_info_request_payload() -> anyhow::Result<Vec<u8>> {
    let (unix_time_sec, millis) = current_unix_time_parts()?;
    let timezone_offset_sec = local_timezone_offset_sec();
    DeviceInfoRequest {
        unix_time_sec,
        gmt_timezone: gmt_timezone_from_offset(timezone_offset_sec),
        os_type: 1,
        millis,
        timezone_offset_sec: Some(timezone_offset_sec),
    }
    .payload()
    .map_err(|err| anyhow_site!("failed to encode vivo device info request: {err}"))
}

fn current_unix_time_parts() -> anyhow::Result<(i32, i32)> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let now = chrono::Local::now();
        let sec = i32::try_from(now.timestamp())
            .map_err(|_| anyhow_site!("current unix timestamp does not fit i32"))?;
        let millis = i32::try_from(now.timestamp_subsec_millis())
            .map_err(|_| anyhow_site!("current millis does not fit i32"))?;
        Ok((sec, millis))
    }

    #[cfg(target_arch = "wasm32")]
    {
        use web_time::{SystemTime, UNIX_EPOCH};

        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| anyhow_site!("system time before unix epoch: {err:?}"))?;
        let sec = i32::try_from(duration.as_secs())
            .map_err(|_| anyhow_site!("current unix timestamp does not fit i32"))?;
        let millis = i32::try_from(duration.subsec_millis())
            .map_err(|_| anyhow_site!("current millis does not fit i32"))?;
        Ok((sec, millis))
    }
}

fn local_timezone_offset_sec() -> i32 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        chrono::Local::now().offset().local_minus_utc()
    }

    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

fn gmt_timezone_from_offset(offset_sec: i32) -> i32 {
    let hours = offset_sec / 3600;
    if hours < 0 { hours + 24 } else { hours }
}
