use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::{
    anyhow_site,
    device::{
        Device, DeviceKind,
        vivo::components::info::{
            InfoComponent as VivoInfoComponent, InfoSystem as VivoInfoSystem,
        },
        xiaomi::components::info::InfoSystem as XiaomiInfoSystem,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceDataType {
    Info,
    Status,
    Storage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfoData {
    pub serial_number: String,
    pub firmware_version: String,
    pub imei: String,
    pub model: String,
    pub product_device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hard_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatusData {
    pub battery: BatteryData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryData {
    pub capacity: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charge_status: Option<ChargeStatusData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charge_info: Option<ChargeInfoData>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChargeStatusData {
    #[serde(rename = "UNKNOWN")]
    Unknown,
    #[serde(rename = "CHARGING")]
    Charging,
    #[serde(rename = "NOT_CHARGING")]
    NotCharging,
    #[serde(rename = "FULL")]
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChargeInfoData {
    pub state: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStatusData {
    pub used: u64,
    pub total: u64,
}

pub async fn request_device_data_json(
    addr: String,
    data_type: DeviceDataType,
) -> anyhow::Result<serde_json::Value> {
    let kind = device_kind(&addr).await?;
    log::info!(
        "[DeviceData] request addr={} kind={:?} type={:?}",
        addr,
        kind,
        data_type
    );

    match kind {
        DeviceKind::Xiaomi => request_xiaomi_device_data_json(addr, data_type).await,
        DeviceKind::Vivo => request_vivo_device_data_json(addr, data_type).await,
    }
}

async fn device_kind(addr: &str) -> anyhow::Result<DeviceKind> {
    let addr_owned = addr.to_string();
    crate::ecs::with_rt_mut(move |rt| {
        rt.component_ref::<Device>(&addr_owned)
            .map(|device| device.kind())
            .ok_or_else(|| anyhow_site!("Device not found"))
    })
    .await
}

async fn request_xiaomi_device_data_json(
    addr: String,
    data_type: DeviceDataType,
) -> anyhow::Result<serde_json::Value> {
    match data_type {
        DeviceDataType::Info => {
            let rx = with_xiaomi_info_system(addr, |sys| sys.request_device_info()).await?;
            let info = await_slot(rx, "Device info response not received").await?;
            serde_json::to_value(info).map_err(Into::into)
        }
        DeviceDataType::Status => {
            let rx = with_xiaomi_info_system(addr, |sys| sys.request_device_status()).await?;
            let status = await_slot(rx, "Device status response not received").await?;
            serde_json::to_value(status).map_err(Into::into)
        }
        DeviceDataType::Storage => {
            let rx = with_xiaomi_info_system(addr, |sys| sys.request_device_storage()).await?;
            let storage = await_slot(rx, "Device storage info response not received").await?;
            serde_json::to_value(storage).map_err(Into::into)
        }
    }
}

async fn request_vivo_device_data_json(
    addr: String,
    data_type: DeviceDataType,
) -> anyhow::Result<serde_json::Value> {
    match data_type {
        DeviceDataType::Info => {
            let rx = with_vivo_info_system(addr, |sys| sys.request_device_info()).await?;
            let info = await_slot(rx, "Vivo device info response not received").await?;
            serde_json::to_value(info).map_err(Into::into)
        }
        DeviceDataType::Status => {
            let rx = with_vivo_info_system(addr, |sys| sys.request_device_status()).await?;
            let status = await_slot(rx, "Vivo device status response not received").await?;
            serde_json::to_value(status).map_err(Into::into)
        }
        DeviceDataType::Storage => {
            let rx = with_vivo_info_system(addr, |sys| sys.request_device_storage()).await?;
            let storage = await_slot(rx, "Vivo device storage response not received").await?;
            serde_json::to_value(storage).map_err(Into::into)
        }
    }
}

async fn with_xiaomi_info_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut XiaomiInfoSystem) -> R + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<XiaomiInfoSystem>(entity)
                .ok_or_else(|| anyhow_site!("Xiaomi info system not found"))?;
            Ok(f(&mut system))
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn with_vivo_info_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoInfoSystem) -> R + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoInfoSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo info system not found"))?;
            Ok(f(&mut system))
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn await_slot<T>(
    rx: oneshot::Receiver<anyhow::Result<T>>,
    missing_msg: &'static str,
) -> anyhow::Result<T> {
    rx.await.map_err(|_| anyhow_site!("{missing_msg}"))?
}

pub async fn read_cached_vivo_device_info(addr: String) -> Option<DeviceInfoData> {
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            world
                .get::<VivoInfoComponent>(entity)
                .and_then(|comp| comp.info.clone())
        })
        .flatten()
    })
    .await
}
