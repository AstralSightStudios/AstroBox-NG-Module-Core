use crate::device::vivo::{
    VivoConnectType, VivoDevice, VivoDeviceConfig,
    components::auth::{AuthComponent as VivoAuthComponent, AuthSystem as VivoAuthSystem},
    components::cloud_bridge::{
        CloudBridgeComponent as VivoCloudBridgeComponent,
        CloudBridgeSystem as VivoCloudBridgeSystem,
    },
    components::erpc::{ErpcComponent as VivoErpcComponent, ErpcSystem as VivoErpcSystem},
    components::info::{InfoComponent as VivoInfoComponent, InfoSystem as VivoInfoSystem},
    components::install::{
        InstallComponent as VivoInstallComponent, InstallSystem as VivoInstallSystem,
    },
    components::resource::{
        ResourceComponent as VivoResourceComponent, ResourceSystem as VivoResourceSystem,
    },
    components::sync::{SyncComponent as VivoSyncComponent, SyncSystem as VivoSyncSystem},
    components::thirdparty_app::{
        ThirdpartyAppComponent as VivoThirdpartyAppComponent,
        ThirdpartyAppSystem as VivoThirdpartyAppSystem,
    },
    components::watchface::{
        WatchfaceComponent as VivoWatchfaceComponent, WatchfaceSystem as VivoWatchfaceSystem,
    },
};
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkComponent;
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkSystem;
use crate::device::xiaomi::components::{
    auth::{AuthComponent, AuthSystem},
    info::{InfoComponent, InfoSystem},
    install::{InstallComponent, InstallSystem},
    mass::{MassComponent, MassSystem},
    media::{MediaComponent, MediaSystem},
    report::ReportSystem,
    resource::{ResourceComponent, ResourceSystem},
    sync::{SyncComponent, SyncSystem},
    thirdparty_app::{ThirdpartyAppComponent, ThirdpartyAppSystem},
    watchface::{WatchfaceComponent, WatchfaceSystem},
};
use crate::device::xiaomi::config::XiaomiDeviceConfig;
use crate::device::xiaomi::r#type::ConnectType;
use crate::device::xiaomi::{SendError, XiaomiDevice, cleanup_cached_state};
use crate::ecs::Component;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::future::Future;
use tokio::runtime::Handle;

pub mod data;
pub mod install;
pub mod resource;
pub mod sync;
pub mod thirdparty_app;
pub mod vivo;
pub mod watchface;
pub mod xiaomi;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Xiaomi,
    Vivo,
}

impl Default for DeviceKind {
    fn default() -> Self {
        Self::Xiaomi
    }
}

#[derive(Component, Serialize)]
pub struct Device {
    pub name: String,
    pub addr: String,
    pub kind: DeviceKind,
}

