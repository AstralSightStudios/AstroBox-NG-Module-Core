//! 设备通用方法：设备类型、数据查询、表盘、三方应用、对时、网速、传输诊断。

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Progress, read_file};
use crate::device::data::DeviceDataType;
use crate::device::vivo::components::install::VivoQuickAppInstallRequest;
use crate::device::xiaomi::XiaomiDevice;
use crate::device::{Device, DeviceKind};
use crate::models::sync::TimeSyncProps;

#[derive(Deserialize)]
pub struct Addr {
    addr: String,
}

pub async fn kind(args: Addr, _: Option<Progress>) -> anyhow::Result<DeviceKind> {
    let addr = args.addr;
    crate::ecs::with_rt_mut(move |rt| {
        rt.component_ref::<Device>(&addr)
            .map(|device| device.kind())
            .ok_or_else(|| anyhow!("Device not found"))
    })
    .await
}

#[derive(Deserialize)]
pub struct DataRequest {
    addr: String,
    r#type: DeviceDataType,
}

pub async fn data_request(args: DataRequest, _: Option<Progress>) -> anyhow::Result<Value> {
    crate::device::data::request_device_data_json(args.addr, args.r#type).await
}

#[derive(Deserialize)]
pub struct PathArg {
    path: String,
}

/// 按内容识别文件类型（`FileType` 的数值）。
pub async fn file_type(args: PathArg, _: Option<Progress>) -> anyhow::Result<Value> {
    let data = read_file(&args.path).await.unwrap_or_default();
    let file_type = crate::device::xiaomi::resutils::get_file_type(&data);
    Ok(serde_json::to_value(file_type)?)
}

pub async fn watchface_list(args: Addr, _: Option<Progress>) -> anyhow::Result<Value> {
    crate::device::resource::request_watchface_list_json(args.addr).await
}

pub async fn quick_app_list(args: Addr, _: Option<Progress>) -> anyhow::Result<Value> {
    crate::device::resource::request_quick_app_list_json(args.addr).await
}

#[derive(Deserialize)]
pub struct WatchfaceId {
    addr: String,
    id: String,
}

pub async fn watchface_set_current(args: WatchfaceId, _: Option<Progress>) -> anyhow::Result<()> {
    crate::device::watchface::set_current(args.addr, args.id).await
}

pub async fn watchface_uninstall(args: WatchfaceId, _: Option<Progress>) -> anyhow::Result<()> {
    crate::device::watchface::uninstall(args.addr, args.id).await
}

#[derive(Deserialize)]
pub struct ResolveId {
    addr: String,
    /// 表盘 bin（mwz 包要先由调用方取出里面的 bin）。
    path: String,
}

/// 小米表盘包里的表盘 ID（取不到为 `null`）。
pub async fn watchface_resolve_id(
    args: ResolveId,
    _: Option<Progress>,
) -> anyhow::Result<Option<String>> {
    let data = read_file(&args.path).await?;
    crate::device::watchface::resolve_xiaomi_watchface_id(args.addr, data).await
}

pub async fn watchface_support_data(args: Addr, _: Option<Progress>) -> anyhow::Result<Vec<i32>> {
    crate::device::watchface::get_watchface_support_data(args.addr).await
}

#[derive(Deserialize)]
pub struct WatchfaceEdit {
    addr: String,
    params: crate::device::watchface::WatchfaceEditParams,
}

/// 发表盘编辑请求（`EditResponseInfo`）。
pub async fn watchface_edit(
    args: WatchfaceEdit,
    _: Option<Progress>,
) -> anyhow::Result<crate::device::watchface::EditResponseInfo> {
    crate::device::watchface::edit_watchface(args.addr, args.params).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchfaceTransfer {
    addr: String,
    /// 编码好的图片 / 字体文件路径。
    path: String,
    slice_length: usize,
}

fn mass_progress(
    progress: Option<Progress>,
) -> Option<std::sync::Arc<dyn Fn(crate::device::xiaomi::components::mass::SendMassCallbackData) + Send + Sync>>
{
    progress.map(|progress| {
        let callback: std::sync::Arc<
            dyn Fn(crate::device::xiaomi::components::mass::SendMassCallbackData) + Send + Sync,
        > = std::sync::Arc::new(move |data| {
            if let Ok(value) = serde_json::to_value(data) {
                progress(value);
            }
        });
        callback
    })
}

/// 传一张表盘编辑图片（`BgImageResultInfo`）；进度为 `SendMassCallbackData`。
pub async fn watchface_transfer_image(
    args: WatchfaceTransfer,
    progress: Option<Progress>,
) -> anyhow::Result<crate::device::watchface::BgImageResultInfo> {
    let data = read_file(&args.path).await?;
    crate::device::watchface::transfer_watchface_image(args.addr, data, args.slice_length, mass_progress(progress))
        .await
}

/// 传文字表盘的子集字体（`FontResultInfo`）；进度为 `SendMassCallbackData`。
pub async fn watchface_transfer_font(
    args: WatchfaceTransfer,
    progress: Option<Progress>,
) -> anyhow::Result<crate::device::watchface::FontResultInfo> {
    let data = read_file(&args.path).await?;
    crate::device::watchface::transfer_watchface_font(args.addr, data, args.slice_length, mass_progress(progress))
        .await
}

/// vivo 表盘 rpk 里 manifest 声明的表盘 ID。
pub async fn vivo_dial_id(args: PathArg, _: Option<Progress>) -> anyhow::Result<Option<String>> {
    let data = read_file(&args.path).await?;
    Ok(crate::device::vivo::dial_manifest::parse_vivo_dial_manifest(&data)
        .ok()
        .map(|manifest| manifest.dial_id.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThirdpartyMessage {
    addr: String,
    package_name: String,
    /// 消息文本（按 UTF-8 字节发送）。
    data: String,
}

pub async fn thirdparty_send_message(
    args: ThirdpartyMessage,
    _: Option<Progress>,
) -> anyhow::Result<()> {
    crate::device::thirdparty_app::send_message(args.addr, args.package_name, args.data.into_bytes())
        .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThirdpartyLaunch {
    addr: String,
    package_name: String,
    page: String,
}

pub async fn thirdparty_launch(args: ThirdpartyLaunch, _: Option<Progress>) -> anyhow::Result<()> {
    crate::device::thirdparty_app::launch(args.addr, args.package_name, args.page).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThirdpartyPackage {
    addr: String,
    package_name: String,
}

pub async fn thirdparty_uninstall(args: ThirdpartyPackage, _: Option<Progress>) -> anyhow::Result<()> {
    crate::device::thirdparty_app::uninstall(args.addr, args.package_name).await
}

#[derive(Deserialize)]
pub struct VivoQuickAppUrl {
    addr: String,
    request: VivoQuickAppInstallRequest,
}

pub async fn vivo_quick_app_install_by_url(
    args: VivoQuickAppUrl,
    _: Option<Progress>,
) -> anyhow::Result<()> {
    crate::device::install::install_vivo_quick_app_by_url(args.addr, args.request).await
}

pub async fn vivo_quick_app_stop_install(
    args: VivoQuickAppUrl,
    _: Option<Progress>,
) -> anyhow::Result<()> {
    crate::device::install::stop_vivo_quick_app_install(args.addr, args.request).await
}

pub async fn vivo_quick_app_cancel_install(
    args: VivoQuickAppUrl,
    _: Option<Progress>,
) -> anyhow::Result<()> {
    crate::device::install::cancel_vivo_quick_app_install(args.addr, args.request).await
}

#[derive(Deserialize)]
pub struct SyncTime {
    addr: String,
    props: TimeSyncProps,
}

pub async fn sync_time(args: SyncTime, _: Option<Progress>) -> anyhow::Result<()> {
    crate::device::sync::sync_time(args.addr, args.props).await
}

#[derive(Deserialize)]
pub struct SyncLanguage {
    addr: String,
    locale: String,
}

pub async fn sync_language(args: SyncLanguage, _: Option<Progress>) -> anyhow::Result<()> {
    crate::device::sync::set_language(args.addr, args.locale).await
}

#[derive(Serialize)]
pub struct NetworkSpeed {
    write: f64,
    read: f64,
}

pub async fn network_speed(args: Addr, _: Option<Progress>) -> anyhow::Result<NetworkSpeed> {
    use crate::device::vivo::components::erpc::ErpcSystem;
    use crate::device::xiaomi::components::network::NetworkSystem;

    let addr = args.addr;
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let kind = world
                .get::<Device>(entity)
                .map(|device| device.kind())
                .unwrap_or(DeviceKind::Xiaomi);
            match kind {
                DeviceKind::Xiaomi => {
                    let system = world
                        .get_mut::<NetworkSystem>(entity)
                        .ok_or_else(|| anyhow!("Network system not found"))?;
                    let speed = system.get_speed();
                    Ok(NetworkSpeed {
                        write: speed.write,
                        read: speed.read,
                    })
                }
                DeviceKind::Vivo => {
                    let system = world
                        .get_mut::<ErpcSystem>(entity)
                        .ok_or_else(|| anyhow!("Vivo ERPC system not found"))?;
                    let speed = system.get_speed();
                    Ok(NetworkSpeed {
                        write: speed.write,
                        read: speed.read,
                    })
                }
            }
        })
        .ok_or_else(|| anyhow!("Device not found"))?
    })
    .await
}

async fn with_xiaomi_device<F, R>(addr: String, f: F) -> anyhow::Result<R>
where
    F: FnOnce(&XiaomiDevice) -> R + Send + 'static,
    R: Send + 'static,
{
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            world
                .get::<XiaomiDevice>(entity)
                .map(f)
                .ok_or_else(|| anyhow!("Device not found"))
        })
        .ok_or_else(|| anyhow!("Device not found"))?
    })
    .await
}

pub async fn profiler_start(args: Addr, _: Option<Progress>) -> anyhow::Result<()> {
    with_xiaomi_device(args.addr, |dev| dev.transport_profiler.start()).await
}

pub async fn profiler_status(args: Addr, _: Option<Progress>) -> anyhow::Result<Value> {
    let status = with_xiaomi_device(args.addr, |dev| dev.transport_profiler.status()).await?;
    Ok(serde_json::to_value(status)?)
}

pub async fn profiler_stop(args: Addr, _: Option<Progress>) -> anyhow::Result<Value> {
    let report = with_xiaomi_device(args.addr, |dev| dev.transport_profiler.stop()).await?;
    Ok(serde_json::to_value(report)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilerRecord {
    addr: String,
    phase: String,
    label: String,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    success: Option<bool>,
}

pub async fn profiler_record(args: ProfilerRecord, _: Option<Progress>) -> anyhow::Result<()> {
    let ProfilerRecord {
        addr,
        phase,
        label,
        duration_ms,
        detail,
        success,
    } = args;
    with_xiaomi_device(addr, move |dev| {
        dev.transport_profiler
            .record(phase, label, duration_ms, None, None, None, success, detail)
    })
    .await
}
