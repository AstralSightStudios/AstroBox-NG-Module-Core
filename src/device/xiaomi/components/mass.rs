use crate::asyncrt::{Duration, timeout};
use crate::bail_site;
use anyhow::{Context, Result};
use byteorder::{LittleEndian, WriteBytesExt};
use pb::xiaomi::protocol;
use prost::Message;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::mem;
use std::sync::Arc;
use tokio::sync::oneshot;

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::device::xiaomi::XiaomiDevice;
use crate::device::xiaomi::config::MassConfig;
use crate::device::xiaomi::packet::{
    self,
    mass::{MassDataType, MassPacket, ReverseMassPacket},
    v2::layer2::{L2Channel, L2OpCode, L2Packet},
};
use crate::device::xiaomi::system::{XiaomiSystemExt, register_xiaomi_system_ext_on_l2packet};
use crate::ecs::{Component, access::with_device_component_mut};
use parking_lot::Mutex;
use crate::device::xiaomi::transport_profiler::TransportProfilerHandle;

#[derive(Clone, serde::Serialize)]
struct ResumeState {
    device_addr: String,
    mass_id: Vec<u8>,
    current_part: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMassCallbackData {
    pub progress: f32,
    pub total_parts: u16,
    pub current_part_num: u16,
    pub actual_data_payload_len: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceiveMassCallbackData {
    pub channel: u8,
    pub progress: f32,
    pub total_parts: u32,
    pub current_part_num: u32,
    pub file_name: String,
}

#[derive(Debug, Clone)]
pub struct ReverseMassReceiveResult {
    pub channel: L2Channel,
    pub file_name: String,
    pub data: Vec<u8>,
}

struct ReverseMassWaiter {
    packet: ReverseMassPacket,
    progress_cb: Arc<dyn Fn(ReceiveMassCallbackData) + Send + Sync>,
    /// 针对 reverse mass 的完成信号发送器
    /// 当多个通道共享同一个逻辑接收过程 (看不懂的猪请看begin_reverse_mass_receive_multi)，所有相关的等待方会共用一个被 mutex 包裹的发送器
    /// 这样可以确保只有第一个完成的通道会发送结果，其余通道的发送操作都no-op
    tx: Arc<parking_lot::Mutex<Option<oneshot::Sender<Result<ReverseMassReceiveResult>>>>>,
    siblings: Vec<u8>,
}

/// 记录已经等待确认的 MASS 分片，用于推进进度与续传。
/// 这里其实他妈的就是个“待确认队列”，发出去还没收到 ACK 的分片都塞进来，
/// 一边盯 ACK、一边按顺序出队，顺手更新进度。
struct PendingMassPart {
    part_num: u16,
    seq: u8,
    payload_len: usize,
    acked: bool,
}

/// 管理 MASS 传输生命周期的系统，负责和 L2 PB 扩展交互。
#[derive(Component)]
pub struct MassSystem {
    owner_id: String,
    reverse_mass_waits: HashMap<u8, ReverseMassWaiter>,
}

impl Default for MassSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl MassSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            owner_id,
            reverse_mass_waits: HashMap::new(),
        }
    }

    pub async fn send_file<F>(
        &mut self,
        file_data: Vec<u8>,
        data_type: MassDataType,
        progress_cb: F,
    ) -> Result<()>
    where
        F: Fn(SendMassCallbackData) + Send + Sync + 'static,
    {
        let owner_id = self.owner_id.clone();
        let cb_arc: Arc<dyn Fn(SendMassCallbackData) + Send + Sync> = Arc::new(progress_cb);
        // 注意：这里用 move 把 cb_arc 捕获，保证生命周期。
        send_file_for_owner(owner_id, file_data, data_type, move |d| (cb_arc)(d)).await
    }

    pub async fn send_file_with_known_slice_length<F>(
        &mut self,
        file_data: Vec<u8>,
        data_type: MassDataType,
        expected_slice_length: usize,
        progress_cb: F,
    ) -> Result<()>
    where
        F: Fn(SendMassCallbackData) + Send + Sync + 'static,
    {
        let owner_id = self.owner_id.clone();
        let cb_arc: Arc<dyn Fn(SendMassCallbackData) + Send + Sync> = Arc::new(progress_cb);
        send_file_for_owner_with_known_slice_length(
            owner_id,
            file_data,
            data_type,
            expected_slice_length,
            move |d| (cb_arc)(d),
        )
        .await
    }

    pub fn begin_reverse_mass_receive(
        &mut self,
        channel: L2Channel,
        progress_cb: Arc<dyn Fn(ReceiveMassCallbackData) + Send + Sync>,
    ) -> Result<oneshot::Receiver<Result<ReverseMassReceiveResult>>> {
        self.begin_reverse_mass_receive_multi(&[channel], progress_cb)
    }

    /// 注册一个reverse mass接收任务，狠狠视奸l2packet，，，
    /// 支持监听多个channel，虽然可能没啥逼用
    /// 一旦某个通道率先接收到完整文件，对应的 future 就会被resolve，此时其他通道上尚未完成的接收进度都会被丢弃
    pub fn begin_reverse_mass_receive_multi(
        &mut self,
        channels: &[L2Channel],
        progress_cb: Arc<dyn Fn(ReceiveMassCallbackData) + Send + Sync>,
    ) -> Result<oneshot::Receiver<Result<ReverseMassReceiveResult>>> {
        if channels.is_empty() {
            bail_site!("reverse MASS receive requires at least one channel");
        }

        for channel in channels {
            if self.reverse_mass_waits.contains_key(&(*channel as u8)) {
                bail_site!(
                    "reverse MASS receive already in progress on channel {:?}",
                    channel
                );
            }
        }

        let (tx, rx) = oneshot::channel();
        let shared_tx = Arc::new(parking_lot::Mutex::new(Some(tx)));
        let siblings: Vec<u8> = channels.iter().map(|c| *c as u8).collect();

        for channel in channels {
            let key = *channel as u8;
            let other_siblings: Vec<u8> =
                siblings.iter().cloned().filter(|s| *s != key).collect();
            self.reverse_mass_waits.insert(
                key,
                ReverseMassWaiter {
                    packet: ReverseMassPacket::new(),
                    progress_cb: progress_cb.clone(),
                    tx: shared_tx.clone(),
                    siblings: other_siblings,
                },
            );
        }
        Ok(rx)
    }

    pub fn clear_reverse_mass_wait(&mut self, channel: L2Channel) {
        let key = channel as u8;
        let siblings = self
            .reverse_mass_waits
            .get(&key)
            .map(|w| w.siblings.clone())
            .unwrap_or_default();
        self.reverse_mass_waits.remove(&key);
        for sibling in siblings {
            self.reverse_mass_waits.remove(&sibling);
        }
    }

    fn handle_prepare_response(&mut self, resp: protocol::PrepareResponse) {
        let owner = self.owner_id.clone();
        let _ = with_device_component_mut::<MassComponent, _, _>(owner, move |comp| {
            if let Some(tx) = comp.prepare_wait.lock().take() {
                let _ = tx.send(resp);
            }
        });
    }

    fn handle_reverse_mass_payload(&mut self, channel: L2Channel, payload: &[u8]) {
        let channel_key = channel as u8;
        let mut progress_cb = None;
        let mut progress = None;
        let mut completion = None;
        let mut should_remove = false;

        if let Some(waiter) = self.reverse_mass_waits.get_mut(&channel_key) {
            if waiter.packet.empty() && !looks_like_reverse_mass_packet(payload) {
                return;
            }
            match waiter.packet.handle_packet(payload.to_vec()) {
                Ok(()) => {
                    progress_cb = Some(waiter.progress_cb.clone());
                    let total_parts = waiter.packet.total_block();
                    let current_part_num = waiter.packet.current_block();
                    let file_name = waiter.packet.file_name();
                    progress = Some(ReceiveMassCallbackData {
                        channel: channel_key,
                        progress: if total_parts == 0 {
                            0.0
                        } else {
                            current_part_num as f32 / total_parts as f32
                        },
                        total_parts,
                        current_part_num,
                        file_name: file_name.clone(),
                    });

                    if waiter.packet.complete() {
                        should_remove = true;
                        completion = Some(
                            waiter
                                .packet
                                .file(false)
                                .map(|data| ReverseMassReceiveResult {
                                    channel,
                                    file_name,
                                    data,
                                })
                                .context("failed to assemble reverse MASS payload"),
                        );
                    }
                }
                Err(err) => {
                    should_remove = true;
                    completion = Some(Err(err).context("failed to decode reverse MASS packet"));
                }
            }
        }

        if let (Some(cb), Some(data)) = (progress_cb, progress) {
            (cb)(data);
        }

        if !should_remove {
            return;
        }

        // First channel to complete wins; tear down sibling waiters too.
        let waiter = match self.reverse_mass_waits.remove(&channel_key) {
            Some(w) => w,
            None => return,
        };
        for sibling in &waiter.siblings {
            self.reverse_mass_waits.remove(sibling);
        }

        if let Some(result) = completion {
            if let Some(tx) = waiter.tx.lock().take() {
                if tx.send(result).is_err() {
                    log::debug!("reverse MASS waiter receiver dropped before completion");
                }
            }
        }
    }
}

