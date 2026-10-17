use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};
#[cfg(target_arch = "wasm32")]
use web_time::{Duration, Instant};

use crate::asyncrt::{TaskHandle, sleep, spawn_with_handle};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc};

use super::SendFn;
use crate::device::xiaomi::{
    config::SarConfig,
    packet::v2::{
        layer1::{L1DataType, L1Packet},
        layer1cmd::{CmdCode, L1CmdBuilder, L1CmdPacket},
        layer2::L2Channel,
    },
    r#type::ConnectType,
    transport_profiler::TransportProfilerHandle,
};

mod command_pool;
pub use command_pool::CommandPool;

/// 同一个队头帧连续超时重传的次数上限，超过就认定 L1 会话已失步，重启会话。
const MAX_PACKET_RETRIES: u8 = 3;
/// 等不到 L1StartRsp 时最多重发几次 L1StartReq。设备每收到一次都可能清空会话，不能无限重发。
const MAX_START_REQ_RESENDS: u8 = 3;

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
    /// 作为队头时已超时重传的次数
    retries: u8,
}

/// 本端 L1 链路参数：发送窗口与重传超时，会写进 L1StartReq 通告给设备。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SarLinkParams {
    pub tx_win: u8,
    pub send_timeout_ms: u16,
}

impl SarLinkParams {
    /// SPP：对齐安卓官方 TransportL1（窗口 32、超时 10s）。
    pub const SPP: Self = Self {
        tx_win: 32,
        send_timeout_ms: 10_000,
    };

    /// BLE：对齐 iOS 官方 MIWBLETransmissionV2。按单次写入上限
    /// （maximumWriteValueLength）分档定窗口，超时 = 窗口 * 2 / 3 * 1.2 + 3 秒（取整）。
    /// 官方取不到写入上限或拿到 >= 518 时一律按 182 处理。
    pub fn ble(max_write_len: usize) -> Self {
        let len = if max_write_len == 0 || max_write_len >= 518 {
            182
        } else {
            max_write_len
        };
        let tx_win: u8 = match len {
            0..=200 => 4,
            201..=300 => 8,
            301..=400 => 12,
            _ => 16,
        };
        let secs = (f64::from(2 * u32::from(tx_win)) / 3.0 * 1.2 + 3.0) as u16;
        Self {
            tx_win,
            send_timeout_ms: secs * 1000,
        }
    }

    pub fn for_transport(connect_type: ConnectType, ble_max_write_len: usize) -> Self {
        match connect_type {
            ConnectType::SPP => Self::SPP,
            ConnectType::BLE => Self::ble(ble_max_write_len),
        }
    }
}

/// 有序发送队列：数据、重传、ACK/NAK、命令帧全部按入队顺序由同一个任务写出（对齐官方单一写队列）。
/// 以前每次发送各 spawn 一个任务，多线程运行时下谁先抢到发送锁不确定，设备就会看到序号跳变而回 NAK。
struct Outbound {
    tx: mpsc::UnboundedSender<Vec<Vec<u8>>>,
}

impl Outbound {
    fn spawn(sender: SendFn, device_id: String, tk_handle: Handle) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<Vec<u8>>>();
        spawn_with_handle(
            async move {
                while let Some(mut frames) = rx.recv().await {
                    // 把已经排着的帧并成一次写出，少过几次发送锁
                    while let Ok(more) = rx.try_recv() {
                        frames.extend(more);
                    }
                    if let Err(err) = (sender)(frames).await {
                        SarController::on_send_failure(&device_id, err).await;
                    }
                }
            },
            tk_handle,
        );
        Self { tx }
    }

    fn send(&self, frames: Vec<Vec<u8>>) {
        if !frames.is_empty() {
            let _ = self.tx.send(frames);
        }
    }
}

/// 管理 SAR L1/L2 发送状态的核心控制器，实现窗口、超时与累积确认逻辑。
pub struct SarController {
    outbound: Outbound,
    tk_handle: Handle,
    device_id: String,
    pub command_pool: CommandPool,
    tx_queue: VecDeque<SendItem>,
    tx_next_seq: u8,
    tx_base: u8,
    /// 设备在 L1StartRsp 里通告的发送窗口，只用于累积 ACK 门限
    tx_win: u8,
    /// 本端发送窗口（链路参数 + 超发余量）
    tx_win_effective: u8,
    link: SarLinkParams,
    send_timeout: Duration,
    rx_expect_seq: u8,
    /// 本会话是否已按序收到过数据帧（决定落后帧要不要补 ACK）
    rx_in_order_seen: bool,
    rx_cum_ack_index: u8,
    rx_cum_ack_seq: u8,
    rx_cum_ack_timer: Option<TaskHandle>,
    cmd_exchanged: bool,
    /// 等 L1StartRsp 的截止时间，超时重发 L1StartReq
    start_req_deadline: Option<Instant>,
    start_req_resends: u8,
    /// 发送失败后序号可能已与设备失步，下次有数据要发时先重启会话
    needs_restart: bool,
    /// 每次丢弃在途数据（中止/重启会话）都会递增，上层据此判断等待的 seq 是否已作废
    session_epoch: u32,
    /// 最近一次因 NAK 触发补发的序号与时间，用来合并同一缺口的重复 NAK
    last_nak_retransmit: Option<(u8, Instant)>,
    /// 记录已经确认的 seq，供上层查询（会在 seq 重用或消费后清理）。
    acked: HashSet<u8>,
    ack_notify: Arc<Notify>,
    profiler: TransportProfilerHandle,
    timeout_checker: Option<TaskHandle>,
}

