//! 存储与媒体：存储空间、歌曲 / 歌单、录音等媒体文件。
//!
//! 等回包的方法都带超时（`timeoutMs`，缺省与原来的前端命令一致：12 秒），超时或没收到回包时
//! 清掉组件里对应的等待者，下一次请求才能重新发出。

use std::time::Duration;

use anyhow::anyhow;
use pb::xiaomi::protocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Progress, await_reply, emit, millis, read_file, with_component};
use crate::device::xiaomi::components::info::InfoSystem;
use crate::device::xiaomi::components::mass::SendMassCallbackData;
use crate::device::xiaomi::components::media::MediaSystem;

const MEDIA_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddrTimeout {
    addr: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub async fn storage(args: AddrTimeout, _: Option<Progress>) -> anyhow::Result<protocol::StorageInfo> {
    let rx = with_component::<InfoSystem, _, _>(&args.addr, "Info system", |sys| {
        Ok(sys.request_device_storage())
    })
    .await?;
    await_reply(rx, millis(args.timeout_ms, MEDIA_REQUEST_TIMEOUT), "Device storage").await
}

pub async fn song_summary(
    args: AddrTimeout,
    _: Option<Progress>,
) -> anyhow::Result<protocol::SongSummary> {
    let rx = with_component::<MediaSystem, _, _>(&args.addr, "Media system", |sys| {
        Ok(sys.request_song_summary())
    })
    .await?;
    await_reply(rx, millis(args.timeout_ms, MEDIA_REQUEST_TIMEOUT), "Song summary").await
}

/// 等媒体回包；没收到 / 超时时清掉对应等待者。
async fn await_media<T>(
    addr: &str,
    rx: tokio::sync::oneshot::Receiver<anyhow::Result<T>>,
    timeout: Duration,
    label: &str,
    clear: fn(&mut MediaSystem),
) -> anyhow::Result<T> {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            let _ = with_component::<MediaSystem, _, _>(addr, "Media system", move |sys| {
                clear(sys);
                Ok(())
            })
            .await;
            Err(anyhow!("{label} response not received"))
        }
        Err(_) => {
            let _ = with_component::<MediaSystem, _, _>(addr, "Media system", move |sys| {
                clear(sys);
                Ok(())
            })
            .await;
            Err(anyhow!("{label} request timed out"))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SongPage {
    addr: String,
    index: u32,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub async fn song_page(
    args: SongPage,
    _: Option<Progress>,
) -> anyhow::Result<protocol::song::GetResponse> {
    let index = args.index;
    let rx = with_component::<MediaSystem, _, _>(&args.addr, "Media system", move |sys| {
        sys.request_song_page(index)
    })
    .await?;
    await_media(
        &args.addr,
        rx,
        millis(args.timeout_ms, MEDIA_REQUEST_TIMEOUT),
        "Song page",
        MediaSystem::clear_song_page_wait,
    )
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Songlist {
    addr: String,
    /// `protocol::media::MediaId` 的数值。
    media_id: i32,
    request: protocol::songlist::Request,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// 歌单操作。不检查返回码，交给调用方判断。
pub async fn songlist(
    args: Songlist,
    _: Option<Progress>,
) -> anyhow::Result<protocol::songlist::Response> {
    let media_id = protocol::media::MediaId::try_from(args.media_id)
        .map_err(|_| anyhow!("unknown media id {}", args.media_id))?;
    let request = args.request;
    let rx = with_component::<MediaSystem, _, _>(&args.addr, "Media system", move |sys| {
        sys.request_songlist_operation(request, media_id)
    })
    .await?;
    await_media(
        &args.addr,
        rx,
        millis(args.timeout_ms, MEDIA_REQUEST_TIMEOUT),
        "Songlist",
        MediaSystem::clear_songlist_wait,
    )
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveSong {
    addr: String,
    /// 歌曲 ID（hex）。
    song_id: String,
}

pub async fn remove_song(
    args: RemoveSong,
    _: Option<Progress>,
) -> anyhow::Result<protocol::song::RemoveResponse> {
    let song_id = hex::decode(args.song_id.trim()).map_err(|err| anyhow!("invalid song id: {err}"))?;
    let rx = with_component::<MediaSystem, _, _>(&args.addr, "Media system", move |sys| {
        sys.request_remove_song(song_id)
    })
    .await?;
    match rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("Song remove response not received")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadSong {
    addr: String,
    song: protocol::Song,
    /// 音乐文件路径（在 core 里读，不经过逻辑层）。
    path: String,
}

#[derive(Serialize)]
pub struct UploadSongResult {
    song: protocol::Song,
    duplicated: bool,
}

/// 上传一首歌；进度为 `SendMassCallbackData`。
pub async fn upload_song(
    args: UploadSong,
    progress: Option<Progress>,
) -> anyhow::Result<UploadSongResult> {
    let bytes = read_file(&args.path).await?;
    let song = args.song;
    let future = with_component::<MediaSystem, _, _>(&args.addr, "Media system", move |sys| {
        sys.upload_song_with_progress(
            song,
            bytes,
            std::sync::Arc::new(move |data: SendMassCallbackData| emit(&progress, data)),
        )
    })
    .await?;
    let result = future.await?;
    Ok(UploadSongResult {
        song: result.song,
        duplicated: result.duplicated,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileList {
    addr: String,
    /// 用兼容模式的请求（老固件）。
    #[serde(default)]
    compat: bool,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// 媒体文件列表（录音等），`MediaFileDescriptor` 数组。
pub async fn file_list(args: FileList, _: Option<Progress>) -> anyhow::Result<Value> {
    let compat = args.compat;
    let rx = with_component::<MediaSystem, _, _>(&args.addr, "Media system", move |sys| {
        if compat {
            sys.request_media_file_list_compat()
        } else {
            sys.request_media_file_list()
        }
    })
    .await?;
    let label = if compat {
        "media file list compatibility"
    } else {
        "media file list"
    };
    let files = await_media(
        &args.addr,
        rx,
        millis(args.timeout_ms, MEDIA_REQUEST_TIMEOUT),
        label,
        MediaSystem::clear_media_file_list_wait,
    )
    .await?;
    Ok(serde_json::to_value(files)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestFiles {
    addr: String,
    identifiers: Vec<protocol::media_file::Identifier>,
    /// 逐个用单文件请求（`request_media_file`），而不是一次请求列表。
    #[serde(default)]
    single: bool,
}

/// 请求设备推送媒体文件（之后经反向 MASS 收取，见 `mass.reverse_receive`）。
pub async fn request_files(args: RequestFiles, _: Option<Progress>) -> anyhow::Result<()> {
    let RequestFiles {
        addr,
        identifiers,
        single,
    } = args;
    with_component::<MediaSystem, _, _>(&addr, "Media system", move |sys| {
        if single {
            for identifier in identifiers {
                sys.request_media_file(identifier);
            }
        } else {
            sys.request_media_files(identifiers);
        }
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
pub struct ConfirmFile {
    addr: String,
    identifier: protocol::media_file::Identifier,
}

pub async fn confirm_file(args: ConfirmFile, _: Option<Progress>) -> anyhow::Result<()> {
    let identifier = args.identifier;
    with_component::<MediaSystem, _, _>(&args.addr, "Media system", move |sys| {
        sys.confirm_media_file(identifier);
        Ok(())
    })
    .await
}