impl Device {
    pub fn new(name: String, addr: String, kind: DeviceKind) -> Self {
        Self { name, addr, kind }
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
    #[serde(default)]
    pub kind: DeviceKind,
}

pub fn cleanup_device_state(kind: DeviceKind, addr: &str) {
    match kind {
        DeviceKind::Xiaomi => cleanup_cached_state(addr),
        DeviceKind::Vivo => vivo::cleanup_cached_state(addr),
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
    tx_win_overrun_allowance: Option<u8>,
    transport_chunk_size_spp: Option<usize>,
    transport_chunk_size_ble: Option<usize>,
    force_android: bool,
    sender: F,
) -> anyhow::Result<DeviceConnectionInfo>
where
    F: Fn(Vec<Vec<u8>>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), SendError>> + Send + 'static,
{
    match device_kind {
        DeviceKind::Vivo => {
            bail!(
                "Vivo devices require create_vivo_device because they do not use Xiaomi authkey/SAR options"
            )
        }
        DeviceKind::Xiaomi => {
            let device_id_for_auth = addr.clone();
            #[cfg(not(target_arch = "wasm32"))]
            let device_id_for_network = addr.clone();
            let addr_for_entity = addr.clone();
            let name_for_entity = name.clone();
            let tk_handle_clone = tk_handle.clone();

            cleanup_device_state(device_kind, &addr);

            crate::ecs::with_rt_mut(move |rt| {
                let mut device_config = XiaomiDeviceConfig::default();
                if let Some(allowance) = tx_win_overrun_allowance {
                    device_config.sar.tx_win_overrun_allowance = allowance.min(16);
                }
                if let Some(chunk_size_spp) = transport_chunk_size_spp {
                    device_config.transport.chunk_size_spp = chunk_size_spp.max(1);
                }
                if let Some(chunk_size_ble) = transport_chunk_size_ble {
                    device_config.transport.chunk_size_ble = chunk_size_ble.max(1);
                }
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
                    MediaComponent::default(),
                    MediaSystem::new(device_id.clone()),
                    InfoComponent::new(),
                    InfoSystem::new(device_id.clone()),
                    ReportSystem::new(device_id.clone()),
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
                kind: DeviceKind::Xiaomi,
            })
        }
    }
}

pub async fn create_vivo_device<F, Fut>(
    tk_handle: Handle,
    name: String,
    addr: String,
    connect_type: VivoConnectType,
    config: VivoDeviceConfig,
    sender: F,
) -> anyhow::Result<DeviceConnectionInfo>
where
    F: Fn(Vec<Vec<u8>>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), vivo::SendError>> + Send + 'static,
{
    let device_id_for_auth = addr.clone();
    let addr_for_entity = addr.clone();
    let name_for_entity = name.clone();
    let auth_component = VivoAuthComponent::from_config(&config);
    cleanup_device_state(DeviceKind::Vivo, &addr);

    crate::ecs::with_rt_mut(move |rt| {
        let dev = VivoDevice::new(
            name_for_entity.clone(),
            addr_for_entity.clone(),
            connect_type,
            config,
            sender,
        );
        let device_id = addr_for_entity.clone();
        let entity = rt.spawn_device(
            addr_for_entity.clone(),
            (
                dev,
                Device::new(name_for_entity, addr_for_entity, DeviceKind::Vivo),
            ),
        );
        let mut entity_ref = rt.world_mut().entity_mut(entity);
        entity_ref.insert((
            auth_component,
            VivoAuthSystem::new(device_id.clone(), tk_handle.clone()),
            VivoInfoComponent::new(),
            VivoInfoSystem::new(device_id.clone(), tk_handle.clone()),
            VivoInstallComponent::new(),
            VivoInstallSystem::new(device_id.clone(), tk_handle.clone()),
            VivoResourceComponent::new(),
            VivoResourceSystem::new(device_id.clone(), tk_handle.clone()),
        ));
        entity_ref.insert((
            VivoWatchfaceComponent::new(),
            VivoWatchfaceSystem::new(device_id.clone(), tk_handle.clone()),
            VivoThirdpartyAppComponent::new(),
            VivoThirdpartyAppSystem::new(device_id.clone(), tk_handle.clone()),
            VivoCloudBridgeComponent::new(),
            VivoCloudBridgeSystem::new(device_id.clone(), tk_handle.clone()),
            VivoErpcComponent::new(),
            VivoErpcSystem::new(device_id.clone(), tk_handle.clone()),
            VivoSyncComponent::new(),
            VivoSyncSystem::new(device_id, tk_handle.clone()),
        ));
    })
    .await;

    let auth_rx = crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&device_id_for_auth, |world, entity| {
            let mut auth_system = world
                .get_mut::<VivoAuthSystem>(entity)
                .expect("VivoAuthSystem missing");
            auth_system.prepare_auth().map(Some)
        })
        .unwrap_or_else(|| Ok(None))
    })
    .await?;

    if let Some(rx) = auth_rx {
        let auth_result = rx.await.context("Vivo auth await response not received")?;
        auth_result?;
    }

    Ok(DeviceConnectionInfo {
        name,
        addr,
        kind: DeviceKind::Vivo,
    })
}