impl SarController {
    pub fn new(
        tk_handle: Handle,
        sender: SendFn,
        device_id: String,
        profiler: TransportProfilerHandle,
        config: SarConfig,
        link: SarLinkParams,
    ) -> Self {
        log::info!(
            "Initializing SarController (tx_win={}, send_timeout_ms={})...",
            link.tx_win,
            link.send_timeout_ms
        );

        let mut ctrl = Self {
            outbound: Outbound::spawn(sender, device_id.clone(), tk_handle.clone()),
            tk_handle,
            device_id: device_id.clone(),
            command_pool: CommandPool::new(),
            tx_queue: VecDeque::new(),
            tx_next_seq: 0,
            tx_base: 0,
            tx_win: link.tx_win,
            tx_win_effective: Self::compute_soft_cap_with_allowance(
                link.tx_win,
                config.tx_win_overrun_allowance,
            ),
            link,
            send_timeout: Duration::from_millis(u64::from(link.send_timeout_ms)),
            rx_expect_seq: 0,
            rx_in_order_seen: false,
            rx_cum_ack_index: 0,
            rx_cum_ack_seq: 0,
            rx_cum_ack_timer: None,
            cmd_exchanged: false,
            start_req_deadline: None,
            start_req_resends: 0,
            needs_restart: false,
            session_epoch: 0,
            last_nak_retransmit: None,
            acked: HashSet::new(),
            ack_notify: Arc::new(Notify::new()),
            profiler,
            timeout_checker: None,
        };

        // 启动定时检查超时任务
        ctrl.timeout_checker = Some(ctrl.start_timeout_checker(device_id));

        log::info!("Sending L1StartReq...");
        ctrl.begin_handshake();

        log::info!("SarController initialization completed!");

        ctrl
    }

