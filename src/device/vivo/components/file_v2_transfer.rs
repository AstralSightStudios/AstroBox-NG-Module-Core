// Vivo FileV2 PhoneToWatch transfer driver.
//
// 这是 Vivo 文件传输 V2 协议的发送端实现，对应 Java 端
// `FtSendTaskV3` / `FtSendTaskProtocolV2`。它把一个内存中的字节流通过 BID 62
// (BLE) / 64 (BT) 推到手表上：先发 SetUpRequestV2 拿到 watch 的 offset+resp_pack_num，
// 再循环发 SendRequestV2 (按 resp_pack_num 节奏要求 ACK)，最后发 EndRequestV2。
//
// 注意：目前还没有真机调试过，阈值 (chunk size, ACK 周期, timeout) 全部按 jadx 里
// 默认值填的。如果手表 reject，最常见的原因是 chunk size 与 SAR/MTU 的关系。
//
// 调试时需要重点关注：
//   * `FtRespCountManager.getBleRespCountV2() = 240`，BT 默认 240。
//   * `MtuManager.getMtuBt()` 在 jadx 里取的是协商后的 max_data_length / pack_size
//     的子集，这里我们直接取 VivoDeviceConfig.vscp.max_biz_payload_len，应该接近。
//   * SendResponseV2.offset 在 SPP 上是 i64，BLE 上是 i32 — 我们一律按 i64 解码，
//     msgpack reader 会自动 widen。
use std::{
    convert::TryFrom,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use tokio::{runtime::Handle, sync::oneshot, task};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        VivoConnectType,
        components::shared::{HasVivoRequestContext, RequestSlot},
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::{
            file_v2::{
                CID_END, CID_END_RESPONSE, CID_SEND, CID_SEND_RESPONSE, CID_SETUP,
                CID_SETUP_RESPONSE, EndAction, EndRequestV2, EndResponseV2,
                FileTransferProgressResponse, FileV2Channel, FileV2Direction, SendRequestV2,
                SetUpRequestV2, business_id,
            },
            vscp::VscpMessage,
        },
    },
    ecs::{Component, access::with_device_component_mut},
};

/// 默认的 ifNeedRsp 周期，与 Java 端 `FtRespCountManager.getBleRespCountV2()` 对齐。
const DEFAULT_RESP_PACK_NUM_BLE: u32 = 240;
const DEFAULT_RESP_PACK_NUM_BT: u32 = 240;

/// 在没拿到 SetUpResponse 之前默认每个 chunk 的字节上限。
/// 真实大小由 VivoDeviceConfig.vscp.max_biz_payload_len 决定，初始化时会覆盖。
const FALLBACK_CHUNK_SIZE: usize = 990;

/// 一次最大发送量上限（BT 通常 1MB/s 以下，10MB 下载 ≈ 30s 卡 ACK 是正常的）。
const MAX_FILE_SIZE: usize = 64 * 1024 * 1024;

/// 一次 transfer 的入参。
#[derive(Debug, Clone)]
pub struct FileV2SendParams {
    /// fileId 字符串。Java 端使用 crc32 文件内容的 8 字符 hex。
    pub file_id: String,
    /// 手表端期望落盘的目录（含尾部 `/`）。
    pub file_path: String,
    /// 手表端期望落盘的文件名（含扩展名）。
    pub file_name: String,
    /// 业务类型（仅用于日志），例如 "TYPE_DIAL" / "TYPE_OTA" / "TYPE_OTHER"。
    pub business_label: &'static str,
    /// 给 SetUpRequestV2.extra 的字节，可选。
    pub extra: Option<Vec<u8>>,
    /// 整个 transfer 的超时（毫秒），用于发往 watch 的 SetUp/Send/End 超时字段。
    pub setup_timeout_ms: i32,
}

#[derive(Debug, Clone)]
pub struct FileV2SendProgress {
    pub bytes_sent: u64,
    pub bytes_total: u64,
}

