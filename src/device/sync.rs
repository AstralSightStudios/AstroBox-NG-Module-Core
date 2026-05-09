use crate::{
    anyhow_site,
    device::{
        Device, DeviceKind, vivo::components::sync::SyncSystem as VivoSyncSystem,
        xiaomi::components::sync::SyncSystem as XiaomiSyncSystem,
    },
    models::sync::TimeSyncProps,
};

pub async fn sync_time(addr: String, props: TimeSyncProps) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            with_xiaomi_sync_system(addr, move |sys| {
                sys.sync_time(props);
                Ok(())
            })
            .await
        }
        DeviceKind::Vivo => with_vivo_sync_system(addr, move |sys| sys.sync_time(props)).await,
    }
}

pub async fn set_language(addr: String, locale: String) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            with_xiaomi_sync_system(addr, move |sys| {
                sys.set_language(locale);
                Ok(())
            })
            .await
        }
        DeviceKind::Vivo => with_vivo_sync_system(addr, move |sys| sys.set_language(locale)).await,
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

async fn with_xiaomi_sync_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut XiaomiSyncSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<XiaomiSyncSystem>(entity)
                .ok_or_else(|| anyhow_site!("Xiaomi sync system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn with_vivo_sync_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoSyncSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoSyncSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo sync system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}