    /// 将数据加入发送队列，返回分配的 seq
    pub fn enqueue(&mut self, data: Vec<u8>) -> u8 {
        self.ensure_session();
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
        self.ensure_session();
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
        self.ensure_session();
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
        self.ensure_session();
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

    #[inline]
    pub fn session_epoch(&self) -> u32 {
        self.session_epoch
    }

    /// 判断单个 seq 是否已被设备确认。
    pub fn is_acked(&self, seq: u8) -> bool {
        self.acked.contains(&seq)
    }

    /// 判断一组 seq 是否全部确认。
    pub fn is_all_acked(&self, seqs: &[u8]) -> bool {
        seqs.iter().all(|s| self.acked.contains(s))
    }

    #[inline]
    pub fn ack_notifier(&self) -> Arc<Notify> {
        self.ack_notify.clone()
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

    fn build_start_cmd(&self, cmd: CmdCode) -> Vec<u8> {
        L1CmdBuilder::new()
            .cmd(cmd)
            .version(1, 0, 0)
            .mps(64512)
            .tx_win(u16::from(self.link.tx_win))
            .send_timeout(self.link.send_timeout_ms)
            .build()
            .unwrap()
            .to_payload_bytes()
    }

    /// 发 L1StartReq 并开始等 L1StartRsp（官方的 START_REQ 是需要应答的发送项，超时会重发）
    fn send_start_req(&mut self) {
        let payload = self.build_start_cmd(CmdCode::CmdL1startReq);
        self.command_pool.push_cmd_front(payload);
        self.start_req_deadline = Some(Instant::now() + self.send_timeout);
        self.try_run_next();
    }

    /// 清空收发序号状态，回到会话初始（对齐官方 clearTranCtl）
    fn reset_sequence_state(&mut self) {
        self.tx_queue.clear();
        self.tx_next_seq = 0;
        self.tx_base = 0;
        self.rx_expect_seq = 0;
        self.rx_in_order_seen = false;
        self.stop_cum_ack_timer();
        self.rx_cum_ack_seq = 0;
        self.acked.clear();
        self.last_nak_retransmit = None;
    }

    fn begin_handshake(&mut self) {
        self.start_req_resends = 0;
        self.send_start_req();
    }

    /// 重新握手：序号归零并发 L1StartReq，设备收到后同样清空会话再回 L1StartRsp。
    fn restart_session(&mut self) {
        log::warn!(
            "[SarController] Restarting L1 session for {}",
            self.device_id
        );
        self.profiler.record(
            "sar",
            "session_restart",
            None,
            None,
            None,
            None,
            Some(false),
            None,
        );
        self.reset_sequence_state();
        self.needs_restart = false;
        self.cmd_exchanged = false;
        self.begin_handshake();
    }

    fn ensure_session(&mut self) {
        if self.needs_restart {
            self.restart_session();
        }
    }

    fn send_ack(&self, seq: u8) {
        let pkt = L1Packet::new(L1DataType::Ack, false, seq, vec![]);
        let bytes = pkt.to_bytes();
        self.profiler.record(
            "sar",
            "ack_send",
            None,
            Some(1),
            Some(bytes.len() as u64),
            Some(u32::from(seq)),
            Some(true),
            None,
        );
        self.outbound.send(vec![bytes]);
    }

    fn send_nak(&self, seq: u8) {
        let pkt = L1Packet::new(L1DataType::Nak, false, seq, vec![]);
        let bytes = pkt.to_bytes();
        self.profiler.record(
            "sar",
            "nak_send",
            None,
            Some(1),
            Some(bytes.len() as u64),
            Some(u32::from(seq)),
            Some(false),
            None,
        );
        self.outbound.send(vec![bytes]);
    }

    fn start_cum_ack_timer(&mut self, device: String) {
        if self.rx_cum_ack_timer.is_some() {
            return;
        }
        self.rx_cum_ack_timer = Some(spawn_with_handle(
            async move {
                sleep(Duration::from_millis(500)).await;
                crate::ecs::with_rt_mut(move |rt| {
                    let _ = rt.with_device_mut(&device, |world, entity| {
                        if let Some(dev) = world.get_mut::<super::XiaomiDevice>(entity) {
                            let mut sar = dev.sar.lock();
                            sar.rx_cum_ack_timer = None;
                            if sar.rx_cum_ack_index > 0 {
                                sar.rx_cum_ack_index = 0;
                                let seq = sar.rx_cum_ack_seq;
                                sar.send_ack(seq);
                            }
                        }
                    });
                })
                .await;
            },
            self.tk_handle.clone(),
        ));
    }

    fn stop_cum_ack_timer(&mut self) {
        if let Some(h) = self.rx_cum_ack_timer.take() {
            h.abort();
        }
        self.rx_cum_ack_index = 0;
    }

    fn start_timeout_checker(&self, device: String) -> TaskHandle {
        let handle = self.tk_handle.clone();
        spawn_with_handle(
            async move {
                loop {
                    sleep(Duration::from_millis(500)).await;
                    let dev_id = device.clone();
                    crate::ecs::with_rt_mut(move |rt| {
                        let _ = rt.with_device_mut(&dev_id, |world, entity| {
                            if let Some(dev) = world.get_mut::<super::XiaomiDevice>(entity) {
                                dev.sar.lock().check_timeouts_internal();
                            }
                        });
                    })
                    .await;
                }
            },
            handle,
        )
    }

    fn check_timeouts_internal(&mut self) {
        let now = Instant::now();

        if !self.cmd_exchanged {
            if let Some(deadline) = self.start_req_deadline {
                if now >= deadline && self.start_req_resends < MAX_START_REQ_RESENDS {
                    self.start_req_resends += 1;
                    log::warn!(
                        "[SarController] No L1StartRsp from {} within {}ms, resending L1StartReq ({}/{})",
                        self.device_id,
                        self.send_timeout.as_millis(),
                        self.start_req_resends,
                        MAX_START_REQ_RESENDS
                    );
                    self.send_start_req();
                } else if now >= deadline {
                    log::warn!(
                        "[SarController] Device {} never answered L1StartReq; giving up on the handshake",
                        self.device_id
                    );
                    self.start_req_deadline = None;
                }
            }
        }

        // 对齐官方：只有队头（最老的待确认帧）超时才算数，超时就把窗口内所有待确认帧整体重传
        let Some(base) = self.tx_queue.front() else {
            return;
        };
        if !base.wait_ack || now < base.deadline {
            return;
        }
        if base.retries >= MAX_PACKET_RETRIES {
            log::warn!(
                "[SarController] Packet seq={} on device {} exceeded max retries ({}); L1 session is out of sync",
                base.packet.seq,
                self.device_id,
                MAX_PACKET_RETRIES
            );
            // 只清队列而不重新握手的话，本端序号已经前进、设备还停在丢的那一帧上，
            // 之后发出的每一帧都会被设备当成乱序帧拒收，直到重连为止。
            self.abort_all_pending(crate::anyhow_site!(
                "SAR retransmission limit exceeded: device stopped acknowledging (timed out)"
            ));
            self.restart_session();
            return;
        }
        for item in self.tx_queue.iter_mut() {
            if item.wait_ack {
                item.wait_ack = false;
                item.need_retransmission = true;
            }
        }
        if let Some(base) = self.tx_queue.front_mut() {
            base.retries = base.retries.saturating_add(1);
        }
        self.try_run_next();
    }

    /// 丢弃所有在途/排队的数据并让等待者立刻失败。序号状态留到下次真正发数据时随会话重启一起归零，
    /// 这样链路已断时不会反复发 L1StartReq。
    pub fn abort_all_pending(&mut self, err: anyhow::Error) {
        log::warn!(
            "[SarController] Aborting all pending packets for {}: {err:#}",
            self.device_id
        );
        self.tx_queue.clear();
        self.command_pool = CommandPool::new();
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.needs_restart = true;
        self.ack_notify.notify_waiters();

        let device_id = self.device_id.clone();
        let handle = self.tk_handle.clone();
        spawn_with_handle(
            async move {
                crate::device::xiaomi::fail_all_device_slots(&device_id, err).await;
            },
            handle,
        );
    }

    async fn on_send_failure(device_id: &str, err: super::SendError) {
        log::warn!(
            "[SarController] Transport send failed for {}: {:?}",
            device_id,
            err
        );
        let dev_id = device_id.to_string();
        crate::ecs::with_rt_mut(move |rt| {
            let _ = rt.with_device_mut(&dev_id, |world, entity| {
                if let Some(dev) = world.get_mut::<super::XiaomiDevice>(entity) {
                    dev.sar.lock().abort_all_pending(crate::anyhow_site!(
                        "Transport send failed: {err:?}"
                    ));
                }
            });
        })
        .await;
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
                if let Some(cmd) = L1CmdPacket::from_payload_bytes(&l1.payload) {
                    match cmd.cmd {
                        CmdCode::CmdL1startRsp => self.handle_start_rsp(&cmd),
                        CmdCode::CmdL1startReq => self.handle_peer_start_req(&cmd),
                        _ => {}
                    }
                }
                false
            }
            L1DataType::Data => {
                let channel = l1.payload.get(0).and_then(|b| L2Channel::try_from(*b).ok());

                let ackable = self.cmd_exchanged
                    && !matches!(channel, Some(L2Channel::Network | L2Channel::MultiModal));
                if !ackable {
                    return true;
                }

                if l1.frx {
                    // 快速接收(frx)不需要 ACK，也不占用顺序序号空间——与官方一致不推进
                    // rx_expect_seq，否则一旦设备走 frx，期望序号会与设备永久错位致后续误 NAK。
                    return true;
                }

                if l1.seq != self.rx_expect_seq {
                    // onDataReceive：(seq - expect) 无符号 >= 128 视为落后
                    let ahead = l1.seq.wrapping_sub(self.rx_expect_seq) < 128;
                    if ahead {
                        // 超前说明中间丢了帧，请设备从期望序号开始重传
                        self.send_nak(self.rx_expect_seq);
                    } else if self.rx_in_order_seen {
                        // 落后（重复）帧多半是我们的 ACK 丢了、设备还在重传。补一个累积 ACK
                        // 让它停下；不回 NAK，免得每个重复帧都触发一次整窗重传。
                        self.send_ack(self.rx_expect_seq.wrapping_sub(1));
                    }
                    return false;
                }

                let immediate = u32::from(self.rx_cum_ack_index)
                    >= (u32::from(self.raw_tx_window_size()) * 2 / 3)
                    || matches!(channel, Some(L2Channel::Pb | L2Channel::Lyra));
                if immediate {
                    self.stop_cum_ack_timer();
                    self.send_ack(l1.seq);
                } else {
                    self.rx_cum_ack_index = self.rx_cum_ack_index.saturating_add(1);
                    self.rx_cum_ack_seq = l1.seq;
                    self.start_cum_ack_timer(self.device_id.clone());
                }

                self.rx_expect_seq = self.rx_expect_seq.wrapping_add(1);
                self.rx_in_order_seen = true;
                true
            }
        }
    }

