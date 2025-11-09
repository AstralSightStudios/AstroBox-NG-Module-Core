use std::collections::{HashSet, VecDeque};

#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};
#[cfg(target_arch = "wasm32")]
use web_time::{Duration, Instant};

use crate::asyncrt::{TaskHandle, sleep, spawn_with_handle};
use tokio::runtime::Handle;

use super::SendFn;
use crate::device::xiaomi::{
    config::SarConfig,
    packet::v2::{
        layer1::{L1DataType, L1Packet},
        layer1cmd::{CmdCode, L1CmdBuilder, L1CmdPacket},
        layer2::L2Channel,
    },
};

mod command_pool;
pub use command_pool::CommandPool;

/// 待发送的数据（已分配 seq）
pub struct QueuedData {
    pub seq: u8,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
struct SendItem {
    /// 已经构造完成、等待发送或等待 ACK 的 L1 数据包。
    packet: L1Packet,
    /// 该条目当前是否处于等待 ACK 状态。
    wait_ack: bool,
    /// 是否触发了重传标记（NAK/超时都会设置）。
    need_retransmission: bool,
    /// 期待收到 ACK 的截止时间，用于检测是否需要重传。
    deadline: Instant,
}

/// 管理 SAR L1/L2 发送状态的核心控制器，实现窗口、超时与累积确认逻辑。
pub struct SarController {
    sender: SendFn,
    tk_handle: Handle,
    device_id: String,
    pub command_pool: CommandPool,
    tx_queue: VecDeque<SendItem>,
    tx_next_seq: u8,
    tx_base: u8,
    tx_win: u8,
    tx_win_effective: u8,
    send_timeout: Duration,
    rx_expect_seq: u8,
    rx_cum_ack_index: u8,
    rx_cum_ack_seq: u8,
    rx_cum_ack_timer: Option<TaskHandle>,
    /// 记录已经确认的 seq，供上层查询（会在 seq 重用或消费后清理）。
    acked: HashSet<u8>,
    config: SarConfig,
}

impl SarController {
    pub fn new(tk_handle: Handle, sender: SendFn, device_id: String, config: SarConfig) -> Self {
        log::info!("Initializing SarController...");

        let mut ctrl = Self {
            sender: sender.clone(),
            tk_handle,
            device_id: device_id.clone(),
            command_pool: CommandPool::new(),
            tx_queue: VecDeque::new(),
            tx_next_seq: 0,
            tx_base: 0,
            tx_win: 16,
            tx_win_effective: Self::compute_soft_cap_with_allowance(
                16,
                config.tx_win_overrun_allowance,
            ),
            send_timeout: Duration::from_millis(15_000),
            rx_expect_seq: 0,
            rx_cum_ack_index: 0,
            rx_cum_ack_seq: 0,
            rx_cum_ack_timer: None,
            acked: HashSet::new(),
            config,
        };

        // 启动定时检查超时任务
        ctrl.start_timeout_checker(device_id.clone());

        log::info!("Sending L1StartReq...");

        // 构建并推入 L1StartReq，优先发送
        let start_req = L1CmdBuilder::new()
            .cmd(CmdCode::CmdL1startReq)
            .version(1, 0, 0)
            .mps(0xFFFF)
            .tx_win(16)
            .send_timeout(15_000)
            .device_type(0)
            .build()
            .unwrap();
        ctrl.command_pool
            .push_cmd_front(start_req.to_payload_bytes());
        ctrl.try_run_next();

        log::info!("SarController initialization completed!");

        ctrl
    }

    /// 将数据加入发送队列，返回分配的 seq
    pub fn enqueue(&mut self, data: Vec<u8>) -> u8 {
        let seq = self.alloc_seq();
        self.command_pool.push(QueuedData { seq, payload: data });
        self.try_run_next();
        seq
    }

    /// 批量入队，可减少多次 runtime 切换开销，返回每个 payload 对应的 seq。
    pub fn enqueue_batch<I>(&mut self, iter: I) -> Vec<u8>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let mut seqs = Vec::new();
        for data in iter {
            let seq = self.alloc_seq();
            self.command_pool.push(QueuedData { seq, payload: data });
            seqs.push(seq);
        }
        self.try_run_next();
        seqs
    }

    /// 插队到队首
    pub fn enqueue_front(&mut self, data: Vec<u8>) -> u8 {
        let seq = self.alloc_seq();
        self.command_pool
            .push_front(QueuedData { seq, payload: data });
        self.try_run_next();
        seq
    }

