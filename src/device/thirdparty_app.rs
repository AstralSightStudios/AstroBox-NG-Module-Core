use anyhow::bail;

use crate::{
    anyhow_site,
    device::{
        Device, DeviceKind,
        vivo::components::{
            cloud_bridge::CloudBridgeSystem as VivoCloudBridgeSystem,
            thirdparty_app::ThirdpartyAppSystem as VivoThirdpartyAppSystem,
        },
        xiaomi::components::{
            resource::ResourceComponent as XiaomiResourceComponent,
            thirdparty_app::{
                AppInfo as XiaomiAppInfo, ThirdpartyAppSystem as XiaomiThirdpartyAppSystem,
            },
        },
    },
};

pub async fn send_message(
    addr: String,
    package_name: String,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let info = xiaomi_app_info(&addr, &package_name).await?;
            with_xiaomi_thirdparty_app_system(addr, move |sys| {
                sys.send_phone_message(&info, payload);
                Ok(())
            })
            .await
        }
        DeviceKind::Vivo => {
            let msg = String::from_utf8(payload)
                .map_err(|err| anyhow_site!("vivo cloud message must be UTF-8: {err}"))?;
            let rx =
                with_vivo_cloud_bridge_system(addr, move |sys| sys.send_cloud_message(msg)).await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo cloud message send response not received"))??;
            Ok(())
        }
    }
}

pub async fn launch(addr: String, package_name: String, page: String) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let info = xiaomi_app_info(&addr, &package_name).await?;
            with_xiaomi_thirdparty_app_system(addr, move |sys| {
                sys.launch_app(&info, &page);
                Ok(())
            })
            .await
        }
        DeviceKind::Vivo => {
            bail!(
                "vivo quick-app launch command is not implemented; use BID 47 cloud bridge when available"
            )
        }
    }
}

pub async fn uninstall(addr: String, package_name: String) -> anyhow::Result<()> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let info = xiaomi_app_info(&addr, &package_name).await?;
            with_xiaomi_thirdparty_app_system(addr, move |sys| {
                sys.uninstall_app(&info);
                Ok(())
            })
            .await
        }
        DeviceKind::Vivo => {
            let rx =
                with_vivo_thirdparty_app_system(addr, move |sys| sys.uninstall_app(&package_name))
                    .await?;
            rx.await
                .map_err(|_| anyhow_site!("Vivo app uninstall response not received"))??;
            Ok(())
        }
    }
}

async fn xiaomi_app_info(addr: &str, package_name: &str) -> anyhow::Result<XiaomiAppInfo> {
    let addr_owned = addr.to_string();
    let package_name = package_name.to_string();
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr_owned, |world, entity| {
            let comp = world
                .get::<XiaomiResourceComponent>(entity)
                .ok_or_else(|| anyhow_site!("Xiaomi resource component not found"))?;
            comp.quick_apps
                .iter()
                .find(|item| item.package_name == package_name)
                .map(|item| XiaomiAppInfo {
                    package_name: item.package_name.clone(),
                    fingerprint: item.fingerprint.clone(),
                })
                .ok_or_else(|| anyhow_site!("AppInfo not found for {package_name}"))
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
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

async fn with_xiaomi_thirdparty_app_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut XiaomiThirdpartyAppSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<XiaomiThirdpartyAppSystem>(entity)
                .ok_or_else(|| anyhow_site!("Xiaomi thirdparty app system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn with_vivo_thirdparty_app_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoThirdpartyAppSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoThirdpartyAppSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo thirdparty app system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

async fn with_vivo_cloud_bridge_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoCloudBridgeSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoCloudBridgeSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo cloud bridge system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}
