use std::sync::Arc;

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
                install::{InstallSystem as VivoInstallSystem, VivoQuickAppInstallRequest},
                ota::OtaSystem as VivoOtaSystem,
                thirdparty_app::ThirdpartyAppSystem as VivoThirdpartyAppSystem,
            },
            quickapp_manifest::parse_vivo_quick_app_manifest,
        },
    },
};

/// Vivo 本地表盘安装。`watchface` 模块里已经有同样的入口，这里只做转发，方便统一从
/// `crate::device::install::*` 调用。
pub use crate::device::watchface::install_local_zip_vivo as install_vivo_watchface_local;

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

/// 把本地 .pkg 固件推到 Vivo 手表，然后让手表立刻安装。
/// `version_name` 必须与 .pkg 内 metadata 一致，否则手表会拒绝；通常是 e.g. "1.0.5.6"。
///
/// **未完整实现**：jadx 里 OTA 推包流程其实有 3 步骤：
///   1. `OTABleHelper.clearBtChannel(...)` 把 BT 通道切到 OTA 子通道，并把
///      isAuto/isNow/versionName 这几个语义参数发给手表（这步我们目前 **没做**）
///   2. file_v2 push，`extra = [isAuto, isNow, versionLen, ...versionBytes]`
///   3. `OTAInstallRequest` 通知手表装上
///
/// 我们目前只做了 (2) + (3)，且 (2) 的 extra 还是空。手表如果需要 step 1 才肯接受
/// 文件，本流程会卡在 SetUpResponseV2 上。需要先收齐 SetUp 错码，再决定补哪些步骤。
pub async fn install_vivo_firmware_local(
    addr: String,
    pkg_data: Vec<u8>,
    version_name: String,
    install_now: bool,
    progress_cb: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
) -> anyhow::Result<()> {
    if pkg_data.is_empty() {
        bail_site!("vivo OTA install: pkg_data is empty");
    }
    if version_name.trim().is_empty() {
        bail_site!("vivo OTA install: version_name is empty");
    }
    if device_kind(&addr).await? != DeviceKind::Vivo {
        bail_site!("install_vivo_firmware_local can only be used with vivo devices");
    }

    let file_id = compute_file_id_v2(&pkg_data);

    let bridge: Option<ProgressCb> = progress_cb.as_ref().map(|cb| {
        let cb = cb.clone();
        let mapped: Arc<dyn Fn(FileV2SendProgress) + Send + Sync> =
            Arc::new(move |p: FileV2SendProgress| cb(p.bytes_sent, p.bytes_total));
        mapped
    });

    // jadx OTAFileSendRequest.o(): fileParam.w("/sdcard/" + str)；
    // version_type==1 走 /sdcard/，否则 /update/。WatchV3/GT2 默认按 /sdcard/ 试。
    // 切到 /update/ 需要 version_type 信息，目前没有这个字段，先用 /sdcard/。
    let file_name = format!("{version_name}.pkg");
    let extra = build_ota_setup_extra(
        &version_name,
        /*is_auto=*/ true,
        /*is_now=*/ install_now,
    );

    let params = FileV2SendParams {
        file_id: file_id.clone(),
        file_path: "/sdcard/".to_string(),
        file_name,
        business_label: "TYPE_OTA",
        extra: Some(extra),
        setup_timeout_ms: 15_000,
    };
    send_file_v2(addr.clone(), params, pkg_data, bridge).await?;

    let rx = with_vivo_ota_system(addr.clone(), move |sys| {
        Ok(sys.send_install(version_name, install_now))
    })
    .await?;
    rx.await
        .map_err(|_| anyhow_site!("Vivo OTAInstall response not received"))??;
    Ok(())
}

/// jadx OTAFileSendRequest.o() 把 [isAuto, isNow, versionLen, ...versionBytes] 拼成
/// SetUpRequestV2.extra；手表用它判断「这次 OTA 是后台还是即时」。
fn build_ota_setup_extra(version_name: &str, is_auto: bool, is_now: bool) -> Vec<u8> {
    let bytes = version_name.as_bytes();
    let len = u8::try_from(bytes.len().min(255)).unwrap_or(255);
    let mut out = Vec::with_capacity(3 + bytes.len());
    out.push(if is_auto { 1 } else { 2 });
    out.push(if is_now { 1 } else { 2 });
    out.push(len);
    out.extend_from_slice(&bytes[..len as usize]);
    out
}

