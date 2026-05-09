use crate::{
    anyhow_site,
    device::{
        Device, DeviceKind,
        vivo::components::install::{
            InstallSystem as VivoInstallSystem, VivoQuickAppInstallRequest,
        },
    },
};

pub async fn install_vivo_quick_app_by_url(
    addr: String,
    req: VivoQuickAppInstallRequest,
) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Vivo => {
            let rx = with_vivo_install_system(addr, move |sys| sys.install_quick_app_by_url(req))
                .await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo quick-app install response not received"))??;
            Ok(())
        }
        DeviceKind::Xiaomi => {
            anyhow::bail!("Xiaomi quick-app URL install is not supported by this endpoint")
        }
    }
}

pub async fn stop_vivo_quick_app_install(
    addr: String,
    req: VivoQuickAppInstallRequest,
) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Vivo => {
            let rx =
                with_vivo_install_system(addr, move |sys| sys.stop_quick_app_install(req)).await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo quick-app stop response not received"))??;
            Ok(())
        }
        DeviceKind::Xiaomi => {
            anyhow::bail!("Xiaomi quick-app URL install is not supported by this endpoint")
        }
    }
}

pub async fn cancel_vivo_quick_app_install(
    addr: String,
    req: VivoQuickAppInstallRequest,
) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Vivo => {
            let rx = with_vivo_install_system(addr, move |sys| sys.cancel_quick_app_install(req))
                .await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo quick-app cancel response not received"))??;
            Ok(())
        }
        DeviceKind::Xiaomi => {
            anyhow::bail!("Xiaomi quick-app URL install is not supported by this endpoint")
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

async fn with_vivo_install_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoInstallSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoInstallSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo install system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}
