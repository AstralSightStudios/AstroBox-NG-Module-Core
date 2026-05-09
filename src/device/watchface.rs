use crate::{
    anyhow_site,
    device::{
        Device, DeviceKind, vivo::components::watchface::WatchfaceSystem as VivoWatchfaceSystem,
        xiaomi::components::watchface::WatchfaceSystem as XiaomiWatchfaceSystem,
    },
};

pub async fn set_current(addr: String, watchface_id: String) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            with_xiaomi_watchface_system(addr, move |sys| {
                sys.set_watchface(&watchface_id);
                Ok(())
            })
            .await?
        }
        DeviceKind::Vivo => {
            let rx = with_vivo_watchface_system(addr, move |sys| sys.set_watchface(&watchface_id))
                .await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo set-current dial response not received"))??;
        }
    }
    Ok(())
}

pub async fn uninstall(addr: String, watchface_id: String) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            with_xiaomi_watchface_system(addr, move |sys| {
                sys.uninstall_watchface(&watchface_id);
                Ok(())
            })
            .await?
        }
        DeviceKind::Vivo => {
            let rx =
                with_vivo_watchface_system(addr, move |sys| sys.uninstall_watchface(&watchface_id))
                    .await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo uninstall dial response not received"))??;
        }
    }
    Ok(())
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

async fn with_xiaomi_watchface_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut XiaomiWatchfaceSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<XiaomiWatchfaceSystem>(entity)
                .ok_or_else(|| anyhow_site!("Xiaomi watchface system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn with_vivo_watchface_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoWatchfaceSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoWatchfaceSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo watchface system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}