/// 把本地 .rpk 快应用装到 Vivo 手表上。
///
/// 与 jadx `WAppBusinessMgr.i0()`（文件传输）→ `h0()`（本地文件安装）顺序对齐：
///
///   1. **先 file_v2 push** 把 rpk 推到 `/sdcard/apps/apparch/{appId}.rpk`
///   2. **再发 `BleAppInstallReq`** (BID 40 / CID 1)，payload 为
///      `(appId, fileName, fileId, appVerCode)`，告诉手表安装刚传完的本地文件。
///
/// WatchV3/GT2 支持的 `BleAppInstallV2Req` (BID 41 / CID 3) 是官方应用商店的 URL
/// 安装入口，手表会按 `appUrl` 自行下载；它不是本地已传文件的 commit 指令。给本地
/// rpk 填 `local://...` 虽然会收到 code=0，但手表没有可下载 URL，也不会消费
/// apparch 里的文件。
pub async fn install_vivo_quick_app_local(
    addr: String,
    rpk_data: Vec<u8>,
    app_id_override: Option<String>,
    file_name_override: Option<String>,
    version_code_override: Option<i32>,
    progress_cb: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
) -> anyhow::Result<()> {
    if rpk_data.is_empty() {
        bail_site!("vivo quick-app install: rpk_data is empty");
    }
    if device_kind(&addr).await? != DeviceKind::Vivo {
        bail_site!("install_vivo_quick_app_local can only be used with vivo devices");
    }

    // 从 rpk manifest.json 抽 appId / versionCode；override 覆盖（前端可显式传）。
    let manifest = parse_vivo_quick_app_manifest(&rpk_data).map_err(|err| {
        anyhow_site!(
            "vivo quick-app install: failed to parse rpk manifest.json (need appId): {err:#}"
        )
    })?;
    let app_id = app_id_override
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| manifest.package.clone());
    if app_id.trim().is_empty() {
        bail_site!("vivo quick-app install: app_id is empty after manifest fallback");
    }
    let version_code = version_code_override.unwrap_or(manifest.version_code);
    // jadx WAppDownloadHelper.d(): `wAppBean.getAppId() + ".rpk"`
    let file_name = file_name_override
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{app_id}.rpk"));

    let file_md5 = compute_file_md5_hex(&rpk_data);

    log::info!(
        "[VivoDevice.QuickApp] resolved manifest package={} name={} version={} version_code={} size={} md5={}",
        manifest.package,
        manifest.name,
        manifest.version_name,
        manifest.version_code,
        rpk_data.len(),
        file_md5
    );

    // ---- 1. file_v2 push 到 /sdcard/apps/apparch/{appId}.rpk ----
    let file_id = compute_file_id_v2(&rpk_data);
    let bridge: Option<ProgressCb> = progress_cb.as_ref().map(|cb| {
        let cb = cb.clone();
        let mapped: Arc<dyn Fn(FileV2SendProgress) + Send + Sync> =
            Arc::new(move |p: FileV2SendProgress| cb(p.bytes_sent, p.bytes_total));
        mapped
    });
    let params = FileV2SendParams {
        file_id: file_id.clone(),
        file_path: "/sdcard/apps/apparch/".to_string(),
        file_name: file_name.clone(),
        business_label: "TYPE_QUICKAPP_AUTO",
        extra: None,
        setup_timeout_ms: 10_000,
    };
    send_file_v2(addr.clone(), params, rpk_data, bridge).await?;

    log::info!(
        "[VivoDevice.QuickApp] file transfer complete addr={} app_id={} file_name={} file_id={}; sending V1 local install commit",
        addr,
        app_id,
        file_name,
        file_id
    );

    // ---- 2. BID 40 / CID 1 BleAppInstallReq：提交本地文件安装 ----
    let install_rx = with_vivo_thirdparty_app_system(addr.clone(), {
        let app_id = app_id.clone();
        let file_name = file_name.clone();
        let file_id = file_id.clone();
        move |sys| sys.send_v1_install(app_id, file_name, file_id, version_code)
    })
    .await?;
    install_rx
        .await
        .map_err(|_| anyhow_site!("Vivo BleAppInstallReq response not received"))??;

    log::info!(
        "[VivoDevice.QuickApp] local install commit accepted addr={} app_id={} file_name={} version_code={}",
        addr,
        app_id,
        file_name,
        version_code
    );
    Ok(())
}

/// 计算字节流的 MD5 hex（小写，32 字符），与 jadx `wAppBean.getFileMd5()` 在云端
/// 接到的格式对齐。
fn compute_file_md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", b);
    }
    out
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

async fn with_vivo_ota_system<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&mut VivoOtaSystem) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut system = world
                .get_mut::<VivoOtaSystem>(entity)
                .ok_or_else(|| anyhow_site!("Vivo OTA system not found"))?;
            f(&mut system)
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

#[allow(dead_code)]
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
