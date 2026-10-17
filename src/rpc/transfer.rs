//! 大块传输：安装、反向 MASS 接收（设备推文件给手机）、设备日志、资源包。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use pb::xiaomi::protocol;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Progress, emit, millis, read_file, with_component};
use crate::device::res_pack::ResumeToken;
use crate::device::xiaomi::components::install::InstallSystem;
use crate::device::xiaomi::components::mass::{MassSystem, ReceiveMassCallbackData, SendMassCallbackData};
use crate::device::xiaomi::components::report::ReportSystem;
use crate::device::xiaomi::packet::mass::MassDataType;
use crate::device::xiaomi::packet::v2::layer2::L2Channel;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallXiaomi {
    addr: String,
    res_type: MassDataType,
    /// 安装包路径（在 core 里读）。
    path: String,
    #[serde(default)]
    package_name: Option<String>,
    /// 改写表盘 ID（只对表盘有效）。
    #[serde(default)]
    watchface_id: Option<String>,
}

/// 小米设备经 MASS 安装资源；进度为 `SendMassCallbackData`。
pub async fn install_xiaomi(args: InstallXiaomi, progress: Option<Progress>) -> anyhow::Result<()> {
    let InstallXiaomi {
        addr,
        res_type,
        path,
        package_name,
        watchface_id,
    } = args;
    let data = read_file(&path).await?;
    let future = with_component::<InstallSystem, _, _>(&addr, "Install system", move |sys| {
        sys.send_install_request_with_progress(
            res_type,
            data,
            package_name.as_deref(),
            Arc::new(move |pg: SendMassCallbackData| emit(&progress, pg)),
            watchface_id.as_deref(),
        )
    })
    .await?;
    future.await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VivoInstallKind {
    Watchface,
    Firmware,
    QuickApp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallVivo {
    addr: String,
    kind: VivoInstallKind,
    path: String,
    /// 固件版本名。
    #[serde(default)]
    version_name: Option<String>,
    /// 固件在手表上的文件名。
    #[serde(default)]
    remote_file_name: Option<String>,
    /// 快应用 appId 覆盖。
    #[serde(default)]
    app_id: Option<String>,
}

/// vivo 设备本地安装（file_v2 推文件 + 业务命令）；进度为 `{ sent, total }`。
pub async fn install_vivo(args: InstallVivo, progress: Option<Progress>) -> anyhow::Result<()> {
    let data = read_file(&args.path).await?;
    let callback: Arc<dyn Fn(u64, u64) + Send + Sync> =
        Arc::new(move |sent, total| emit(&progress, json!({ "sent": sent, "total": total })));
    match args.kind {
        VivoInstallKind::Watchface => {
            crate::device::install::install_vivo_watchface_local(args.addr, data, None, Some(callback))
                .await
        }
        VivoInstallKind::Firmware => {
            crate::device::install::install_vivo_firmware_local(
                args.addr,
                data,
                args.version_name.unwrap_or_else(|| "unknown".to_string()),
                args.remote_file_name,
                true,
                Some(callback),
            )
            .await
        }
        VivoInstallKind::QuickApp => {
            crate::device::install::install_vivo_quick_app_local(
                args.addr,
                data,
                args.app_id,
                None,
                None,
                Some(callback),
            )
            .await
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseReceive {
    addr: String,
    /// 监听的 L2 通道（数值），缺省只听 MASS。
    #[serde(default = "default_channels")]
    channels: Vec<u8>,
    /// 收到的文件先落到这个目录里（文件名随机），由调用方再改名。
    save_dir: String,
    /// 从开始传输到收完的超时。
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn default_channels() -> Vec<u8> {
    vec![L2Channel::Mass as u8]
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseReceived {
    channel: u8,
    file_name: String,
    path: String,
    size: u64,
}

fn parse_channels(raw: &[u8]) -> anyhow::Result<Vec<L2Channel>> {
    raw.iter()
        .map(|value| L2Channel::try_from(*value).map_err(|_| anyhow!("invalid L2 channel {value}")))
        .collect()
}

async fn clear_reverse_waits(addr: &str, channels: Vec<L2Channel>) {
    let _ = with_component::<MassSystem, _, _>(addr, "Mass system", move |sys| {
        for channel in channels {
            sys.clear_reverse_mass_wait(channel);
        }
        Ok(())
    })
    .await;
}

/// 注册一次反向 MASS 接收并等它收完。
///
/// 进度消息依次是：`{"event":"ready"}`（已注册，调用方可以发起让设备推送的请求了），然后若干条
/// `{"event":"progress", channel, progress, totalParts, currentPartNum, fileName}`。
/// 调用方自己的超时 / 失败路径要调 `mass.clear_reverse_wait`。
pub async fn reverse_receive(
    args: ReverseReceive,
    progress: Option<Progress>,
) -> anyhow::Result<ReverseReceived> {
    let channels = parse_channels(&args.channels)?;
    let progress_for_cb = progress.clone();
    let channels_for_register = channels.clone();
    let rx = with_component::<MassSystem, _, _>(&args.addr, "Mass system", move |sys| {
        sys.begin_reverse_mass_receive_multi(
            &channels_for_register,
            Arc::new(move |data: ReceiveMassCallbackData| {
                emit(
                    &progress_for_cb,
                    json!({
                        "event": "progress",
                        "channel": data.channel,
                        "progress": data.progress,
                        "totalParts": data.total_parts,
                        "currentPartNum": data.current_part_num,
                        "fileName": data.file_name,
                    }),
                )
            }),
        )
    })
    .await?;
    emit(&progress, json!({ "event": "ready" }));

    let timeout = millis(args.timeout_ms, Duration::from_secs(300));
    let received = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(Ok(received))) => received,
        Ok(Ok(Err(err))) => {
            clear_reverse_waits(&args.addr, channels).await;
            return Err(err);
        }
        Ok(Err(_)) => {
            clear_reverse_waits(&args.addr, channels).await;
            return Err(anyhow!("reverse transfer ended unexpectedly"));
        }
        Err(_) => {
            clear_reverse_waits(&args.addr, channels).await;
            return Err(anyhow!("reverse transfer timed out"));
        }
    };

    let dir = PathBuf::from(&args.save_dir);
    let path = dir.join(format!("received-{:016x}.part", rand_suffix()));
    let size = received.data.len() as u64;
    let write_path = path.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        std::fs::write(&write_path, &received.data)
            .with_context(|| format!("failed to write {}", write_path.display()))
    })
    .await
    .map_err(|err| anyhow!("save task failed: {err}"))??;

    Ok(ReverseReceived {
        channel: received.channel as u8,
        file_name: received.file_name,
        path: path.to_string_lossy().into_owned(),
        size,
    })
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ (nanorand::Rng::generate::<u64>(&mut nanorand::WyRand::new()))
}

#[derive(Deserialize)]
pub struct ClearReverse {
    addr: String,
    #[serde(default = "default_channels")]
    channels: Vec<u8>,
}

pub async fn clear_reverse_wait(args: ClearReverse, _: Option<Progress>) -> anyhow::Result<()> {
    let channels = parse_channels(&args.channels)?;
    clear_reverse_waits(&args.addr, channels).await;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogExport {
    addr: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// 请求设备导出日志，返回设备的应答（`report_data::Result`）。设备之后会经反向 MASS 推送。
/// 没收到 / 超时会清掉等待者。
pub async fn device_log_export(
    args: LogExport,
    _: Option<Progress>,
) -> anyhow::Result<protocol::report_data::Result> {
    let rx = with_component::<ReportSystem, _, _>(&args.addr, "Report system", |sys| {
        Ok(sys.request_device_log_export())
    })
    .await?;
    let timeout = millis(args.timeout_ms, Duration::from_secs(300));
    let outcome = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => return result,
        Ok(Err(_)) => anyhow!("Device log export response not received"),
        Err(_) => anyhow!("Device log export request timed out"),
    };
    let _ = with_component::<ReportSystem, _, _>(&args.addr, "Report system", |sys| {
        sys.clear_device_log_wait();
        Ok(())
    })
    .await;
    Err(outcome)
}

#[derive(Deserialize)]
pub struct Addr {
    addr: String,
}

pub async fn clear_device_log_wait(args: Addr, _: Option<Progress>) -> anyhow::Result<()> {
    with_component::<ReportSystem, _, _>(&args.addr, "Report system", |sys| {
        sys.clear_device_log_wait();
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
pub struct ResPackInstall {
    addr: String,
    path: String,
    #[serde(default)]
    resume: Option<ResumeToken>,
}

/// 安装资源包；进度为 `ResourcePackProgress`。
pub async fn res_pack_install(args: ResPackInstall, progress: Option<Progress>) -> anyhow::Result<()> {
    crate::device::res_pack::install(args.addr, PathBuf::from(args.path), args.resume, move |p| {
        emit(&progress, p)
    })
    .await
}

pub async fn res_pack_list_installed(args: Addr, _: Option<Progress>) -> anyhow::Result<serde_json::Value> {
    let installed = crate::device::res_pack::list_installed(args.addr).await?;
    Ok(serde_json::to_value(installed)?)
}