    /// 批量插队，返回按顺序分配的 seq 列表
    pub fn enqueue_front_batch<I>(&mut self, iter: I) -> Vec<u8>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let mut items: Vec<Vec<u8>> = iter.into_iter().collect();
        let mut seqs = Vec::new();
        while let Some(d) = items.pop() {
            let seq = self.alloc_seq();
            self.command_pool.push_front(QueuedData { seq, payload: d });
            seqs.push(seq);
        }
        self.try_run_next();
        seqs.reverse();
        seqs
    }

    #[inline]
    pub fn runtime_handle(&self) -> Handle {
        self.tk_handle.clone()
    }

    #[inline]
    pub fn tx_window_size(&self) -> u8 {
        self.effective_tx_win()
    }

    #[inline]
    pub fn raw_tx_window_size(&self) -> u8 {
        self.tx_win.max(1)
    }

    #[inline]
    pub fn send_timeout_ms(&self) -> u64 {
        self.send_timeout.as_millis().try_into().unwrap_or(u64::MAX)
    }

    /// 判断单个 seq 是否已被设备确认。
    pub fn is_acked(&self, seq: u8) -> bool {
        self.acked.contains(&seq)
    }

    /// 判断一组 seq 是否全部确认。
    pub fn is_all_acked(&self, seqs: &[u8]) -> bool {
        seqs.iter().all(|s| self.acked.contains(s))
    }

    /// 在外部消费 ACK 后调用，避免陈旧的 ACK 记录影响后续判断。
    pub fn mark_ack_consumed(&mut self, seq: u8) {
        self.acked.remove(&seq);
    }

    fn alloc_seq(&mut self) -> u8 {
        let seq = self.tx_next_seq;
        self.tx_next_seq = self.tx_next_seq.wrapping_add(1);
        if self.tx_next_seq == 0 {
            self.acked.clear();
        }
        seq
    }

    #[inline]
    fn effective_tx_win(&self) -> u8 {
        self.tx_win_effective.max(1)
    }

    fn compute_soft_cap_with_allowance(win: u8, allowance: u8) -> u8 {
        let base = win.max(1);
        base.saturating_add(allowance).clamp(base, u8::MAX)
    }

    fn send_ack(&self, seq: u8) {
        let pkt = L1Packet::new(L1DataType::Ack, false, seq, vec![]);
        let send_fn = self.sender.clone();
        let handle = self.tk_handle.clone();
        spawn_with_handle(
            async move {
                let _ = (send_fn)(pkt.to_bytes()).await;
            },
            handle,
        );
    }

    fn send_nak(&self, seq: u8) {
        let pkt = L1Packet::new(L1DataType::Nak, false, seq, vec![]);
        let send_fn = self.sender.clone();
        let handle = self.tk_handle.clone();
        spawn_with_handle(
            async move {
                let _ = (send_fn)(pkt.to_bytes()).await;
            },
            handle,
        );
    }

    fn start_cum_ack_timer(&mut self, device: String) {
        if self.rx_cum_ack_timer.is_some() {
            return;
        }
        let handle_outer = self.tk_handle.clone();
        let handle_spawn = handle_outer.clone();
        self.rx_cum_ack_timer = Some(spawn_with_handle(
            async move {
                sleep(Duration::from_millis(500)).await;
                let handle_inner = handle_outer.clone();
                crate::ecs::with_rt_mut(move |rt| {
                    if let Some(dev) = rt.find_entity_by_id_mut::<super::XiaomiDevice>(&device) {
                        if dev.sar.rx_cum_ack_index > 0 {
                            let seq = dev.sar.rx_cum_ack_seq;
                            dev.sar.rx_cum_ack_index = 0;
                            dev.sar.rx_cum_ack_timer = None;
                            let send_fn = dev.sar.sender.clone();
                            let handle_send = handle_inner.clone();
                            spawn_with_handle(
                                async move {
                                    let pkt = L1Packet::new(L1DataType::Ack, false, seq, vec![]);
                                    let _ = (send_fn)(pkt.to_bytes()).await;
                                },
                                handle_send,
                            );
                        }
                    }
                })
                .await;
            },
            handle_spawn,
        ));
    }

    fn stop_cum_ack_timer(&mut self) {
        if let Some(h) = self.rx_cum_ack_timer.take() {
            h.abort();
        }
        self.rx_cum_ack_index = 0;
    }

    fn start_timeout_checker(&self, device: String) {
        let handle = self.tk_handle.clone();
        spawn_with_handle(
            async move {
                loop {
                    sleep(Duration::from_millis(500)).await;
                    let dev_id = device.clone();
                    crate::ecs::with_rt_mut(move |rt| {
                        if let Some(dev) = rt.find_entity_by_id_mut::<super::XiaomiDevice>(&dev_id)
                        {
                            dev.sar.check_timeouts_internal();
                        }
                    })
                    .await;
                }
            },
            handle,
        );
    }

    fn check_timeouts_internal(&mut self) {
        let now = Instant::now();
        let mut need = false;
        for item in self.tx_queue.iter_mut() {
            if item.wait_ack && now >= item.deadline {
                item.wait_ack = false;
                item.need_retransmission = true;
                need = true;
            }
        }
        if need {
            self.try_run_next();
        }
    }

    pub fn on_l1_packet(&mut self, l1: &L1Packet) -> bool {
        match l1.pkt_type {
            L1DataType::Ack => {
                self.handle_ack(l1.seq);
                false
            }
            L1DataType::Nak => {
                self.handle_nak(l1.seq);
                false
            }
            L1DataType::Cmd => {
                // 根据发来的CmdRsp调整自身发包参数
                if let Some(cmd) = L1CmdPacket::from_payload_bytes(&l1.payload) {
                    if cmd.cmd == CmdCode::CmdL1startRsp {
                        if let Some(win) = cmd.get_tx_win() {
                            let normalized = win.clamp(1, u16::from(u8::MAX)) as u8;
                            self.tx_win = normalized;
                            self.tx_win_effective = Self::compute_soft_cap_with_allowance(
                                normalized,
                                self.config.tx_win_overrun_allowance,
                            );
                        }
                        if let Some(to) = cmd.get_send_timeout() {
                            self.send_timeout = Duration::from_millis(to as u64);
                        }
                        log::info!(
                            "[SarController] L1StartRsp applied: tx_win={} soft_cap={} send_timeout_ms={}",
                            self.tx_win,
                            self.tx_win_effective,
                            self.send_timeout.as_millis()
                        );
                    }
                }
                false
            }
            L1DataType::Data => {
                let channel = l1.payload.get(0).and_then(|b| L2Channel::try_from(*b).ok());

                if matches!(channel, Some(L2Channel::Network)) {
                    // Network 频道的所有包seq均为0，因此不做seq校验
                    return true;
                }

                if l1.frx {
                    // 快速接收不需要 ACK
                    self.rx_expect_seq = self.rx_expect_seq.wrapping_add(1);
                    return true;
                }

                if l1.seq != self.rx_expect_seq {
                    // 收到的 seq 不是预期的，回 NAK 请求重传
                    self.send_nak(self.rx_expect_seq);
                    return false;
                }

                if channel != Some(L2Channel::Network) {
                    let immediate = u32::from(self.rx_cum_ack_index)
                        >= (u32::from(self.effective_tx_win()) * 2 / 3)
                        || matches!(channel, Some(L2Channel::Pb | L2Channel::Lyra));
                    if immediate {
                        self.stop_cum_ack_timer();
                        self.send_ack(l1.seq);
                    } else {
                        self.rx_cum_ack_index = self.rx_cum_ack_index.saturating_add(1);
                        self.rx_cum_ack_seq = l1.seq;
                        self.start_cum_ack_timer(self.device_id.clone());
                    }
                }

                self.rx_expect_seq = self.rx_expect_seq.wrapping_add(1);
                true
            }
        }
    }

    fn handle_ack(&mut self, seq: u8) {
        while let Some(item) = self.tx_queue.front() {
            if Self::seq_le(item.packet.seq, seq) {
                let seq_val = item.packet.seq;
                self.acked.insert(seq_val);
                self.tx_queue.pop_front();
                self.tx_base = self.tx_base.wrapping_add(1);
            } else {
                break;
            }
        }
        self.try_run_next();
    }

    fn handle_nak(&mut self, seq: u8) {
        if seq > 0 {
            let ack_seq = seq.wrapping_sub(1);
            self.handle_ack(ack_seq);
        }
        for item in self.tx_queue.iter_mut() {
            if Self::seq_le(seq, item.packet.seq) {
                item.need_retransmission = true;
                item.wait_ack = false;
            }
        }
        self.try_run_next();
    }

    fn seq_le(a: u8, b: u8) -> bool {
        b.wrapping_sub(a) < 128
    }

    fn try_run_next(&mut self) {
        // 优先重传，防止错错包
        if let Some(item) = self.tx_queue.iter_mut().find(|i| i.need_retransmission) {
            let pkt = item.packet.clone();
            item.need_retransmission = false;
            item.wait_ack = true;
            item.deadline = Instant::now() + self.send_timeout;
            let send_fn = self.sender.clone();
            let handle = self.tk_handle.clone();
            spawn_with_handle(
                async move {
                    let _ = (send_fn)(pkt.to_bytes()).await;
                },
                handle,
            );
            return;
        }

        // 先发命令
        while let Some(cmd) = self.command_pool.pop_cmd() {
            let pkt = L1Packet::new(L1DataType::Cmd, false, 0, cmd);
            let send_fn = self.sender.clone();
            let handle = self.tk_handle.clone();
            spawn_with_handle(
                async move {
                    let _ = (send_fn)(pkt.to_bytes()).await;
                },
                handle.clone(),
            );
        }

        // 再发数据，受窗口限制
        // 仅根据当前正在等待 ACK 的数量控制窗口，允许上层一次性排队更多数据。
        while self.tx_queue.len() < usize::from(self.effective_tx_win()) {
            let Some(qd) = self.command_pool.pop_data() else {
                break;
            };
            let pkt = L1Packet::new(L1DataType::Data, false, qd.seq, qd.payload);
            let bytes = pkt.to_bytes();
            let send_fn = self.sender.clone();
            let deadline = Instant::now() + self.send_timeout;
            let handle = self.tk_handle.clone();
            spawn_with_handle(
                async move {
                    let _ = (send_fn)(bytes).await;
                },
                handle,
            );
            self.tx_queue.push_back(SendItem {
                packet: pkt,
                wait_ack: true,
                need_retransmission: false,
                deadline,
            });
        }
    }
}