    fn handle_start_rsp(&mut self, cmd: &L1CmdPacket) {
        self.cmd_exchanged = true;
        self.start_req_deadline = None;
        if let Some(win) = cmd.get_tx_win() {
            self.tx_win = win.clamp(1, u16::from(u8::MAX)) as u8;
        }
        // 两端官方客户端都用自己的发送超时，设备回的值只记日志
        let remote_send_timeout = cmd.get_send_timeout();
        log::info!(
            "[SarController] L1StartRsp applied: local_send_win={} remote_tx_win={} send_timeout_ms={} remote_send_timeout_ms={:?}",
            self.effective_tx_win(),
            self.tx_win,
            self.send_timeout.as_millis(),
            remote_send_timeout
        );
        self.profiler.record(
            "sar",
            "l1start_rsp",
            None,
            None,
            None,
            None,
            Some(true),
            Some(format!(
                "local_send_win={},remote_tx_win={},send_timeout_ms={},remote_send_timeout_ms={:?}",
                self.effective_tx_win(),
                self.tx_win,
                self.send_timeout.as_millis(),
                remote_send_timeout
            )),
        );
    }

    /// 设备主动发起 L1Start：官方收到后 clearTranCtl 再回 L1StartRsp。会话中途收到说明
    /// 设备侧已经清空了会话，本端在途数据作废、序号归零；以前这里直接忽略，设备就再也收不到回包。
    fn handle_peer_start_req(&mut self, cmd: &L1CmdPacket) {
        if self.cmd_exchanged {
            log::warn!(
                "[SarController] Device {} restarted the L1 session",
                self.device_id
            );
            self.abort_all_pending(crate::anyhow_site!(
                "device restarted the L1 session; pending transfer dropped"
            ));
            self.reset_sequence_state();
        }
        if let Some(win) = cmd.get_tx_win() {
            self.tx_win = win.clamp(1, u16::from(u8::MAX)) as u8;
        }
        self.needs_restart = false;
        self.cmd_exchanged = true;
        self.start_req_deadline = None;
        let rsp = self.build_start_cmd(CmdCode::CmdL1startRsp);
        self.command_pool.push_cmd_front(rsp);
        self.try_run_next();
    }