impl XiaomiSystemExt for MassSystem {
    fn on_layer2_packet(&mut self, channel: L2Channel, _opcode: L2OpCode, payload: &[u8]) {
        match channel {
            L2Channel::Pb => match protocol::WearPacket::decode(Cursor::new(payload)) {
                Ok(packet) => {
                    if let Some(protocol::wear_packet::Payload::Mass(mass)) = packet.payload {
                        if let Some(protocol::mass::Payload::PrepareResponse(resp)) = mass.payload {
                            self.handle_prepare_response(resp);
                        }
                    }
                }
                Err(err) => {
                    log::warn!(
                        "failed to decode Xiaomi PB payload for MassSystem ({} bytes): {}",
                        payload.len(),
                        err
                    );
                }
            },
            _ => {
                let key = channel as u8;
                if self.reverse_mass_waits.contains_key(&key) {
                    self.handle_reverse_mass_payload(channel, payload);
                }
            }
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct MassComponent {
    #[serde(skip_serializing)]
    prepare_wait: Mutex<Option<oneshot::Sender<protocol::PrepareResponse>>>, // 等 Prepare 回包的单次通道
    resume_state: Option<ResumeState>, // 断点续传需要的“我现在传到哪了”
}

impl MassComponent {
    pub fn new() -> Self {
        Self {
            prepare_wait: Mutex::new(None),
            resume_state: None,
        }
    }
}

fn looks_like_reverse_mass_packet(payload: &[u8]) -> bool {
    payload.len() >= 12 && payload.first().copied() == Some(0)
}

/// 构造 Prepare 请求（问设备：你能吃多大一口？）
fn build_mass_prepare_request(
    data_type: MassDataType,
    file_md5: &Vec<u8>,
    file_length: usize,
) -> protocol::WearPacket {
    let mass_payload = protocol::PrepareRequest {
        data_type: data_type as u32,
        data_id: file_md5.to_vec(),
        data_length: file_length as u32,
        support_compress_mode: None,
    };

    let mass_pkt = protocol::Mass {
        payload: Some(protocol::mass::Payload::PrepareRequest(mass_payload)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::Mass as i32,
        id: protocol::mass::MassId::Prepare as u32,
        payload: Some(protocol::wear_packet::Payload::Mass(mass_pkt)),
    }
}

/// 通过 owner_id 驱动 MASS 文件发送（核心逻辑基本都在这）
/// 流程小抄：
/// 1) 发 Prepare，拿设备切片能力；
/// 2) 把文件做成 MassPayload，再按设备给的大小切片；
/// 3) 持续把分片丢给底层 SAR（它有窗口控制，负责限速/重传等）；
/// 4) 盯 ACK、更新进度；卡住就等一等；
/// 5) 断点续传：随进度更新 current_part。
pub async fn send_file_for_owner<F>(
    owner_id: String,
    file_data: Vec<u8>,
    data_type: MassDataType,
    progress_cb: F,
) -> Result<()>
where
    F: Fn(SendMassCallbackData) + Send + Sync,
{
    let file_md5 = crate::tools::calc_md5(&file_data);
    let file_len = file_data.len();
    let profiler = get_transport_profiler(&owner_id).await;
    log::info!("Building MASS Prepare response listener...");

    // 1) 建立一次性通道，等设备的 PrepareResponse
    let (tx, rx) = oneshot::channel();
    crate::ecs::with_rt_mut({
        let owner = owner_id.clone();
        move |rt| {
            let _ = rt.with_device_mut(&owner, |world, entity| {
                if let Some(comp) = world.get_mut::<MassComponent>(entity) {
                    *comp.prepare_wait.lock() = Some(tx);
                }
            });
        }
    })
    .await;

    log::info!("Sending MASS Prepare...");
    let prepare_started_at = Instant::now();
    // 2) 发 Prepare 请求，同时取下设备地址（断点续传用来校验是不是同一台设备）
    let device_addr = crate::ecs::with_rt_mut({
        let owner = owner_id.clone();
        let file_md5_clone = file_md5.clone();
        move |rt| {
            rt.with_device_mut(&owner, |world, entity| {
                if let Some(mut dev) = world.get_mut::<XiaomiDevice>(entity) {
                    let prepare_pkt =
                        build_mass_prepare_request(data_type, &file_md5_clone, file_len);
                    packet::cipher::enqueue_pb_packet(
                        dev.as_mut(),
                        prepare_pkt,
                        "MassSystem::send_file_for_owner.prepare",
                    );
                    dev.addr().to_string()
                } else {
                    String::new()
                }
            })
            .unwrap_or_default()
        }
    })
    .await;
    if let Some(profiler) = profiler.as_ref() {
        profiler.record(
            "mass",
            "prepare_request",
            None,
            Some(1),
            Some(file_len as u64),
            None,
            Some(true),
            None,
        );
    }

    log::info!("Waiting for prepare response...");

    // 3) 等设备回能力参数
    let prepare_resp = rx.await.context("Mass prepare response not received")?;
    if prepare_resp.prepare_status != protocol::PrepareStatus::Ready as i32 {
        bail_site!("Mass data prepare was not READY");
    }
    if let Some(profiler) = profiler.as_ref() {
        profiler.record(
            "mass",
            "prepare_response",
            Some(
                prepare_started_at
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            ),
            Some(1),
            None,
            None,
            Some(true),
            Some(format!(
                "expected_slice_length={}",
                prepare_resp.expected_slice_length()
            )),
        );
    }
    let expected_slice_length = prepare_resp.expected_slice_length() as usize;
    send_file_for_owner_with_slice_length(
        owner_id,
        file_data,
        data_type,
        expected_slice_length,
        file_md5,
        device_addr,
        progress_cb,
    )
    .await
}

pub async fn send_file_for_owner_with_known_slice_length<F>(
    owner_id: String,
    file_data: Vec<u8>,
    data_type: MassDataType,
    expected_slice_length: usize,
    progress_cb: F,
) -> Result<()>
where
    F: Fn(SendMassCallbackData) + Send + Sync,
{
    let file_md5 = crate::tools::calc_md5(&file_data);
    let device_addr = crate::ecs::with_rt_mut({
        let owner = owner_id.clone();
        move |rt| {
            rt.with_device_mut(&owner, |world, entity| {
                world
                    .get::<XiaomiDevice>(entity)
                    .map(|dev| dev.addr().to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
        }
    })
    .await;

    send_file_for_owner_with_slice_length(
        owner_id,
        file_data,
        data_type,
        expected_slice_length,
        file_md5,
        device_addr,
        progress_cb,
    )
    .await
}

async fn send_file_for_owner_with_slice_length<F>(
    owner_id: String,
    file_data: Vec<u8>,
    data_type: MassDataType,
    expected_slice_length: usize,
    file_md5: Vec<u8>,
    device_addr: String,
    progress_cb: F,
) -> Result<()>
where
    F: Fn(SendMassCallbackData) + Send + Sync,
{
    let profiler = get_transport_profiler(&owner_id).await;
    let miwear_packet_body_max_len = expected_slice_length;
    if miwear_packet_body_max_len == 0 {
        bail_site!("Device reported expected_slice_length of 0, cannot proceed.");
    }

    // 把文件包成 MASS 内部负载，并附带 CRC32（设备端用于校验）
    let mass_inner_payload = MassPacket::build(file_data, data_type)?;
    let mass_inner_payload_with_crc32 = mass_inner_payload.encode_with_crc32();

    // MiWearPacket Body 结构：Channel(1) | Op(1) | blocks_num(2) | resume_block(2) | MassFragment
    // 所以真正能放分片的空间要减去上面 1+1+2+2 的头部
    let mass_fragment_max_len = miwear_packet_body_max_len.saturating_sub(1 + 1 + 2 + 2);
    if mass_fragment_max_len == 0 {
        bail_site!(
            "Calculated mass_fragment_max_len is 0. Device limit ({}) is too small.",
            miwear_packet_body_max_len
        );
    }

    // 算总片数：向上取整
    let total_parts =
        (mass_inner_payload_with_crc32.len() as f32 / mass_fragment_max_len as f32).ceil() as u16;
    if total_parts == 0 && !mass_inner_payload_with_crc32.is_empty() {
        bail_site!("Calculated total_parts is 0 for non-empty payload.");
    }

    // 断点续传：如果有之前记录且 data/device 都匹配，就从记录的分片号继续
    let start_part = crate::ecs::with_rt_mut({
        let owner = owner_id.clone();
        let file_md5_for_resume = file_md5.clone();
        let device_addr_for_resume = device_addr.clone();
        move |rt| {
            rt.with_device_mut(&owner, |world, entity| {
                if let Some(mut comp) = world.get_mut::<MassComponent>(entity) {
                    if let Some(state) = comp.resume_state.as_ref().filter(|s| {
                        s.mass_id == file_md5_for_resume && s.device_addr == device_addr_for_resume
                    }) {
                        return state.current_part;
                    }
                    comp.resume_state = Some(ResumeState {
                        device_addr: device_addr_for_resume.clone(),
                        mass_id: file_md5_for_resume.clone(),
                        current_part: 1,
                    });
                }
                1u16
            })
            .unwrap_or(1u16)
        }
    })
    .await;

    // 从 SAR 拿窗口大小 & 发送超时，便于自适应批量/等待策略
    let sar_hints = crate::ecs::with_rt_mut({
        let owner = owner_id.clone();
        move |rt| {
            rt.with_device_mut(&owner, |world, entity| {
                world.get_mut::<XiaomiDevice>(entity).map(|dev| {
                    let sar = dev.sar.lock();
                    (
                        sar.tx_window_size(),
                        sar.raw_tx_window_size(),
                        sar.send_timeout_ms(),
                    )
                })
            })
            .flatten()
        }
    })
    .await;

    let (tx_window_hint, raw_tx_window, send_timeout_hint_ms) = sar_hints
        .map(|(soft, raw, timeout)| (Some(soft), Some(raw), Some(timeout)))
        .unwrap_or((None, None, None));

    let mass_config = crate::ecs::with_rt_mut({
        let owner = owner_id.clone();
        move |rt| {
            rt.with_device_mut(&owner, |world, entity| {
                world
                    .get::<XiaomiDevice>(entity)
                    .map(|dev| dev.config.mass.clone())
            })
            .flatten()
        }
    })
    .await
    .with_context(|| format!("Device {} not found when retrieving MASS config", owner_id))?;

    // 7) 基于 hint 计算我们的批大小/软上限/卡顿判定门限
    let batch_limit = compute_batch_limit(&mass_config, tx_window_hint);
    let backlog_soft_limit = compute_backlog_soft_limit(&mass_config, tx_window_hint);
    let ack_stall_deadline =
        compute_ack_stall_deadline(&mass_config, tx_window_hint, send_timeout_hint_ms);

    log::info!(
        "[Mass] send setup: tx_window_soft_hint={:?}, tx_window_raw_hint={:?}, send_timeout_hint_ms={:?}, total_parts={}, fragment_max_len={}, batch_limit={}, backlog_limit={}",
        tx_window_hint,
        raw_tx_window,
        send_timeout_hint_ms,
        total_parts,
        mass_fragment_max_len,
        batch_limit,
        backlog_soft_limit,
    );
    if let Some(profiler) = profiler.as_ref() {
        profiler.record(
            "mass",
            "send_setup",
            None,
            None,
            Some(mass_inner_payload_with_crc32.len() as u64),
            None,
            Some(true),
            Some(format!(
                "total_parts={},fragment_max_len={},batch_limit={},backlog_limit={}",
                total_parts, mass_fragment_max_len, batch_limit, backlog_soft_limit
            )),
        );
    }

    // 发送主循环：按批次装包 -> 入队 -> 根据 ACK 控制节奏
    let mut pending_parts = VecDeque::new();
    let mut batch_payloads: Vec<Vec<u8>> = Vec::with_capacity(batch_limit);
    let mut batch_meta: Vec<(u16, usize)> = Vec::with_capacity(batch_limit);
    let mut last_progress_at = Instant::now();
    let mut flush_count = 0usize;

    for i in (start_part - 1)..total_parts {
        let current_part_num = i + 1;
        let start_index = i as usize * mass_fragment_max_len;
        let end_index = std::cmp::min(
            start_index + mass_fragment_max_len,
            mass_inner_payload_with_crc32.len(),
        );
        let fragment = &mass_inner_payload_with_crc32[start_index..end_index];

        // MASS 片内的实际负载：总片数(2B) + 当前片号(2B) + 数据片
        let mut actual_data_payload = Vec::with_capacity(4 + fragment.len());
        actual_data_payload
            .write_u16::<LittleEndian>(total_parts)
            .unwrap();
        actual_data_payload
            .write_u16::<LittleEndian>(current_part_num)
            .unwrap();
        actual_data_payload.extend_from_slice(fragment);

        // 打成 L2 包（Mass 写操作）
        let actual_data_payload_len = actual_data_payload.len();
        batch_payloads
            .push(L2Packet::new(L2Channel::Mass, L2OpCode::Write, actual_data_payload).to_bytes());
        batch_meta.push((current_part_num, actual_data_payload_len));

        // 批攒够了就立刻下发，并根据 ACK 调整节奏
        if batch_payloads.len() >= batch_limit {
            flush_count += 1;
            flush_mass_batch(
                &owner_id,
                &mut batch_payloads,
                &mut batch_meta,
                &mut pending_parts,
                flush_count,
                profiler.as_ref(),
            )
            .await?;
            enforce_flow_control(
                &owner_id,
                &mut pending_parts,
                total_parts,
                &progress_cb,
                &mass_config,
                ack_stall_deadline,
                backlog_soft_limit,
                &mut last_progress_at,
                profiler.as_ref(),
            )
            .await?;
        }
    }

    // 把尾巴再送出去
    if !batch_payloads.is_empty() {
        flush_count += 1;
        flush_mass_batch(
            &owner_id,
            &mut batch_payloads,
            &mut batch_meta,
            &mut pending_parts,
            flush_count,
            profiler.as_ref(),
        )
        .await?;
    }

    // 再来一轮节流/推进
    enforce_flow_control(
        &owner_id,
        &mut pending_parts,
        total_parts,
        &progress_cb,
        &mass_config,
        ack_stall_deadline,
        backlog_soft_limit,
        &mut last_progress_at,
        profiler.as_ref(),
    )
    .await?;

    // 最后把队头一个个等 ACK，直到清空
    while let Some(front_seq) = pending_parts.front().map(|p| p.seq) {
        wait_for_seq_ack(&owner_id, front_seq, &mass_config).await?;
        consume_acked_parts(&owner_id, &mut pending_parts, total_parts, &progress_cb).await?;
    }

    Ok(())
}

/// 把一批分片丢进底层 SAR 队列，并记录 seq -> part 的映射到 pending 队列
async fn flush_mass_batch(
    owner_id: &str,
    batch_payloads: &mut Vec<Vec<u8>>,
    batch_meta: &mut Vec<(u16, usize)>,
    pending_parts: &mut VecDeque<PendingMassPart>,
    flush_round: usize,
    profiler: Option<&TransportProfilerHandle>,
) -> Result<()> {
    if batch_payloads.is_empty() {
        return Ok(());
    }

    let payloads = mem::take(batch_payloads);
    let meta = mem::take(batch_meta);
    let meta_len = meta.len();

    let seqs = enqueue_mass_batch(owner_id, payloads).await?;
    if seqs.len() != meta_len {
        bail_site!(
            "enqueue_batch returned {} seqs but {} payloads were submitted",
            seqs.len(),
            meta_len
        );
    }

    for ((part_num, payload_len), seq) in meta.into_iter().zip(seqs.into_iter()) {
        pending_parts.push_back(PendingMassPart {
            part_num,
            seq,
            payload_len,
            acked: false,
        });
    }

    if let Some(profiler) = profiler {
        let packet_count = pending_parts
            .iter()
            .rev()
            .take(meta_len)
            .count() as u32;
        let total_bytes = pending_parts
            .iter()
            .rev()
            .take(meta_len)
            .map(|part| part.payload_len as u64)
            .sum::<u64>();
        profiler.record(
            "mass",
            "flush_batch",
            None,
            Some(packet_count),
            Some(total_bytes),
            None,
            Some(true),
            Some(format!("round={flush_round}")),
        );
    }

    Ok(())
}

/// 根据 ACK 推进进度 + 节流：
/// 思想：先尽量“吃掉”已经 ACK 的队头；如果 backlog 太大或太久没进展，就强制等一个 ACK。
async fn enforce_flow_control<F>(
    owner_id: &str,
    pending_parts: &mut VecDeque<PendingMassPart>,
    total_parts: u16,
    progress_cb: &F,
    config: &MassConfig,
    ack_stall_deadline: Duration,
    backlog_soft_limit: usize,
    last_progress_at: &mut Instant,
    profiler: Option<&TransportProfilerHandle>,
) -> Result<()>
where
    F: Fn(SendMassCallbackData) + Send + Sync,
{
    // 先看看能不能把队头消费一波
    let consumed = consume_acked_parts(owner_id, pending_parts, total_parts, progress_cb).await?;
    if consumed > 0 {
        *last_progress_at = Instant::now();
    }

    if pending_parts.is_empty() {
        return Ok(());
    }

    // 两种情况需要“踩刹车”：
    // 1) backlog 超过软上限；2) 太久没进展（可能设备处理不过来）
    let now = Instant::now();
    let mut should_wait = pending_parts.len() >= backlog_soft_limit;
    if !should_wait && now.duration_since(*last_progress_at) >= ack_stall_deadline {
        should_wait = true;
    }

    if should_wait {
        if let Some(front_seq) = pending_parts.front().map(|p| p.seq) {
            let wait_reason = if pending_parts.len() >= backlog_soft_limit {
                "backlog"
            } else {
                "stall"
            };
            let wait_started_at = Instant::now();
            // 等队头 ACK 一个，再继续推进
            wait_for_seq_ack(owner_id, front_seq, config).await?;
            if let Some(profiler) = profiler {
                profiler.record(
                    "mass",
                    "flow_wait",
                    Some(wait_started_at.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
                    None,
                    None,
                    Some(u32::from(front_seq)),
                    Some(true),
                    Some(format!(
                        "reason={},pending_parts={}",
                        wait_reason,
                        pending_parts.len()
                    )),
                );
            }
            let consumed_after_wait =
                consume_acked_parts(owner_id, pending_parts, total_parts, progress_cb).await?;
            if consumed_after_wait > 0 {
                *last_progress_at = Instant::now();
            }
        }
    }

    Ok(())
}

async fn get_transport_profiler(owner_id: &str) -> Option<TransportProfilerHandle> {
    let owner = owner_id.to_string();
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&owner, |world, entity| {
            world
                .get::<XiaomiDevice>(entity)
                .map(|dev| dev.transport_profiler.clone())
        })
        .flatten()
    })
    .await
}

/// 真正把批量包入队（交给 SAR），拿回每个包对应的 seq
async fn enqueue_mass_batch(owner_id: &str, payloads: Vec<Vec<u8>>) -> Result<Vec<u8>> {
    if payloads.is_empty() {
        return Ok(Vec::new());
    }

    crate::ecs::with_rt_mut({
        let owner = owner_id.to_string();
        move |rt| -> Result<Vec<u8>> {
            rt.with_device_mut(&owner, |world, entity| {
                if let Some(dev) = world.get_mut::<XiaomiDevice>(entity) {
                    Ok(dev.sar.lock().enqueue_batch(payloads))
                } else {
                    bail_site!("Device {} not found when enqueueing MASS batch", owner)
                }
            })
            .unwrap_or_else(|| bail_site!("Device {} not found when enqueueing MASS batch", owner))
        }
    })
    .await
}

/// 批大小：有窗口 hint 就按窗口来（上限 MAX_BATCH_PARTS），否则用保守值
fn compute_batch_limit(config: &MassConfig, window_hint: Option<u8>) -> usize {
    window_hint
        .map(|win| usize::from(win.max(1)))
        .map(|win| win.min(config.max_batch_parts))
        .unwrap_or(config.fallback_batch_parts)
        .max(1)
}

/// backlog 软上限：窗口 * 系数（拿不到窗口就用保守上限），避免把队列堆爆
fn compute_backlog_soft_limit(config: &MassConfig, window_hint: Option<u8>) -> usize {
    let limit = window_hint
        .map(|win| usize::from(win.max(1)).saturating_mul(config.backlog_multiplier))
        .unwrap_or(config.fallback_backlog_limit);
    limit.clamp(config.backlog_multiplier, 256)
}

/// 卡顿判定门限：
/// - 来自窗口大小推一个默认值（window * poll * 3）
/// - 结合底层 send_timeout（取其 1/8，别太激进）
/// - 两者取较小，再夹在 MIN~MAX 范围内
fn compute_ack_stall_deadline(
    config: &MassConfig,
    window_hint: Option<u8>,
    send_timeout_ms: Option<u64>,
) -> Duration {
    let from_window = window_hint
        .map(|win| u64::from(win.max(1)) * config.ack_poll_interval_ms * 3)
        .unwrap_or(config.ack_stall_default_ms);

    let timeout_candidate = send_timeout_ms
        .filter(|ms| *ms > 0 && *ms < u64::MAX)
        .map(|ms| (ms / 8).max(config.ack_stall_min_ms))
        .unwrap_or(from_window);

    let combined = from_window.min(timeout_candidate);
    Duration::from_millis(combined.clamp(config.ack_stall_min_ms, config.ack_stall_max_ms))
}

/// 阻塞等待某个 seq 收到 ACK（带总超时保护）
async fn wait_for_seq_ack(owner_id: &str, seq: u8, config: &MassConfig) -> Result<()> {
    let owner = owner_id.to_string();
    let ack_notifier = crate::ecs::with_rt_mut({
        let owner = owner.clone();
        move |rt| {
            rt.with_device_mut(&owner, |world, entity| {
                world
                    .get_mut::<XiaomiDevice>(entity)
                    .map(|dev| dev.sar.lock().ack_notifier())
            })
            .flatten()
        }
    })
    .await
    .with_context(|| format!("Device {owner_id} not found when waiting for MASS ACK"))?;

    let ack_future = async {
        loop {
            let notified = ack_notifier.notified();
            let owner_clone = owner.clone();
            let acked = crate::ecs::with_rt_mut(move |rt| {
                rt.with_device_mut(&owner_clone, |world, entity| {
                    if let Some(dev) = world.get_mut::<XiaomiDevice>(entity) {
                        return dev.sar.lock().is_acked(seq);
                    }
                    false
                })
                .unwrap_or(false)
            })
            .await;
            if acked {
                break;
            }
            notified.await;
        }
    };

    timeout(
        Duration::from_secs(config.ack_wait_timeout_secs),
        ack_future,
    )
    .await
    .context("Timeout waiting for mass packet ACK")?;
    Ok(())
}

/// 把已经 ACK 的队头逐个弹出，顺便更新断点续传状态 & 进度回调
async fn consume_acked_parts<F>(
    owner_id: &str,
    pending_parts: &mut VecDeque<PendingMassPart>,
    total_parts: u16,
    progress_cb: &F,
) -> Result<usize>
where
    F: Fn(SendMassCallbackData) + Send + Sync,
{
    let mut consumed = 0usize;
    let mut latest_progress = None;

    loop {
        let (part_num, payload_len, seq) = {
            let front = match pending_parts.front_mut() {
                Some(front) => front,
                None => break,
            };

            if !front.acked {
                // 如果队头还没标记 acked，就去底层查一下；
                // ack 了就更新续传状态，并把该 seq 标记“可消费”。
                let seq = front.seq;
                let next_part = front.part_num.saturating_add(1);
                let owner = owner_id.to_string();
                let acked = crate::ecs::with_rt_mut({
                    let owner = owner.clone();
                    move |rt| {
                        rt.with_device_mut(&owner, |world, entity| {
                            let acked = if let Some(dev) = world.get_mut::<XiaomiDevice>(entity) {
                                dev.sar.lock().is_acked(seq)
                            } else {
                                false
                            };
                            if acked {
                                if let Some(mut comp) = world.get_mut::<MassComponent>(entity) {
                                    if let Some(state) = comp.resume_state.as_mut() {
                                        state.current_part = next_part;
                                        if state.current_part > total_parts {
                                            comp.resume_state = None;
                                        }
                                    }
                                }
                                if let Some(dev) = world.get_mut::<XiaomiDevice>(entity) {
                                    dev.sar.lock().mark_ack_consumed(seq);
                                }
                            }
                            acked
                        })
                        .unwrap_or(false)
                    }
                })
                .await;

                if !acked {
                    break;
                }

                front.acked = true;
            }

            (front.part_num, front.payload_len, front.seq)
        };

        pending_parts.pop_front();
        let progress = if total_parts == 0 {
            1.0
        } else {
            part_num as f32 / total_parts as f32
        };
        latest_progress = Some(SendMassCallbackData {
            progress,
            total_parts,
            current_part_num: part_num,
            actual_data_payload_len: payload_len,
        });
        let _ = seq;
        consumed += 1;
    }

    if let Some(progress) = latest_progress {
        (progress_cb)(progress);
    }

    Ok(consumed)
}
