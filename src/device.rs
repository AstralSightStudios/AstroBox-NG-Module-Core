use crate::device::xiaomi::components::{
    auth::{AuthComponent, AuthSystem},
    info::{InfoComponent, InfoSystem},
    install::{InstallComponent, InstallSystem},
    mass::{MassComponent, MassSystem},
    resource::{ResourceComponent, ResourceSystem},
    sync::{SyncComponent, SyncSystem},
    thirdparty_app::{ThirdpartyAppComponent, ThirdpartyAppSystem},
    watchface::{WatchfaceComponent, WatchfaceSystem},
};
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkComponent;
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkSystem;
use crate::device::xiaomi::config::XiaomiDeviceConfig;
use crate::device::xiaomi::r#type::ConnectType;
use crate::device::xiaomi::{SendError, XiaomiDevice, cleanup_cached_state};
use crate::ecs::Component;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::runtime::Handle;

pub mod xiaomi;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Xiaomi,
}

#[derive(Component, Serialize)]
pub struct Device {
    pub name: String,
    pub addr: String,
    pub kind: DeviceKind,
}

impl Device {
    pub fn new(name: String, addr: String, kind: DeviceKind) -> Self {
        Self {
            name,
            addr,
            kind,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn kind(&self) -> DeviceKind {
        self.kind
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct DeviceConnectionInfo {
    pub name: String,
    pub addr: String,
}

pub fn cleanup_device_state(kind: DeviceKind, addr: &str) {
    match kind {
        DeviceKind::Xiaomi => cleanup_cached_state(addr),
    }
}

pub async fn create_device<F, Fut>(
    tk_handle: Handle,
    device_kind: DeviceKind,
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
    match device_kind {
        DeviceKind::Xiaomi => {
            let device_id_for_auth = addr.clone();
            #[cfg(not(target_arch = "wasm32"))]
            let device_id_for_network = addr.clone();
            let addr_for_entity = addr.clone();
            let name_for_entity = name.clone();
            let tk_handle_clone = tk_handle.clone();

            cleanup_device_state(device_kind, &addr);

            crate::ecs::with_rt_mut(move |rt| {
                let device_config = XiaomiDeviceConfig::default();
                #[cfg(not(target_arch = "wasm32"))]
                let network_config = device_config.network.clone();
                let authkey_for_component = authkey.clone();
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
                let device_id = dev.addr().to_string();
                let entity = rt.spawn_device(
                    device_id.clone(),
                    (
                        dev,
                        Device::new(
                            name_for_entity.clone(),
                            addr_for_entity.clone(),
                            device_kind,
                        ),
                    ),
                );
                let mut entity_ref = rt.world_mut().entity_mut(entity);
                entity_ref.insert((
                    AuthComponent::new(authkey_for_component),
                    AuthSystem::new(device_id.clone()),
                    InstallComponent::new(),
                    InstallSystem::new(device_id.clone()),
                    MassComponent::new(),
                    MassSystem::new(device_id.clone()),
                    InfoComponent::new(),
                    InfoSystem::new(device_id.clone()),
                ));
                entity_ref.insert((
                    ThirdpartyAppComponent::new(),
                    ThirdpartyAppSystem::new(device_id.clone()),
                    ResourceComponent::new(),
                    ResourceSystem::new(device_id.clone()),
                    WatchfaceComponent::new(),
                    WatchfaceSystem::new(device_id.clone()),
                    SyncComponent::new(),
                    SyncSystem::new(device_id.clone()),
                ));
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let network_config_for_runtime = network_config.clone();
                    entity_ref.insert((
                        NetworkComponent::new(network_config),
                        NetworkSystem::new(device_id.clone()),
                    ));
                    if let Some(mut sys) = rt.world_mut().get_mut::<NetworkSystem>(entity) {
                        if let Err(err) =
                            sys.ensure_runtime(tk_handle_clone.clone(), network_config_for_runtime)
                        {
                            log::warn!("[XiaomiDevice] failed to start network stack: {err:?}");
                        }
                    }
                }
            })
            .await;

            let auth_rx = crate::ecs::with_rt_mut(move |rt| {
                rt.with_device_mut(&device_id_for_auth, |world, entity| {
                    let mut auth_system = world
                        .get_mut::<AuthSystem>(entity)
                        .expect("AuthSystem missing");
                    auth_system.prepare_auth().map(Some)
                })
                .unwrap_or_else(|| Ok(None))
            })
            .await?;

            if let Some(rx) = auth_rx {
                let auth_result = rx.await.context("Auth await response not received")?;
                auth_result?;
            }

            #[cfg(not(target_arch = "wasm32"))]
            // 在Auth完成后同步网络状态以确保蓝牙联网可用
            crate::ecs::with_rt_mut(move |rt| {
                rt.with_device_mut(&device_id_for_network, |world, entity| {
                    let mut sys = world
                        .get_mut::<NetworkSystem>(entity)
                        .expect("NetworkSystem missing");
                    let _ = sys.sync_network_status();
                });
            })
            .await;

            Ok(DeviceConnectionInfo {
                name: name.clone(),
                addr: addr.clone(),
            })
        }
    }
}