    /// 把 seq 及之前的待确认帧出队，返回是否有推进
    fn ack_through(&mut self, seq: u8) -> bool {
        let mut advanced = false;
        while let Some(item) = self.tx_queue.front() {
            if Self::seq_le(item.packet.seq, seq) {
                let seq_val = item.packet.seq;
                self.acked.insert(seq_val);
                self.tx_queue.pop_front();
                self.tx_base = self.tx_base.wrapping_add(1);
                advanced = true;
            } else {
                break;
            }
        }
        if advanced {
            self.profiler.record(
                "sar",
                "ack_received",
                None,
                None,
                None,
                Some(u32::from(seq)),
                Some(true),
                Some("window_advanced".to_string()),
            );
            self.ack_notify.notify_waiters();
        }
        advanced
    }

    fn handle_ack(&mut self, seq: u8) {
        self.ack_through(seq);
        self.try_run_next();
    }

    fn handle_nak(&mut self, seq: u8) {
        self.profiler.record(
            "sar",
            "nak_received",
            None,
            None,
            None,
            Some(u32::from(seq)),
            Some(false),
            None,
        );
        // NAK(seq) 同时意味着 seq 之前的都已收到
        self.ack_through(seq.wrapping_sub(1));

        // 一处缺帧时设备会对后面每个乱序帧都回同一个 NAK，补发已在路上就不再重复整窗重传
        let now = Instant::now();
        let holdoff = self.send_timeout / 4;
        if let Some((last_seq, at)) = self.last_nak_retransmit {
            if last_seq == seq && now.duration_since(at) < holdoff {
                return;
            }
        }

        let Some(base) = self.tx_queue.front() else {
            return;
        };
        let base_covered = Self::seq_le(seq, base.packet.seq);
        if base_covered && base.retries >= MAX_PACKET_RETRIES {
            // 设备反复索要一帧我们早已不在窗口里的数据（或一直收不到），会话已失步。
            // 每次 NAK 补发都会刷新超时，光靠超时永远等不到重启，所以这里也要计数。
            log::warn!(
                "[SarController] Device {} keeps NAKing seq={} (base seq={}); L1 session is out of sync",
                self.device_id,
                seq,
                base.packet.seq
            );
            self.abort_all_pending(crate::anyhow_site!(
                "SAR retransmission limit exceeded: device keeps rejecting packets (timed out)"
            ));
            self.restart_session();
            return;
        }

        for item in self.tx_queue.iter_mut() {
            if Self::seq_le(seq, item.packet.seq) {
                item.need_retransmission = true;
                item.wait_ack = false;
            }
        }
        if base_covered {
            if let Some(base) = self.tx_queue.front_mut() {
                base.retries = base.retries.saturating_add(1);
            }
        }
        self.last_nak_retransmit = Some((seq, now));
        self.try_run_next();
    }

    fn seq_le(a: u8, b: u8) -> bool {
        b.wrapping_sub(a) < 128
    }

