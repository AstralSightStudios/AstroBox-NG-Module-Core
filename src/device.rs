use crate::device::xiaomi::components::auth::{AuthComponent, AuthSystem};
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkComponent;
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkSystem;
use crate::device::xiaomi::config::XiaomiDeviceConfig;
use crate::device::xiaomi::r#type::ConnectType;
use crate::device::xiaomi::{SendError, XiaomiDevice, cleanup_cached_state};
use crate::ecs::component::Component;
use crate::ecs::entity::{EntityExt, EntityMeta};
use crate::impl_has_entity_meta;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::runtime::Handle;

pub mod xiaomi;

#[derive(Serialize)]
pub struct Device {
    #[serde(skip_serializing)]
    meta: EntityMeta,
    pub name: String,
    pub addr: String,
}

impl Device {
    pub fn new(name: String, addr: String) -> Self {
        Self {
            meta: EntityMeta {
                id: addr.clone(),
                ..Default::default()
            },
            name,
            addr,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
}

impl_has_entity_meta!(Device, meta);

#[derive(Serialize, Deserialize, Clone)]
pub struct DeviceConnectionInfo {
    pub name: String,
    pub addr: String,
}

pub async fn create_miwear_device<F, Fut>(
    tk_handle: Handle,
    name: String,
    addr: String,
    authkey: String,
    sar_version: u32,
    connect_type: ConnectType,
    force_android: bool,
    sender: F,
) -> anyhow::Result<DeviceConnectionInfo>
where
    F: Fn(Vec<u8>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), SendError>> + Send + 'static,
{
    let device_id_for_auth = addr.clone();
    let device_id_for_network = addr.clone();
    let addr_for_entity = addr.clone();
    let name_for_entity = name.clone();
    let tk_handle_clone = tk_handle.clone();

    cleanup_cached_state(&addr);

    crate::ecs::with_rt_mut(move |rt| {
        let device_config = XiaomiDeviceConfig::default();
        let dev = XiaomiDevice::new(
            tk_handle_clone.clone(),
            name_for_entity.clone(),
            addr_for_entity.clone(),
            authkey,
            sar_version,
            connect_type,
            force_android,
            device_config,
            sender,
        );
        rt.add_entity(dev);
    })
    .await;

    let auth_rx = crate::ecs::with_rt_mut(move |rt| {
        if let Some(dev) = rt.find_entity_by_id_mut::<XiaomiDevice>(&device_id_for_auth) {
            dev.get_component_as_mut::<AuthComponent>(AuthComponent::ID)
                .unwrap()
                .as_logic_component_mut()
                .unwrap()
                .system_mut()
                .as_any_mut()
                .downcast_mut::<AuthSystem>()
                .unwrap()
                .prepare_auth()
                .map(Some)
        } else {
            Ok(None)
        }
    })
    .await?;

    if let Some(rx) = auth_rx {
        let auth_result = rx.await.context("Auth await response not received")?;
        auth_result?;
    }

    #[cfg(not(target_arch = "wasm32"))]
    // 在Auth完成后同步网络状态以确保蓝牙联网可用
    crate::ecs::with_rt_mut(move |rt| {
        if let Some(dev) = rt.find_entity_by_id_mut::<XiaomiDevice>(&device_id_for_network) {
            dev.get_component_as_mut::<NetworkComponent>(NetworkComponent::ID)
                .unwrap()
                .as_logic_component_mut()
                .unwrap()
                .system_mut()
                .as_any_mut()
                .downcast_mut::<NetworkSystem>()
                .unwrap()
                .sync_network_status()
                .map(Some)
        } else {
            Ok(None)
        }
    })
    .await?;

    Ok(DeviceConnectionInfo {
        name: name.clone(),
        addr: addr.clone(),
    })
}