pub type ProgressCb = Arc<dyn Fn(FileV2SendProgress) + Send + Sync>;

/// 一个驻留在设备上的「FileV2 transfer 路由器」组件：
/// - 收 BID 62/63/64/65 的入站消息，按 file_id 把响应分发给对应 oneshot
/// - 不直接驱动循环，循环在 `send_file_v2` 这个 free async fn 里
#[derive(Component)]
pub struct FileV2TransferSystem {
    owner_id: String,
    tk_handle: Handle,
    inflight: Arc<Mutex<Option<InflightTransfer>>>,
    /// 标记是否有用户主动 cancel。
    canceled: Arc<AtomicBool>,
    /// 兜底用，万一上层没等就 drop receiver，至少不会泄漏。
    _slot: RequestSlot<()>,
}

struct InflightTransfer {
    file_id: String,
    setup_ack: Option<oneshot::Sender<anyhow::Result<FileTransferProgressResponse>>>,
    send_ack: Option<oneshot::Sender<anyhow::Result<FileTransferProgressResponse>>>,
    end_ack: Option<oneshot::Sender<anyhow::Result<EndResponseV2>>>,
}

impl FileV2TransferSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            inflight: Arc::new(Mutex::new(None)),
            canceled: Arc::new(AtomicBool::new(false)),
            _slot: RequestSlot::new(),
        }
    }

    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
        let mut guard = self.inflight.lock();
        if let Some(state) = guard.as_mut() {
            if let Some(tx) = state.setup_ack.take() {
                let _ = tx.send(Err(anyhow_site!(
                    "vivo file_v2 transfer canceled before SetUp ack"
                )));
            }
            if let Some(tx) = state.send_ack.take() {
                let _ = tx.send(Err(anyhow_site!("vivo file_v2 transfer canceled mid-send")));
            }
            if let Some(tx) = state.end_ack.take() {
                let _ = tx.send(Err(anyhow_site!(
                    "vivo file_v2 transfer canceled before End ack"
                )));
            }
        }
        *guard = None;
    }

    fn install_setup_ack(
        &self,
        file_id: &str,
    ) -> oneshot::Receiver<anyhow::Result<FileTransferProgressResponse>> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.inflight.lock();
        // 强制覆盖之前的 inflight，避免悬空 oneshot。
        *guard = Some(InflightTransfer {
            file_id: file_id.to_string(),
            setup_ack: Some(tx),
            send_ack: None,
            end_ack: None,
        });
        self.canceled.store(false, Ordering::SeqCst);
        rx
    }

    fn install_send_ack(
        &self,
        file_id: &str,
    ) -> oneshot::Receiver<anyhow::Result<FileTransferProgressResponse>> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.inflight.lock();
        if let Some(state) = guard.as_mut() {
            if state.file_id == file_id {
                if let Some(prev) = state.send_ack.replace(tx) {
                    let _ = prev.send(Err(anyhow_site!(
                        "vivo file_v2 send ack waiter replaced before resolve"
                    )));
                }
                return rx;
            }
        }
        // 没 inflight 就直接报错，让上层短路
        let _ = tx.send(Err(anyhow_site!(
            "vivo file_v2 send ack requested but no inflight transfer matches file_id={}",
            file_id
        )));
        rx
    }

    fn install_end_ack(&self, file_id: &str) -> oneshot::Receiver<anyhow::Result<EndResponseV2>> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.inflight.lock();
        if let Some(state) = guard.as_mut() {
            if state.file_id == file_id {
                if let Some(prev) = state.end_ack.replace(tx) {
                    let _ = prev.send(Err(anyhow_site!(
                        "vivo file_v2 end ack waiter replaced before resolve"
                    )));
                }
                return rx;
            }
        }
        let _ = tx.send(Err(anyhow_site!(
            "vivo file_v2 end ack requested but no inflight transfer matches file_id={}",
            file_id
        )));
        rx
    }

    fn finish_inflight(&self) {
        let mut guard = self.inflight.lock();
        *guard = None;
    }

    fn handle_setup_response(&mut self, message: &VscpMessage) {
        let parsed = match FileTransferProgressResponse::decode(&message.payload) {
            Ok(p) => p,
            Err(err) => {
                log::warn!(
                    "[VivoDevice.FileV2] failed to decode SetUpResponseV2: {err:?} payload_hex={}",
                    crate::tools::to_hex_string(&message.payload)
                );
                return;
            }
        };

        log::info!(
            "[VivoDevice.FileV2] SetUpResponseV2 file_id={} code={} offset={} crc={} resp_pack={} payload_hex={}",
            parsed.file_id,
            parsed.code,
            parsed.offset,
            parsed.file_accumulate_crc,
            parsed.resp_pack_num,
            crate::tools::to_hex_string(&message.payload)
        );

        let mut guard = self.inflight.lock();
        if let Some(state) = guard.as_mut() {
            if state.file_id == parsed.file_id {
                if let Some(tx) = state.setup_ack.take() {
                    let _ = tx.send(Ok(parsed));
                }
            }
        }
    }

    fn handle_send_response(&mut self, message: &VscpMessage) {
        let parsed = match FileTransferProgressResponse::decode(&message.payload) {
            Ok(p) => p,
            Err(err) => {
                log::warn!("[VivoDevice.FileV2] failed to decode SendResponseV2: {err:?}");
                return;
            }
        };

        log::debug!(
            "[VivoDevice.FileV2] SendResponseV2 file_id={} code={} offset={} crc={} resp_pack={}",
            parsed.file_id,
            parsed.code,
            parsed.offset,
            parsed.file_accumulate_crc,
            parsed.resp_pack_num
        );

        let mut guard = self.inflight.lock();
        if let Some(state) = guard.as_mut() {
            if state.file_id == parsed.file_id {
                if let Some(tx) = state.send_ack.take() {
                    let _ = tx.send(Ok(parsed));
                }
            }
        }
    }

    fn handle_end_response(&mut self, message: &VscpMessage) {
        let parsed = match EndResponseV2::decode(&message.payload) {
            Ok(p) => p,
            Err(err) => {
                log::warn!("[VivoDevice.FileV2] failed to decode EndResponseV2: {err:?}");
                return;
            }
        };

        log::info!(
            "[VivoDevice.FileV2] EndResponseV2 file_id={} code={}",
            parsed.file_id,
            parsed.code
        );

        let mut guard = self.inflight.lock();
        if let Some(state) = guard.as_mut() {
            if state.file_id == parsed.file_id {
                if let Some(tx) = state.end_ack.take() {
                    let _ = tx.send(Ok(parsed));
                }
            }
        }
    }
}