    fn try_run_next(&mut self) {
        let now = Instant::now();
        let deadline = now + self.send_timeout;
        let mut frames = Vec::new();

        // 先发命令（L1StartReq/Rsp），不受窗口限制
        let mut cmd_count = 0u32;
        while let Some(cmd) = self.command_pool.pop_cmd() {
            frames.push(L1Packet::new(L1DataType::Cmd, false, 0, cmd).to_bytes());
            cmd_count += 1;
        }

        // 再按序号顺序把所有被标记的帧一次补发（以前一次只补一帧，剩下的要等下一轮才发）
        let mut retransmit_count = 0u32;
        for item in self.tx_queue.iter_mut() {
            if item.need_retransmission {
                item.need_retransmission = false;
                item.wait_ack = true;
                item.deadline = deadline;
                frames.push(item.packet.to_bytes());
                retransmit_count += 1;
            }
        }

        // 最后在窗口内放新数据
        let mut data_count = 0u32;
        while self.tx_queue.len() < usize::from(self.effective_tx_win()) {
            let Some(qd) = self.command_pool.pop_data() else {
                break;
            };
            let pkt = L1Packet::new(L1DataType::Data, false, qd.seq, qd.payload);
            frames.push(pkt.to_bytes());
            self.tx_queue.push_back(SendItem {
                packet: pkt,
                wait_ack: true,
                need_retransmission: false,
                deadline,
                retries: 0,
            });
            data_count += 1;
        }

        if frames.is_empty() {
            return;
        }

        let total_bytes = frames.iter().map(|pkt| pkt.len() as u64).sum::<u64>();
        if retransmit_count > 0 {
            self.profiler.record(
                "sar",
                "data_retransmit",
                None,
                Some(retransmit_count),
                None,
                None,
                None,
                None,
            );
        }
        if cmd_count > 0 {
            self.profiler.record(
                "sar",
                "cmd_batch_send",
                None,
                Some(cmd_count),
                None,
                None,
                Some(true),
                None,
            );
        }
        if data_count > 0 {
            self.profiler.record(
                "sar",
                "data_batch_send",
                None,
                Some(data_count),
                Some(total_bytes),
                None,
                Some(true),
                Some(format!(
                    "soft_window={},raw_window={}",
                    self.effective_tx_win(),
                    self.raw_tx_window_size()
                )),
            );
        }
        self.outbound.send(frames);
    }
}

