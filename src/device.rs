use crate::device::xiaomi::SendError;
use crate::device::xiaomi::XiaomiDevice;
use crate::device::xiaomi::components::auth::{AuthComponent, AuthSystem};
use crate::device::xiaomi::config::XiaomiDeviceConfig;
use crate::device::xiaomi::r#type::ConnectType;
use crate::ecs::component::Component;
use crate::ecs::entity::EntityExt;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::runtime::Handle;

pub mod xiaomi;

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
    let addr_for_entity = addr.clone();
    let name_for_entity = name.clone();
    let tk_handle_clone = tk_handle.clone();

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
        rx.await.context("Auth await response not received")?;
    }

    Ok(DeviceConnectionInfo {
        name: name.clone(),
        addr: addr.clone(),
    })
}