impl HasVivoRequestContext for FileV2TransferSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for FileV2TransferSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        // 我们关心 PhoneToWatch 方向的响应：BID 62 (BLE) / 64 (BT)。
        // 反向 BID 63 / 65 是 watch→phone 上传，目前不处理。
        if message.bid != 62 && message.bid != 64 {
            return;
        }
        match message.cid {
            CID_SETUP_RESPONSE => self.handle_setup_response(message),
            CID_SEND_RESPONSE => self.handle_send_response(message),
            CID_END_RESPONSE => self.handle_end_response(message),
            _ => {}
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct FileV2TransferComponent {
    pub last_file_id: Option<String>,
}

impl FileV2TransferComponent {
    pub fn new() -> Self {
        Self { last_file_id: None }
    }
}

/// 计算给定字节流的 fileId（CRC32 hex 8 字符），与 Java `FileIdManager.getFileIdV2`
/// 对齐。
pub fn compute_file_id_v2(bytes: &[u8]) -> String {
    let mut crc: u32 = 0xffff_ffff;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    let crc = !crc;
    format!("{:08x}", crc)
}

/// 真正驱动一次 PhoneToWatch transfer 的入口。
/// 必须在 Tokio 运行时里调用。
pub async fn send_file_v2(
    device_addr: String,
    params: FileV2SendParams,
    payload: Vec<u8>,
    progress_cb: Option<ProgressCb>,
) -> anyhow::Result<()> {
    if payload.is_empty() {
        bail_site!("vivo file_v2: payload is empty");
    }
    if payload.len() > MAX_FILE_SIZE {
        bail_site!(
            "vivo file_v2: payload too large ({} bytes, max {})",
            payload.len(),
            MAX_FILE_SIZE
        );
    }

    let total_size = payload.len();
    let total_size_i32 = i32::try_from(total_size)
        .map_err(|_| anyhow_site!("vivo file_v2: file size overflows i32: {}", total_size))?;

    let (channel, chunk_size) = with_device_component_mut::<
        crate::device::vivo::VivoDevice,
        _,
        _,
    >(device_addr.clone(), |dev| {
        let chan = match dev.connect_type {
            VivoConnectType::BLE => FileV2Channel::BLE,
            VivoConnectType::SPP => FileV2Channel::BT,
        };
        // 关键：用 sar 里被 BindInitResponse 更新过的活动 config，而不是 dev.config.vscp
        // —— 后者一直停留在初始默认值 (247/247)，会让 chunk 缩到 ~171 字节，
        // 用满 BLE MTU 时就没法跑实际带宽。
        let active = {
            let sar = dev.sar.lock();
            sar.config().clone()
        };
        let chunk = active
            .max_biz_payload_len()
            .saturating_sub(64) // 留点头给 SendRequestV2 的 msgpack 字段
            .max(64)
            .min(FALLBACK_CHUNK_SIZE);
        log::info!(
            "[VivoDevice.FileV2] effective sar pack_size={} max_data_length={} biz_payload_max={} chunk_size={}",
            active.pack_size,
            active.max_data_length,
            active.max_biz_payload_len(),
            chunk
        );
        (chan, chunk)
    })
    .map_err(|err| anyhow_site!("vivo file_v2: failed to read device transport: {err:?}"))?;

    let bid = business_id(FileV2Direction::PhoneToWatch, channel);
    let initial_resp_pack = match channel {
        FileV2Channel::BLE => DEFAULT_RESP_PACK_NUM_BLE,
        FileV2Channel::BT => DEFAULT_RESP_PACK_NUM_BT,
    };

    log::info!(
        "[VivoDevice.FileV2] start send addr={} business={} file_id={} size={} chunk={} bid={} channel={:?}",
        device_addr,
        params.business_label,
        params.file_id,
        total_size,
        chunk_size,
        bid,
        channel
    );

    // ---- 1. SetUp ----
    let setup_rx = with_device_component_mut::<FileV2TransferSystem, _, _>(device_addr.clone(), {
        let file_id = params.file_id.clone();
        move |sys| sys.install_setup_ack(&file_id)
    })
    .map_err(|err| anyhow_site!("vivo file_v2: install setup ack failed: {err:?}"))?;

    let setup_payload = SetUpRequestV2 {
        file_id: params.file_id.clone(),
        file_path: params.file_path.clone(),
        file_name: params.file_name.clone(),
        file_size: total_size_i32,
        check_type: 1,
        n_bytes_write_tmp_file: chunk_size as i32,
        pack_write_status_file: initial_resp_pack as i32,
        timeout: params.setup_timeout_ms,
        extra: params.extra.clone(),
    }
    .payload()
    .map_err(|err| anyhow_site!("vivo file_v2: encode SetUpRequestV2 failed: {err:?}"))?;

    send_one(
        &device_addr,
        VscpMessage::new(bid, CID_SETUP, setup_payload),
    )
    .await?;
    let setup_resp = match setup_rx.await {
        Ok(Ok(p)) => p,
        Ok(Err(err)) => return Err(anyhow_site!("vivo file_v2: setup ack error: {err:#}")),
        Err(_) => bail_site!("vivo file_v2: setup ack receiver dropped"),
    };
    setup_resp
        .ensure_success("vivo file_v2 SetUpResponseV2")
        .map_err(|err| anyhow_site!("vivo file_v2: SetUp rejected: {err:#}"))?;

    let mut offset = u64::try_from(setup_resp.offset.max(0)).map_err(|_| {
        anyhow_site!(
            "vivo file_v2: setup offset out of range: {}",
            setup_resp.offset
        )
    })?;
    let mut resp_pack_num = if setup_resp.resp_pack_num > 0 {
        setup_resp.resp_pack_num as u32
    } else {
        initial_resp_pack
    };

    if offset > total_size as u64 {
        bail_site!(
            "vivo file_v2: setup offset {} exceeds file size {}",
            offset,
            total_size
        );
    }

    if let Some(cb) = progress_cb.as_ref() {
        cb(FileV2SendProgress {
            bytes_sent: offset,
            bytes_total: total_size as u64,
        });
    }

    // ---- 2. Send loop ----
    let mut chunk_idx: u32 = 0;
    while offset < total_size as u64 {
        let remaining = (total_size as u64 - offset) as usize;
        let chunk_len = remaining.min(chunk_size);
        let is_last = chunk_len == remaining;
        chunk_idx = chunk_idx.saturating_add(1);
        // 与 Java FtSendTaskV3 P0 一致：每发 resp_pack_num 个就要 ack 一次；
        // 最后一个 chunk 也强制 ack。
        let need_ack = is_last || (resp_pack_num > 0 && chunk_idx % resp_pack_num == 0);

        let send_rx = if need_ack {
            Some(
                with_device_component_mut::<FileV2TransferSystem, _, _>(device_addr.clone(), {
                    let file_id = params.file_id.clone();
                    move |sys| sys.install_send_ack(&file_id)
                })
                .map_err(|err| anyhow_site!("vivo file_v2: install send ack failed: {err:?}"))?,
            )
        } else {
            None
        };

        let chunk_start = offset as usize;
        let chunk_end = chunk_start + chunk_len;
        let chunk_data = &payload[chunk_start..chunk_end];
        let send_payload = SendRequestV2 {
            file_id: &params.file_id,
            offset: offset as i64,
            if_need_rsp: need_ack,
            data: chunk_data,
        }
        .payload()
        .map_err(|err| anyhow_site!("vivo file_v2: encode SendRequestV2 failed: {err:?}"))?;

        send_one(&device_addr, VscpMessage::new(bid, CID_SEND, send_payload)).await?;

        if let Some(rx) = send_rx {
            match rx.await {
                Ok(Ok(progress)) => {
                    progress
                        .ensure_success("vivo file_v2 SendResponseV2")
                        .map_err(|err| anyhow_site!("vivo file_v2: send rejected: {err:#}"))?;
                    let new_offset = u64::try_from(progress.offset.max(0)).map_err(|_| {
                        anyhow_site!(
                            "vivo file_v2: send progress offset out of range: {}",
                            progress.offset
                        )
                    })?;
                    if new_offset < offset + chunk_len as u64 {
                        log::warn!(
                            "[VivoDevice.FileV2] watch returned smaller offset {} (expected >= {}); resuming from watch offset",
                            new_offset,
                            offset + chunk_len as u64
                        );
                    }
                    offset = new_offset;
                    if progress.resp_pack_num > 0 {
                        resp_pack_num = progress.resp_pack_num as u32;
                    }
                    if let Some(cb) = progress_cb.as_ref() {
                        cb(FileV2SendProgress {
                            bytes_sent: offset,
                            bytes_total: total_size as u64,
                        });
                    }
                }
                Ok(Err(err)) => return Err(anyhow_site!("vivo file_v2: send ack error: {err:#}")),
                Err(_) => bail_site!("vivo file_v2: send ack receiver dropped"),
            }
        } else {
            offset += chunk_len as u64;
            // 不阻塞 ack 时的进度回调用 fire-and-forget 频率，加点节流避免淹没
            if chunk_idx % 16 == 0 {
                if let Some(cb) = progress_cb.as_ref() {
                    cb(FileV2SendProgress {
                        bytes_sent: offset,
                        bytes_total: total_size as u64,
                    });
                }
            }
        }
    }

    // ---- 3. End ----
    let end_rx = with_device_component_mut::<FileV2TransferSystem, _, _>(device_addr.clone(), {
        let file_id = params.file_id.clone();
        move |sys| sys.install_end_ack(&file_id)
    })
    .map_err(|err| anyhow_site!("vivo file_v2: install end ack failed: {err:?}"))?;

    let end_payload = EndRequestV2 {
        file_id: params.file_id.clone(),
        result_code: 0,
        action: EndAction::Default,
    }
    .payload()
    .map_err(|err| anyhow_site!("vivo file_v2: encode EndRequestV2 failed: {err:?}"))?;

    send_one(&device_addr, VscpMessage::new(bid, CID_END, end_payload)).await?;
    let end_resp = match end_rx.await {
        Ok(Ok(p)) => p,
        Ok(Err(err)) => return Err(anyhow_site!("vivo file_v2: end ack error: {err:#}")),
        Err(_) => bail_site!("vivo file_v2: end ack receiver dropped"),
    };
    if end_resp.code != 0 {
        bail_site!(
            "vivo file_v2: EndResponseV2 rejected code={}",
            end_resp.code
        );
    }

    let _ = with_device_component_mut::<FileV2TransferSystem, _, _>(device_addr.clone(), |sys| {
        sys.finish_inflight()
    });
    let _ = with_device_component_mut::<FileV2TransferComponent, _, _>(device_addr.clone(), {
        let file_id = params.file_id.clone();
        move |comp| comp.last_file_id = Some(file_id)
    });

    if let Some(cb) = progress_cb.as_ref() {
        cb(FileV2SendProgress {
            bytes_sent: total_size as u64,
            bytes_total: total_size as u64,
        });
    }

    log::info!(
        "[VivoDevice.FileV2] transfer complete addr={} file_id={} size={}",
        device_addr,
        params.file_id,
        total_size
    );

    Ok(())
}

/// 取出设备的 send_fn 把单条 VSCP 消息发出去。
async fn send_one(device_addr: &str, message: VscpMessage) -> anyhow::Result<()> {
    let send_parts = with_device_component_mut::<crate::device::vivo::VivoDevice, _, _>(
        device_addr.to_string(),
        move |dev| dev.transport_send_parts(message),
    )
    .map_err(|err| anyhow_site!("vivo file_v2: prepare send failed: {err:?}"))?;
    let (sender, packets) =
        send_parts.map_err(|err| anyhow_site!("vivo file_v2: encode send failed: {err:?}"))?;
    // 防止 caller 在 send_one 之前持有 ECS write 锁导致死锁，这里把 send 切到
    // tokio blocking 上跑（实际 send 是 async 的，但底层 BT send 可能阻塞）。
    let send_future = (sender)(packets);
    task::spawn(async move {
        send_future
            .await
            .map_err(|err| anyhow::Error::msg(format!("vivo file_v2: send_fn failed: {err:?}")))
    })
    .await
    .map_err(|err| anyhow_site!("vivo file_v2: send task join failed: {err:?}"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::compute_file_id_v2;

    #[test]
    fn file_id_matches_java_crc32_lower_hex_8() {
        // crc32("hello") = 0x3610a686
        assert_eq!(compute_file_id_v2(b"hello"), "3610a686");
        // crc32(""): we reject empty, but the algorithm itself returns 0x00000000
        // sanity check on a known vector
        assert_eq!(compute_file_id_v2(b"123456789"), "cbf43926");
    }
}