impl Drop for SarController {
    fn drop(&mut self) {
        if let Some(h) = self.timeout_checker.take() {
            h.abort();
        }
        if let Some(h) = self.rx_cum_ack_timer.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn recording_sender() -> (SendFn, Arc<Mutex<Vec<Vec<u8>>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = sent.clone();
        let sender: SendFn = Arc::new(move |frames| {
            sink.lock().unwrap().extend(frames);
            Box::pin(async { Ok(()) })
        });
        (sender, sent)
    }

    fn new_controller(device_id: &str, link: SarLinkParams) -> (SarController, Arc<Mutex<Vec<Vec<u8>>>>) {
        let (sender, sent) = recording_sender();
        let sar = SarController::new(
            tokio::runtime::Handle::current(),
            sender,
            device_id.to_string(),
            TransportProfilerHandle::new(),
            SarConfig::default(),
            link,
        );
        (sar, sent)
    }

    async fn flushed(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> Vec<L1Packet> {
        // 让有序发送任务把队列写完
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        sent.lock()
            .unwrap()
            .iter()
            .map(|bytes| L1Packet::from_bytes(bytes).unwrap())
            .collect()
    }

    fn start_rsp() -> L1Packet {
        let payload = L1CmdBuilder::new()
            .cmd(CmdCode::CmdL1startRsp)
            .tx_win(16)
            .build()
            .unwrap()
            .to_payload_bytes();
        L1Packet::new(L1DataType::Cmd, false, 0, payload)
    }

    #[test]
    fn ble_link_params_follow_official_mtu_tiers() {
        assert_eq!(SarLinkParams::ble(182), SarLinkParams { tx_win: 4, send_timeout_ms: 6000 });
        assert_eq!(SarLinkParams::ble(244), SarLinkParams { tx_win: 8, send_timeout_ms: 9000 });
        assert_eq!(SarLinkParams::ble(400), SarLinkParams { tx_win: 12, send_timeout_ms: 12000 });
        assert_eq!(SarLinkParams::ble(512), SarLinkParams { tx_win: 16, send_timeout_ms: 15000 });
        assert_eq!(SarLinkParams::ble(0), SarLinkParams::ble(182));
        assert_eq!(SarLinkParams::ble(600), SarLinkParams::ble(182));
    }

    #[tokio::test]
    async fn start_req_advertises_link_params() {
        let (_sar, sent) = new_controller("test_start_req", SarLinkParams::ble(244));
        let packets = flushed(&sent).await;
        let cmd = L1CmdPacket::from_payload_bytes(&packets[0].payload).unwrap();
        assert_eq!(cmd.cmd, CmdCode::CmdL1startReq);
        assert_eq!(cmd.get_tx_win(), Some(8));
        assert_eq!(cmd.get_send_timeout(), Some(9000));
    }

    #[tokio::test]
    async fn send_window_follows_link_params() {
        let (mut sar, _sent) = new_controller("test_window", SarLinkParams::ble(244));
        sar.enqueue_batch((0..20u8).map(|i| vec![L2Channel::Pb as u8, i]));
        assert_eq!(sar.tx_queue.len(), 8);
    }

    #[tokio::test]
    async fn frames_are_written_in_sequence_order() {
        let (mut sar, sent) = new_controller("test_order", SarLinkParams::SPP);
        for i in 0..40u8 {
            sar.enqueue(vec![L2Channel::Pb as u8, i]);
        }
        // 设备确认前 10 帧，窗口滑动后再补发新的
        sar.on_l1_packet(&L1Packet::new(L1DataType::Ack, false, 9, vec![]));
        let seqs: Vec<u8> = flushed(&sent)
            .await
            .into_iter()
            .filter(|p| p.pkt_type == L1DataType::Data)
            .map(|p| p.seq)
            .collect();
        assert_eq!(seqs, (0..40u8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn base_timeout_retransmits_whole_window_in_order() {
        let (mut sar, sent) = new_controller("test_go_back_n", SarLinkParams::ble(244));
        sar.enqueue_batch((0..4u8).map(|i| vec![L2Channel::Pb as u8, i]));
        flushed(&sent).await;
        sent.lock().unwrap().clear();

        sar.tx_queue[0].deadline = Instant::now() - Duration::from_secs(1);
        sar.check_timeouts_internal();
        let seqs: Vec<u8> = flushed(&sent).await.into_iter().map(|p| p.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
        assert_eq!(sar.tx_queue[0].retries, 1);
    }

    #[tokio::test]
    async fn max_retries_exceeded_restarts_l1_session() {
        let (mut sar, sent) = new_controller("test_device", SarLinkParams::SPP);
        sar.on_l1_packet(&start_rsp());

        // 先推进几个序号，模拟会话进行中
        for i in 0..3u8 {
            sar.enqueue(vec![L2Channel::Pb as u8, i]);
        }
        sar.on_l1_packet(&L1Packet::new(L1DataType::Ack, false, 2, vec![]));
        let seq = sar.enqueue(vec![L2Channel::Pb as u8, 0x10]);
        assert_eq!(seq, 3);
        let epoch = sar.session_epoch();

        for expected_retries in 1..=MAX_PACKET_RETRIES {
            sar.tx_queue[0].deadline = Instant::now() - Duration::from_secs(1);
            sar.check_timeouts_internal();
            assert_eq!(sar.tx_queue[0].retries, expected_retries);
            assert!(sar.tx_queue[0].wait_ack);
            assert!(!sar.tx_queue[0].need_retransmission);
        }

        flushed(&sent).await;
        sent.lock().unwrap().clear();

        // 第 4 次超时：丢弃在途数据并重新握手，序号从 0 重新开始
        sar.tx_queue[0].deadline = Instant::now() - Duration::from_secs(1);
        sar.check_timeouts_internal();
        assert_eq!(sar.tx_queue.len(), 0);
        assert_ne!(sar.session_epoch(), epoch);

        let packets = flushed(&sent).await;
        let cmd = L1CmdPacket::from_payload_bytes(&packets[0].payload).unwrap();
        assert_eq!(cmd.cmd, CmdCode::CmdL1startReq);
        assert_eq!(sar.enqueue(vec![L2Channel::Pb as u8, 0x20]), 0);
    }

    #[tokio::test]
    async fn send_failure_restarts_session_on_next_enqueue() {
        let (mut sar, sent) = new_controller("test_send_failure", SarLinkParams::SPP);
        sar.on_l1_packet(&start_rsp());
        sar.enqueue(vec![L2Channel::Pb as u8, 1]);
        sar.abort_all_pending(crate::anyhow_site!("simulated send failure"));
        flushed(&sent).await;
        sent.lock().unwrap().clear();

        assert_eq!(sar.enqueue(vec![L2Channel::Pb as u8, 2]), 0);
        let packets = flushed(&sent).await;
        assert_eq!(packets[0].pkt_type, L1DataType::Cmd);
        assert_eq!(packets[1].pkt_type, L1DataType::Data);
        assert_eq!(packets[1].seq, 0);
    }

    #[tokio::test]
    async fn peer_start_req_mid_session_resets_and_replies() {
        let (mut sar, sent) = new_controller("test_peer_start", SarLinkParams::SPP);
        sar.on_l1_packet(&start_rsp());
        sar.enqueue(vec![L2Channel::Pb as u8, 1]);
        let data = L1Packet::new(L1DataType::Data, false, 0, vec![L2Channel::Pb as u8, 0]);
        assert!(sar.on_l1_packet(&data));
        flushed(&sent).await;
        sent.lock().unwrap().clear();

        let req = L1CmdBuilder::new()
            .cmd(CmdCode::CmdL1startReq)
            .tx_win(8)
            .build()
            .unwrap()
            .to_payload_bytes();
        sar.on_l1_packet(&L1Packet::new(L1DataType::Cmd, false, 0, req));

        assert_eq!(sar.tx_queue.len(), 0);
        let packets = flushed(&sent).await;
        let cmd = L1CmdPacket::from_payload_bytes(&packets[0].payload).unwrap();
        assert_eq!(cmd.cmd, CmdCode::CmdL1startRsp);
        // 新会话从 0 开始收发
        let data = L1Packet::new(L1DataType::Data, false, 0, vec![L2Channel::Pb as u8, 0]);
        assert!(sar.on_l1_packet(&data));
        assert_eq!(sar.enqueue(vec![L2Channel::Pb as u8, 2]), 0);
    }

    #[tokio::test]
    async fn duplicate_data_is_reacknowledged_without_nak() {
        let (mut sar, sent) = new_controller("test_dup", SarLinkParams::SPP);
        sar.on_l1_packet(&start_rsp());
        let data = L1Packet::new(L1DataType::Data, false, 0, vec![L2Channel::Pb as u8, 0]);
        assert!(sar.on_l1_packet(&data));
        flushed(&sent).await;
        sent.lock().unwrap().clear();

        // 设备没收到 ACK 又重传了 seq 0
        assert!(!sar.on_l1_packet(&data));
        let packets = flushed(&sent).await;
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].pkt_type, L1DataType::Ack);
        assert_eq!(packets[0].seq, 0);
    }

    #[tokio::test]
    async fn missing_start_rsp_resends_start_req() {
        let (mut sar, sent) = new_controller("test_start_resend", SarLinkParams::SPP);
        flushed(&sent).await;
        sent.lock().unwrap().clear();

        sar.start_req_deadline = Some(Instant::now() - Duration::from_secs(1));
        sar.check_timeouts_internal();
        let packets = flushed(&sent).await;
        let cmd = L1CmdPacket::from_payload_bytes(&packets[0].payload).unwrap();
        assert_eq!(cmd.cmd, CmdCode::CmdL1startReq);

        // 重发有上限，之后不再打扰设备
        for _ in 1..MAX_START_REQ_RESENDS {
            sar.start_req_deadline = Some(Instant::now() - Duration::from_secs(1));
            sar.check_timeouts_internal();
        }
        flushed(&sent).await;
        sent.lock().unwrap().clear();
        sar.start_req_deadline = Some(Instant::now() - Duration::from_secs(1));
        sar.check_timeouts_internal();
        assert!(flushed(&sent).await.is_empty());
        assert!(sar.start_req_deadline.is_none());
    }

    #[tokio::test]
    async fn repeated_nak_for_same_gap_retransmits_once() {
        let (mut sar, sent) = new_controller("test_nak_dedup", SarLinkParams::ble(244));
        sar.on_l1_packet(&start_rsp());
        sar.enqueue_batch((0..6u8).map(|i| vec![L2Channel::Pb as u8, i]));
        flushed(&sent).await;
        sent.lock().unwrap().clear();

        // seq 2 丢了，设备对 3/4/5 各回一次 NAK(2)
        for _ in 0..3 {
            sar.on_l1_packet(&L1Packet::new(L1DataType::Nak, false, 2, vec![]));
        }
        let seqs: Vec<u8> = flushed(&sent).await.into_iter().map(|p| p.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4, 5]);
        assert_eq!(sar.tx_queue.front().map(|i| i.packet.seq), Some(2));
    }

    #[tokio::test]
    async fn endless_nak_for_base_restarts_session() {
        let (mut sar, sent) = new_controller("test_nak_storm", SarLinkParams::SPP);
        sar.on_l1_packet(&start_rsp());
        sar.enqueue(vec![L2Channel::Pb as u8, 1]);
        let epoch = sar.session_epoch();

        for _ in 0..=MAX_PACKET_RETRIES {
            sar.last_nak_retransmit = None;
            sar.on_l1_packet(&L1Packet::new(L1DataType::Nak, false, 0, vec![]));
        }
        assert_ne!(sar.session_epoch(), epoch);
        assert_eq!(sar.tx_queue.len(), 0);
        flushed(&sent).await;
    }

    #[tokio::test]
    async fn abort_all_pending_notifies_ack_waiter() {
        let (mut sar, _sent) = new_controller("test_device_2", SarLinkParams::SPP);

        let notifier = sar.ack_notifier();
        let waiter = notifier.notified();

        sar.abort_all_pending(crate::anyhow_site!("test abort"));

        // Waiter must be notified immediately
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("ack_notifier was not notified upon abort");
    }
}
