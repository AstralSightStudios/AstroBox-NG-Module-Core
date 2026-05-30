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
        xiaomi::components::{
            mass::{SendMassCallbackData, send_file_for_owner_with_known_slice_length},
            watchface::WatchfaceSystem as XiaomiWatchfaceSystem,
        },
        xiaomi::packet::mass::MassDataType,
    },
};
use pb::xiaomi::protocol;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditSlotItem {
    pub slot_id: String,
    pub widget_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchfaceEditParams {
    pub id: String,
    #[serde(default)]
    pub set_current: bool,
    #[serde(default)]
    pub background_color: Option<String>,
    #[serde(default)]
    pub foreground_color: Option<String>,
    #[serde(default)]
    pub style: String,
    #[serde(default)]
    pub style_color_index: Option<u32>,
    #[serde(default)]
    pub data_list: Vec<i32>,
    #[serde(default)]
    pub slot_item_list: Vec<EditSlotItem>,
    #[serde(default)]
    pub background_image: Option<String>,
    #[serde(default)]
    pub background_image_size: Option<u32>,
    #[serde(default)]
    pub background_image_list: Vec<String>,
    #[serde(default)]
    pub background_image_size_list: Vec<u32>,
    #[serde(default)]
    pub order_image_list: Vec<String>,
    #[serde(default)]
    pub delete_all_images: Option<bool>,
}

impl WatchfaceEditParams {
    fn into_edit_request(self) -> protocol::EditRequest {
        let foreground_color = self
            .foreground_color
            .as_deref()
            .and_then(parse_accent_color_bytes);
        let slot_item_list = self
            .slot_item_list
            .into_iter()
            .map(|item| protocol::watch_face_slot::Item {
                slot_id: item.slot_id,
                widget_id: item.widget_id,
            })
            .collect();
        protocol::EditRequest {
            id: self.id,
            set_current: self.set_current,
            background_color: self.background_color.unwrap_or_default(),
            background_image: self.background_image.unwrap_or_default(),
            background_image_size: self.background_image_size,
            style: self.style,
            data_list: self.data_list,
            background_image_list: self.background_image_list,
            background_image_size_list: self.background_image_size_list,
            order_image_list: self.order_image_list,
            delete_all_images: self.delete_all_images,
            slot_item_list,
            foreground_color,
            style_color_index: self.style_color_index,
            image_group_list: None,
            literal: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditResponseInfo {
    pub code: i32,
    pub can_accept_image_count: u32,
    pub expected_slice_length: u32,
}

impl From<protocol::EditResponse> for EditResponseInfo {
    fn from(resp: protocol::EditResponse) -> Self {
        Self {
            code: resp.code,
            can_accept_image_count: resp.can_accept_image_count.unwrap_or(0),
            expected_slice_length: resp.expected_slice_length.unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BgImageResultInfo {
    pub code: i32,
    pub id: String,
    pub background_image: String,
}

impl From<protocol::BgImageResult> for BgImageResultInfo {
    fn from(result: protocol::BgImageResult) -> Self {
        Self {
            code: result.code,
            id: result.id,
            background_image: result.background_image,
        }
    }
}

pub async fn edit_watchface(
    addr: String,
    params: WatchfaceEditParams,
) -> anyhow::Result<EditResponseInfo> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let request = params.into_edit_request();
            let rx =
                with_xiaomi_watchface_system(addr, move |sys| Ok(sys.request_edit(request))).await?;
            let resp = rx
                .await
                .map_err(|_| anyhow_site!("Xiaomi watchface edit response not received"))??;
            Ok(EditResponseInfo::from(resp))
        }
        DeviceKind::Vivo => bail_site!("watchface edit is only supported on Xiaomi devices"),
    }
}

pub async fn transfer_watchface_image(
    addr: String,
    encoded: Vec<u8>,
    slice_len: usize,
    progress_cb: Option<Arc<dyn Fn(SendMassCallbackData) + Send + Sync>>,
) -> anyhow::Result<BgImageResultInfo> {
    if device_kind(&addr).await? != DeviceKind::Xiaomi {
        bail_site!("watchface image transfer is only supported on Xiaomi devices");
    }
    if encoded.is_empty() {
        bail_site!("watchface image transfer: encoded image is empty");
    }
    let slice_len = if slice_len == 0 { 4096 } else { slice_len };

    let rx =
        with_xiaomi_watchface_system(addr.clone(), move |sys| Ok(sys.prepare_bg_image_wait()))
            .await?;

    let cb = move |d: SendMassCallbackData| {
        if let Some(cb) = progress_cb.as_ref() {
            cb(d);
        }
    };
    send_file_for_owner_with_known_slice_length(
        addr.clone(),
        encoded,
        MassDataType::WatchfaceImage,
        slice_len,
        cb,
    )
    .await?;

    let result = rx
        .await
        .map_err(|_| anyhow_site!("Xiaomi watchface bg image result not received"))??;
    Ok(BgImageResultInfo::from(result))
}

pub async fn resolve_xiaomi_watchface_id(
    addr: String,
    data: Vec<u8>,
) -> anyhow::Result<Option<String>> {
    use crate::device::xiaomi::{XiaomiDevice, resutils};
    let res_config = crate::ecs::with_rt_mut(move |rt| {
        rt.component_ref::<XiaomiDevice>(&addr)
            .map(|dev| dev.config.res.clone())
    })
    .await;
    Ok(res_config.and_then(|cfg| resutils::get_watchface_id(&data, &cfg)))
}

pub async fn get_watchface_support_data(addr: String) -> anyhow::Result<Vec<i32>> {
    match device_kind(&addr).await? {
        DeviceKind::Xiaomi => {
            let rx = with_xiaomi_watchface_system(addr, move |sys| Ok(sys.request_support_data()))
                .await?;
            rx.await
                .map_err(|_| anyhow_site!("Xiaomi watchface support data not received"))?
        }
        DeviceKind::Vivo => bail_site!("watchface support data is only supported on Xiaomi devices"),
    }
}

fn parse_accent_color_bytes(input: &str) -> Option<Vec<u8>> {
    let hex = input.trim().strip_prefix('#')?;
    let (r, g, b) = match hex.len() {
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        8 => (
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            u8::from_str_radix(&hex[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some(vec![0xFF, r, g, b])
}
