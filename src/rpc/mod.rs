//! 按名字调用 core 的能力。
//!
//! 逻辑层（app 的 `applogic`，可能是热更新下发的 WASM）不能像原生代码那样把闭包塞进 ECS
//! 线程去操作组件，所以 core 把对外能力登记成「方法名 → 处理函数」，参数与结果都是 JSON：
//!
//! ```text
//! call("watchface.set_current", {"addr": "...", "id": "..."}, None).await
//! ```
//!
//! 需要进度的方法收到一个 [`Progress`] 回调，每条进度也是 JSON。参数字段一律 camelCase。
//!
//! 新增方法：写一个 `async fn(args: 某个 Deserialize 类型, progress: Option<Progress>) ->
//! anyhow::Result<某个 Serialize 类型>`，在下面的 [`rpc_methods!`] 里登记一行。逻辑层随即可以经
//! 宿主能力 `core.call` 调用它，不需要写任何胶水。
//!
//! 大块数据（安装包、音乐、导出的日志）一律走文件路径，不经 JSON。

mod device;
mod media;
mod transfer;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use bevy_ecs::component::Component;
use serde_json::Value;
use tokio::sync::oneshot;

/// 进度回调：每次调用投递一条 JSON。
pub type Progress = Arc<dyn Fn(Value) + Send + Sync>;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type Handler = fn(Value, Option<Progress>) -> BoxFuture<anyhow::Result<Value>>;

macro_rules! rpc_methods {
    ( $( $name:literal => $f:path ; )* ) => {
        const METHODS: &[(&str, Handler)] = &[
            $( ($name, {
                fn entry(args: Value, progress: Option<Progress>) -> BoxFuture<anyhow::Result<Value>> {
                    Box::pin(async move {
                        let args = serde_json::from_value(args)
                            .map_err(|err| anyhow!("invalid arguments for `{}`: {err}", $name))?;
                        #[allow(clippy::let_unit_value)]
                        let output = $f(args, progress).await?;
                        Ok(serde_json::to_value(output)?)
                    })
                }
                entry as Handler
            }) ),*
        ];
    };
}

rpc_methods! {
    // 设备通用
    "device.kind" => device::kind;
    "data.request" => device::data_request;
    "file.type" => device::file_type;
    "resource.watchface_list" => device::watchface_list;
    "resource.quick_app_list" => device::quick_app_list;
    "watchface.set_current" => device::watchface_set_current;
    "watchface.uninstall" => device::watchface_uninstall;
    "watchface.resolve_id" => device::watchface_resolve_id;
    "watchface.support_data" => device::watchface_support_data;
    "watchface.edit" => device::watchface_edit;
    "watchface.transfer_image" => device::watchface_transfer_image;
    "watchface.transfer_font" => device::watchface_transfer_font;
    "vivo.dial_id" => device::vivo_dial_id;
    "thirdparty.send_message" => device::thirdparty_send_message;
    "thirdparty.launch" => device::thirdparty_launch;
    "thirdparty.uninstall" => device::thirdparty_uninstall;
    "vivo.quick_app.install_by_url" => device::vivo_quick_app_install_by_url;
    "vivo.quick_app.stop_install" => device::vivo_quick_app_stop_install;
    "vivo.quick_app.cancel_install" => device::vivo_quick_app_cancel_install;
    "sync.time" => device::sync_time;
    "sync.language" => device::sync_language;
    "network.speed" => device::network_speed;
    "transport.profiler_start" => device::profiler_start;
    "transport.profiler_status" => device::profiler_status;
    "transport.profiler_stop" => device::profiler_stop;
    "transport.profiler_record" => device::profiler_record;

    // 存储 / 媒体
    "info.storage" => media::storage;
    "media.song_summary" => media::song_summary;
    "media.song_page" => media::song_page;
    "media.songlist" => media::songlist;
    "media.remove_song" => media::remove_song;
    "media.upload_song" => media::upload_song;
    "media.file_list" => media::file_list;
    "media.request_files" => media::request_files;
    "media.confirm_file" => media::confirm_file;

    // 传输：安装、反向接收、日志、资源包
    "install.xiaomi" => transfer::install_xiaomi;
    "install.vivo" => transfer::install_vivo;
    "mass.reverse_receive" => transfer::reverse_receive;
    "mass.clear_reverse_wait" => transfer::clear_reverse_wait;
    "report.device_log_export" => transfer::device_log_export;
    "report.clear_device_log_wait" => transfer::clear_device_log_wait;
    "res_pack.install" => transfer::res_pack_install;
    "res_pack.list_installed" => transfer::res_pack_list_installed;
}

/// 全部方法名。
pub fn methods() -> Vec<&'static str> {
    METHODS.iter().map(|(name, _)| *name).collect()
}

/// 调用一个方法。
pub async fn call(method: &str, args: Value, progress: Option<Progress>) -> anyhow::Result<Value> {
    let handler = METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, handler)| *handler)
        .ok_or_else(|| anyhow!("unknown core method `{method}`"))?;
    handler(args, progress).await
}

// ---- 公共小工具 -------------------------------------------------------------

/// 在 ECS 线程里对设备上的某个组件执行一次操作。
pub(crate) async fn with_component<C, R, F>(addr: &str, what: &'static str, f: F) -> anyhow::Result<R>
where
    C: Component,
    F: FnOnce(&mut C) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    let addr = addr.to_string();
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&addr, |world, entity| {
            let mut component = world
                .get_mut::<C>(entity)
                .ok_or_else(|| anyhow!("{what} not found"))?;
            f(&mut component)
        })
        .ok_or_else(|| anyhow!("Device not found"))?
    })
    .await
}

/// 等一个组件回包；超时报 `"{what} timed out"`。
pub(crate) async fn await_reply<T>(
    rx: oneshot::Receiver<anyhow::Result<T>>,
    timeout: Duration,
    what: &str,
) -> anyhow::Result<T> {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(anyhow!("{what} response not received")),
        Err(_) => Err(anyhow!("{what} timed out")),
    }
}

/// 毫秒参数（缺省用 `default`）。
pub(crate) fn millis(value: Option<u64>, default: Duration) -> Duration {
    value.map(Duration::from_millis).unwrap_or(default)
}

/// 读本地文件（大文件在 core 里读，不经过逻辑层）。
pub(crate) async fn read_file(path: &str) -> anyhow::Result<Vec<u8>> {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        std::fs::read(&path).with_context(|| format!("failed to read {path}"))
    })
    .await
    .map_err(|err| anyhow!("file read task failed: {err}"))?
}

/// 往进度回调里投递一个可序列化的值。
pub(crate) fn emit(progress: &Option<Progress>, value: impl serde::Serialize) {
    if let Some(progress) = progress {
        if let Ok(value) = serde_json::to_value(value) {
            progress(value);
        }
    }
}
