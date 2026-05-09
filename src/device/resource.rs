use tokio::sync::oneshot;

use crate::{
    anyhow_site,
    device::{
        Device, DeviceKind, vivo::components::resource::ResourceSystem as VivoResourceSystem,
        xiaomi::components::resource::ResourceSystem as XiaomiResourceSystem,
    },
};

pub async fn request_watchface_list_json(addr: String) -> anyhow::Result<serde_json::Value> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let rx = with_xiaomi_resource_system(addr, |sys| sys.request_watchface_list()).await?;
            let items = await_slot(rx, "Watchface list response not received").await?;
            serde_json::to_value(items).map_err(Into::into)
        }
        DeviceKind::Vivo => {
            let rx = with_vivo_resource_system(addr, |sys| sys.request_watchface_list()).await?;
            let items = await_slot(rx, "Vivo watchface list response not received").await?;
            serde_json::to_value(items).map_err(Into::into)
        }
    }
}

pub async fn request_quick_app_list_json(addr: String) -> anyhow::Result<serde_json::Value> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let rx = with_xiaomi_resource_system(addr, |sys| sys.request_quick_app_list()).await?;
            let items = await_slot(rx, "Quick app list response not received").await?;
            serde_json::to_value(items).map_err(Into::into)
        }
        DeviceKind::Vivo => {
            let rx = with_vivo_resource_system(addr, |sys| sys.request_quick_app_list()).await?;
            let items = await_slot(rx, "Vivo quick app list response not received").await?;
            serde_json::to_value(items).map_err(Into::into)
        }
    }
}

pub async fn request_dial_free_storage_json(addr: String) -> anyhow::Result<serde_json::Value> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            anyhow::bail!("Xiaomi dial free storage uses the generic storage endpoint")
        }
        DeviceKind::Vivo => {
            let rx = with_vivo_resource_system(addr, |sys| sys.request_dial_free_storage()).await?;
            let free = await_slot(rx, "Vivo dial free storage response not received").await?;
            serde_json::to_value(free).map_err(Into::into)
        }
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

async fn with_xiaomi_resource_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut XiaomiResourceSystem) -> R + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<XiaomiResourceSystem>(entity)
                .ok_or_else(|| anyhow_site!("Xiaomi resource system not found"))?;
            Ok(f(&mut system))
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn with_vivo_resource_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoResourceSystem) -> R + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoResourceSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo resource system not found"))?;
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
