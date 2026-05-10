use crate::{
    anyhow_site, bail_site,
    device::{
        Device, DeviceKind,
        vivo::{
            components::{
                file_v2_transfer::{
                    FileV2SendParams, FileV2SendProgress, ProgressCb, compute_file_id_v2,
                    send_file_v2,
                },
                watchface::WatchfaceSystem as VivoWatchfaceSystem,
            },
            dial_manifest::parse_vivo_dial_manifest,
        },
        xiaomi::components::watchface::WatchfaceSystem as XiaomiWatchfaceSystem,
    },
};
use std::sync::Arc;

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

/// 把一个本地表盘 rpk/zip 装到手表上。
/// 仅 Vivo 设备调用 — Xiaomi 走 `device_install` 走 MASS。
///
/// `dial_id_override` 为 `Some` 时使用调用方提供的 dialId；为 `None` 时从 rpk
/// `manifest.json` 里抽。Vivo 表盘 install 命令必须带真实 dialId，传 0 会被静默
/// 丢弃（无任何回包）。
pub async fn install_local_zip_vivo(
    addr: String,
    zip_data: Vec<u8>,
    dial_id_override: Option<i64>,
    progress_cb: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
) -> anyhow::Result<()> {
    if zip_data.is_empty() {
        bail_site!("vivo watchface install: zip_data is empty");
    }
    let kind = device_kind(&addr).await?;
    if kind != DeviceKind::Vivo {
        bail_site!("install_local_zip_vivo can only be used with vivo devices");
    }

    let manifest = parse_vivo_dial_manifest(&zip_data).map_err(|err| {
        anyhow_site!(
            "vivo watchface install: failed to parse rpk manifest.json (need it to extract dialId): {err:#}"
        )
    })?;
    let dial_id = dial_id_override.unwrap_or(manifest.dial_id);
    if dial_id == 0 {
        bail_site!("vivo watchface install: dial_id resolved to 0; refusing to send");
    }
    log::info!(
        "[VivoDevice.Watchface] resolved dial manifest package={} version={} version_code={} dial_id={}",
        manifest.package,
        manifest.version_name,
        manifest.version_code,
        dial_id
    );

    let file_id = compute_file_id_v2(&zip_data);
    // jadx `DialFileUtils.a(dialInfo)` 把文件名拼成 `{dialId}_{version}.rpk`。
    // 我们没有云端 DialInfo.version，这里用 rpk manifest 的 versionCode 兜底。
    let dial_version_for_name = if manifest.version_code > 0 {
        manifest.version_code as i64
    } else {
        1
    };
    let file_name = format!("{dial_id}_{dial_version_for_name}.rpk");

    let bridge: Option<ProgressCb> = progress_cb.as_ref().map(|cb| {
        let cb = cb.clone();
        let mapped: Arc<dyn Fn(FileV2SendProgress) + Send + Sync> =
            Arc::new(move |p: FileV2SendProgress| cb(p.bytes_sent, p.bytes_total));
        mapped
    });

    // jadx: WatchPathUtils.getWatchPathByType(2 /* TYPE_DIAL */) → "/sdcard/watch/"
    let params = FileV2SendParams {
        file_id: file_id.clone(),
        file_path: "/sdcard/watch/".to_string(),
        file_name,
        business_label: "TYPE_DIAL",
        extra: None,
        setup_timeout_ms: 10_000,
    };
    send_file_v2(addr.clone(), params, zip_data, bridge).await?;

    // 关键：jadx `DialBleModule#lambda$installDialToWatch$0` 在 dialInfo.resId == null 时
    // 传 `""`，而不是 file_v2 的 CRC32 fileId。本地安装走这条路。
    let install_file_id_field = String::new();
    let rx = with_vivo_watchface_system(addr.clone(), move |sys| {
        sys.send_install_request(dial_id, install_file_id_field, false, false, String::new())
    })
    .await?;
    let install_result = rx
        .await
        .map_err(|_| anyhow_site!("Vivo dial install response not received"))??;
    log::info!(
        "[VivoDevice.Watchface] dial install result dial_id={} order={}",
        install_result.dial_id,
        install_result.order
    );
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
