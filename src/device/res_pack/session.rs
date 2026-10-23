//! Corona CanopusConnect sender session state machine and streaming CRPack installer.
//!
//! Implements:
//! - `Hello -> Open -> [Windowed DATA loop] -> Finish -> Poll Query -> Installed/Error`
//! - Dual-window control: `sendOffset <= min(size, ackedOffset + windowFrames * chunkBytes)`
//!   and `inflightRpc <= negotiatedMaxRpc`.
//! - CPC2 completion only releases an RPC slot; cumulative ACK (`0x83`) or `QUERY` response
//!   advances `ackedOffset`.
//! - Streaming file reader: reads 468-byte slices on demand from the local `.crpack` file and
//!   computes SHA-256 via a fixed 4 KiB buffer without copying the archive into memory.
//! - Recovery from ACK loss (pause window, drain in-flight RPCs, send `QUERY`), RPC timeout or
//!   disconnect (drain/abort, reconnect/listen -> `HELLO` -> `OPEN` with a fresh lease token,
//!   resume from device-reported `writtenOffset`).

use std::{
    collections::HashSet,
    fs::File,
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::Path,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, anyhow, bail, ensure};
use sha2::{Digest, Sha256};
use tokio::{
    sync::broadcast,
    task::JoinSet,
    time::{Instant, sleep, timeout_at},
};

use crate::{
    device::{
        canopus::connect::{ConnectPayload, ConnectWindowSender},
        crpack::inspect_crpack,
    },
    events::CoreEvent,
};

use super::{
    InstalledResourcePack, ResourcePackComponent, ResourcePackProgress, ResumeToken,
    proto::{
        CORONA_MODULE_ID, ControlResponse, CoronaCode, CoronaOp, CoronaPhase, DEFAULT_MAX_RPC,
        HelloCaps, LIST_END_INDEX, MAX_PAYLOAD_BYTES, MAX_SUPPORTED_RPC,
        PROTOCOL_VERSION, ProgressState, decode_ack_frame, decode_control_response,
        decode_hello_response, decode_list_response, decode_open_response, encode_control_request,
        encode_data_frame, encode_hello_request, encode_list_request, encode_open_request,
    },
};

const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_ACK_WAIT: Duration = Duration::from_secs(1);
const DEFAULT_FINISH_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RECOVERY_ROUNDS: usize = 6;
const MAX_FINISH_POLLS: usize = 120;
const MAX_LIST_REVISIONS: usize = 4;

pub type RpcFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    /// Host upper bound for concurrent DATA RPCs (`1..=3`, default 2).
    pub host_max_rpc: u8,
    pub control_timeout: Duration,
    pub data_rpc_timeout: Duration,
    pub ack_wait: Duration,
    pub finish_poll_interval: Duration,
    pub max_recovery_rounds: usize,
    pub max_finish_polls: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        let host_max_rpc = std::env::var("ASTROBOX_CORONA_MAX_RPC")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .map(|v| v.clamp(1, MAX_SUPPORTED_RPC))
            .unwrap_or(DEFAULT_MAX_RPC);
        Self {
            host_max_rpc,
            control_timeout: DEFAULT_CONTROL_TIMEOUT,
            data_rpc_timeout: DEFAULT_CONTROL_TIMEOUT,
            ack_wait: DEFAULT_ACK_WAIT,
            finish_poll_interval: DEFAULT_FINISH_POLL_INTERVAL,
            max_recovery_rounds: MAX_RECOVERY_ROUNDS,
            max_finish_polls: MAX_FINISH_POLLS,
        }
    }
}

/// Abstract transport for Corona CanopusConnect binary frames so the full state machine
/// can run both against a live Canopus device and against deterministic host mocks.
pub trait CoronaTransport: Send {
    fn connect_and_listen<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn set_max_rpc(&mut self, max_rpc: usize);

    fn send_control<'a>(
        &'a mut self,
        frame: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn submit_data<'a>(
        &'a mut self,
        offset: u32,
        frame: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RpcFuture>> + Send + 'a>>;

    fn recv_frame<'a>(
        &'a mut self,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send + 'a>>;

    fn close<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

/// Live CanopusConnect transport bound to an ECS device entity.
pub struct LiveCoronaTransport {
    addr: String,
    session: Arc<tokio::sync::Mutex<()>>,
    events: broadcast::Receiver<CoreEvent>,
    sender: ConnectWindowSender,
    subscribed: bool,
}

impl LiveCoronaTransport {
    pub fn new(
        addr: String,
        session: Arc<tokio::sync::Mutex<()>>,
        initial_max_rpc: usize,
    ) -> anyhow::Result<Self> {
        let sender = ConnectWindowSender::new(addr.clone(), CORONA_MODULE_ID, initial_max_rpc)
            .map_err(|e| anyhow!(e))?;
        Ok(Self {
            addr,
            session,
            events: crate::events::subscribe(),
            sender,
            subscribed: false,
        })
    }

    async fn ensure_current_session(&self) -> anyhow::Result<()> {
        let addr = self.addr.clone();
        let session = self.session.clone();
        let current = crate::ecs::with_rt_mut(move |rt| {
            rt.component_ref::<ResourcePackComponent>(&addr)
                .is_some_and(|comp| Arc::ptr_eq(&comp.session, &session))
        })
        .await;
        ensure!(
            current,
            "device_not_connected: resource pack connection changed"
        );
        Ok(())
    }
}

impl CoronaTransport for LiveCoronaTransport {
    fn connect_and_listen<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.ensure_current_session().await?;
            self.events = crate::events::subscribe();
            if !self.subscribed {
                crate::device::canopus::connect::subscribe(&self.addr, CORONA_MODULE_ID)
                    .await
                    .map_err(|e| anyhow!(e))?;
                self.subscribed = true;
            }
            Ok(())
        })
    }

    fn set_max_rpc(&mut self, max_rpc: usize) {
        if let Ok(sender) = ConnectWindowSender::new(self.addr.clone(), CORONA_MODULE_ID, max_rpc) {
            self.sender = sender;
        }
    }

    fn send_control<'a>(
        &'a mut self,
        frame: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.ensure_current_session().await?;
            self.sender
                .send_control(ConnectPayload::Binary(frame), timeout)
                .await
                .map_err(|e| anyhow!(e))
        })
    }

    fn submit_data<'a>(
        &'a mut self,
        _offset: u32,
        frame: Vec<u8>,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<RpcFuture>> + Send + 'a>> {
        Box::pin(async move {
            self.ensure_current_session().await?;
            let fut = self
                .sender
                .submit_ordered(ConnectPayload::Binary(frame), timeout)
                .await
                .map_err(|e| anyhow!(e))?;
            Ok(Box::pin(async move { fut.await.map_err(|e| anyhow!(e)) }) as RpcFuture)
        })
    }

    fn recv_frame<'a>(
        &'a mut self,
        deadline: Instant,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send + 'a>> {
        Box::pin(async move {
            loop {
                let event = match timeout_at(deadline, self.events.recv()).await {
                    Err(_) => return Ok(None),
                    Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                    Ok(result) => result.context("CanopusConnect event bus closed")?,
                };
                match event {
                    CoreEvent::DeviceStateChanged(change) if change.device_addr == self.addr => {
                        self.ensure_current_session().await?;
                    }
                    CoreEvent::SharedLinkDetected(shared) if shared.device_addr == self.addr => {
                        bail!("device_not_connected: shared BLE link detected");
                    }
                    CoreEvent::CanopusConnectMessage(msg)
                        if msg.device_addr == self.addr && msg.module == CORONA_MODULE_ID =>
                    {
                        self.ensure_current_session().await?;
                        if let ConnectPayload::Binary(bytes) = msg.payload {
                            return Ok(Some(bytes));
                        }
                    }
                    _ => {}
                }
            }
        })
    }

    fn close<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if self.subscribed {
                self.subscribed = false;
                let _ =
                    crate::device::canopus::connect::unsubscribe(&self.addr, CORONA_MODULE_ID)
                        .await;
            }
        })
    }
}

/// Prepared local `.crpack` metadata computed by streaming without buffering the full archive.
#[derive(Debug, Clone)]
pub struct PreparedArchive {
    pub theme_id: String,
    pub archive_bytes: u32,
    pub unpacked_bytes: u32,
    pub sha256: [u8; 32],
    pub digest_hex: String,
}

/// Validates the local `.crpack` structure and computes its full-archive SHA-256 using
/// a fixed 4 KiB streaming buffer. Peak RAM does not grow with archive size.
pub fn prepare_archive_streaming(path: &Path) -> anyhow::Result<PreparedArchive> {
    let summary = inspect_crpack(path)?;
    let mut file = File::open(path).context("failed to open CRPack file")?;
    let meta_len = file.metadata().context("failed to stat CRPack")?.len();
    ensure!(
        meta_len > 0 && meta_len <= u64::from(super::proto::MAX_ARCHIVE_BYTES_LIMIT),
        "CRPack archive size {meta_len} is outside 1..={} bytes",
        super::proto::MAX_ARCHIVE_BYTES_LIMIT
    );
    let archive_bytes = u32::try_from(meta_len).context("CRPack archive too large")?;
    let unpacked_bytes =
        u32::try_from(summary.total_unpacked_bytes).context("CRPack unpacked bytes too large")?;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 4096];
    let mut read_total = 0u64;
    loop {
        let n = file.read(&mut buf).context("failed to read CRPack")?;
        if n == 0 {
            break;
        }
        read_total += n as u64;
        hasher.update(&buf[..n]);
    }
    ensure!(
        read_total == meta_len,
        "CRPack size changed while hashing ({read_total} != {meta_len})"
    );
    let sha256: [u8; 32] = hasher.finalize().into();
    let digest_hex = hex::encode(sha256);

    Ok(PreparedArchive {
        theme_id: summary.theme_id,
        archive_bytes,
        unpacked_bytes,
        sha256,
        digest_hex,
    })
}

/// Pure window state tracker for a single Corona lease (`token`).
#[derive(Debug, Clone)]
pub struct WindowTracker {
    pub token: u32,
    pub archive_bytes: u32,
    pub chunk_bytes: u16,
    pub window_frames: u8,
    pub ack_every: u8,
    pub max_rpc: usize,
    pub acked_offset: u32,
    pub durable_offset: u32,
    pub send_offset: u32,
    pub inflight_rpc: usize,
    pub phase: CoronaPhase,
    pub detail: CoronaCode,
}

impl WindowTracker {
    pub fn new(caps: &HelloCaps, progress: &ProgressState, archive_bytes: u32, host_max_rpc: u8) -> Self {
        let window_frames = if progress.window_frames > 0 {
            progress.window_frames
        } else {
            caps.window_frames
        };
        let ack_every = if progress.ack_every > 0 {
            progress.ack_every
        } else {
            caps.ack_every
        };
        Self {
            token: progress.token,
            archive_bytes,
            chunk_bytes: caps.chunk_bytes,
            window_frames,
            ack_every,
            max_rpc: caps.effective_max_rpc(host_max_rpc),
            acked_offset: progress.written_offset,
            durable_offset: progress.durable_offset,
            send_offset: progress.written_offset,
            inflight_rpc: 0,
            phase: progress.phase,
            detail: progress.detail,
        }
    }

    /// Upper bound on `send_offset` permitted by the current business byte window:
    /// `min(archiveBytes, ackedOffset + windowFrames * chunkBytes)`.
    pub fn window_limit(&self) -> u32 {
        let budget = u32::from(self.window_frames) * u32::from(self.chunk_bytes);
        self.acked_offset
            .saturating_add(budget)
            .min(self.archive_bytes)
    }

    pub fn can_send_next(&self) -> bool {
        self.phase == CoronaPhase::Receiving
            && self.inflight_rpc < self.max_rpc
            && self.send_offset < self.window_limit()
    }

    pub fn next_chunk_range(&self) -> Option<(u32, usize)> {
        if !self.can_send_next() {
            return None;
        }
        let remaining = (self.archive_bytes - self.send_offset) as usize;
        let len = remaining.min(usize::from(self.chunk_bytes));
        Some((self.send_offset, len))
    }

    pub fn record_sent(&mut self, len: usize) {
        self.send_offset += len as u32;
        self.inflight_rpc += 1;
    }

    pub fn record_rpc_completed(&mut self) {
        self.inflight_rpc = self.inflight_rpc.saturating_sub(1);
    }

    /// Applies a validated cumulative ACK or QUERY progress update for the same `token`.
    /// Old/smaller or duplicate `written_offset` values do not move `acked_offset` backwards
    /// or add duplicate window credit.
    pub fn apply_progress(&mut self, progress: &ProgressState) -> anyhow::Result<bool> {
        ensure!(
            progress.token == self.token,
            "Corona progress token mismatch: expected {}, got {}",
            self.token,
            progress.token
        );
        ensure!(
            progress.written_offset <= self.send_offset
                && progress.written_offset <= self.archive_bytes,
            "Corona writtenOffset ({}) exceeds sent ({}) or archiveBytes ({})",
            progress.written_offset,
            self.send_offset,
            self.archive_bytes
        );
        self.phase = progress.phase;
        self.detail = progress.detail;
        if progress.window_frames > 0 {
            self.window_frames = progress.window_frames;
        }
        if progress.ack_every > 0 {
            self.ack_every = progress.ack_every;
        }
        self.durable_offset = progress.durable_offset;
        if progress.written_offset > self.acked_offset {
            self.acked_offset = progress.written_offset;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

struct SeqAlloc {
    next: u16,
}

impl SeqAlloc {
    fn new() -> Self {
        Self { next: 1 }
    }

    /// Returns the next control request sequence number, or `None` if the u16 space
    /// is exhausted within the current lease (requiring an OPEN lease rotation).
    fn next(&mut self) -> Option<u16> {
        if self.next == 0 {
            return None;
        }
        let seq = self.next;
        self.next = self.next.wrapping_add(1);
        Some(seq)
    }
}

fn new_open_nonce() -> [u8; 8] {
    let bytes = crate::tools::generate_random_bytes(8);
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

fn emit_phase_progress(
    progress_cb: &(impl Fn(ResourcePackProgress) + Send),
    written_offset: u32,
    archive_bytes: u32,
    phase: CoronaPhase,
    resume: Option<ResumeToken>,
) {
    let ratio = if archive_bytes == 0 {
        0.0
    } else {
        (written_offset as f32 / archive_bytes as f32).clamp(0.0, 1.0)
    };
    let (progress, status, token) = match phase {
        CoronaPhase::Idle | CoronaPhase::Receiving => (ratio * 0.90, "uploading", resume),
        CoronaPhase::Verifying => (0.94, "verifying", resume),
        CoronaPhase::Installing => (0.98, "installing", resume),
        CoronaPhase::Installed => (1.0, "installed", None),
        CoronaPhase::Error => (ratio * 0.90, "error", resume),
        CoronaPhase::Canceled => (ratio * 0.90, "canceled", None),
    };
    progress_cb(ResourcePackProgress {
        progress,
        status: status.into(),
        resume: token,
    });
}

/// Runs the complete Corona CRPack upload and installation state machine over `transport`.
pub async fn run_install_session<T: CoronaTransport>(
    transport: &mut T,
    archive_path: &Path,
    prepared: &PreparedArchive,
    resume: Option<ResumeToken>,
    config: SessionConfig,
    progress_cb: impl Fn(ResourcePackProgress) + Send,
) -> anyhow::Result<()> {
    if let Some(token) = &resume {
        ensure!(
            token.digest == prepared.digest_hex,
            "res_pack changed; remove the old task and start a replacement"
        );
    }

    let mut file = File::open(archive_path).context("failed to open CRPack for streaming")?;
    let mut read_buf = [0u8; MAX_PAYLOAD_BYTES];
    let mut recovery_round = 0usize;
    let mut cancel_hash_or_pack = false;
    let mut previous_token_for_cancel: Option<u32> = None;

    'recovery: loop {
        ensure!(
            recovery_round <= config.max_recovery_rounds,
            "Corona transfer exceeded maximum recovery attempts ({})",
            config.max_recovery_rounds
        );

        transport.connect_and_listen().await?;
        let mut seq_alloc = SeqAlloc::new();

        // 1. HELLO
        let caps = exchange_hello(transport, &mut seq_alloc, config.control_timeout).await?;
        ensure!(
            prepared.archive_bytes <= caps.max_archive_bytes,
            "CRPack archive size ({}) exceeds device limit ({})",
            prepared.archive_bytes,
            caps.max_archive_bytes
        );
        ensure!(
            prepared.unpacked_bytes <= caps.max_unpacked_bytes,
            "CRPack unpacked size ({}) exceeds device limit ({})",
            prepared.unpacked_bytes,
            caps.max_unpacked_bytes
        );
        let negotiated_rpc = caps.effective_max_rpc(config.host_max_rpc);
        transport.set_max_rpc(negotiated_rpc);

        if cancel_hash_or_pack {
            if let Some(old_token) = previous_token_for_cancel.take() {
                let _ = exchange_control(
                    transport,
                    &mut seq_alloc,
                    CoronaOp::Cancel,
                    old_token,
                    prepared.archive_bytes,
                    config.control_timeout,
                )
                .await;
            }
            cancel_hash_or_pack = false;
        }

        // 2. OPEN (fresh nonce per lease rotation; retained across retries of the same OPEN)
        let open_nonce = new_open_nonce();
        let open_resp = exchange_open_with_timeout(
            transport,
            &mut seq_alloc,
            prepared.archive_bytes,
            &prepared.sha256,
            &open_nonce,
            config.control_timeout,
        )
        .await?;

        if open_resp.code != CoronaCode::Ok {
            if matches!(open_resp.code, CoronaCode::Hash | CoronaCode::Pack)
                && open_resp.token != 0
                && recovery_round < config.max_recovery_rounds
            {
                previous_token_for_cancel = Some(open_resp.token);
                cancel_hash_or_pack = true;
                recovery_round += 1;
                continue 'recovery;
            }
            bail!(
                "Corona OPEN rejected with code {} (detail={:?})",
                open_resp.code,
                open_resp.progress.map(|p| p.detail)
            );
        }

        let initial_progress = open_resp
            .progress
            .context("Corona OPEN OK response missing progress")?;
        let resume_token = ResumeToken {
            digest: prepared.digest_hex.clone(),
            chunk_size: usize::from(caps.chunk_bytes),
        };
        emit_phase_progress(
            &progress_cb,
            initial_progress.written_offset,
            prepared.archive_bytes,
            initial_progress.phase,
            Some(resume_token.clone()),
        );

        match initial_progress.phase {
            CoronaPhase::Installed => {
                emit_phase_progress(
                    &progress_cb,
                    prepared.archive_bytes,
                    prepared.archive_bytes,
                    CoronaPhase::Installed,
                    None,
                );
                return Ok(());
            }
            CoronaPhase::Verifying | CoronaPhase::Installing => {
                match poll_until_installed(
                    transport,
                    &mut seq_alloc,
                    initial_progress.token,
                    prepared.archive_bytes,
                    &resume_token,
                    &config,
                    &progress_cb,
                )
                .await
                {
                    Ok(()) => return Ok(()),
                    Err(PollError::NeedReopen) => {
                        recovery_round += 1;
                        continue 'recovery;
                    }
                    Err(PollError::Fatal(err)) => return Err(err),
                }
            }
            CoronaPhase::Error => {
                if matches!(initial_progress.detail, CoronaCode::Hash | CoronaCode::Pack)
                    && recovery_round < config.max_recovery_rounds
                {
                    previous_token_for_cancel = Some(initial_progress.token);
                    cancel_hash_or_pack = true;
                    recovery_round += 1;
                    continue 'recovery;
                }
                bail!(
                    "Corona task is in ERROR phase (detail={})",
                    initial_progress.detail
                );
            }
            CoronaPhase::Canceled => {
                recovery_round += 1;
                continue 'recovery;
            }
            CoronaPhase::Idle => {
                bail!("Corona OPEN returned unexpected IDLE phase");
            }
            CoronaPhase::Receiving => {}
        }

        // 3. Windowed DATA loop
        let mut tracker = WindowTracker::new(
            &caps,
            &initial_progress,
            prepared.archive_bytes,
            config.host_max_rpc,
        );
        let mut inflight = JoinSet::<(u32, usize, anyhow::Result<()>)>::new();
        let mut last_ack_at = Instant::now();

        while tracker.acked_offset < prepared.archive_bytes || !inflight.is_empty() {
            // Fill window while both byte budget and RPC budget allow.
            while let Some((offset, len)) = tracker.next_chunk_range() {
                file.seek(SeekFrom::Start(u64::from(offset)))
                    .context("failed to seek CRPack file")?;
                file.read_exact(&mut read_buf[..len])
                    .context("failed to read CRPack chunk")?;
                let frame = encode_data_frame(
                    tracker.token,
                    offset,
                    &read_buf[..len],
                    prepared.archive_bytes,
                    tracker.chunk_bytes,
                )?;
                match transport
                    .submit_data(offset, frame, config.data_rpc_timeout)
                    .await
                {
                    Ok(rpc_fut) => {
                        if inflight.is_empty() && tracker.send_offset == tracker.acked_offset {
                            last_ack_at = Instant::now();
                        }
                        tracker.record_sent(len);
                        inflight.spawn(async move { (offset, len, rpc_fut.await) });
                    }
                    Err(err) => {
                        log::warn!("[Corona] submit_data failed at offset {offset}: {err:#}");
                        drain_inflight(&mut inflight, &mut tracker).await;
                        recovery_round += 1;
                        continue 'recovery;
                    }
                }
            }

            let ack_deadline = last_ack_at + config.ack_wait;

            if !inflight.is_empty() {
                let mut recv_fut = std::pin::pin!(transport.recv_frame(ack_deadline));
                tokio::select! {
                    join_res = inflight.join_next() => {
                        if let Some(res) = join_res {
                            tracker.record_rpc_completed();
                            match res {
                                Ok((_off, _len, Ok(()))) => {
                                    // CPC2 completion only releases an RPC slot; do NOT advance ackedOffset.
                                }
                                Ok((off, _len, Err(err))) => {
                                    log::warn!("[Corona] DATA RPC at offset {off} failed: {err:#}");
                                    // Pause window, drain remaining in-flight RPCs, and rotate lease via OPEN.
                                    drain_inflight(&mut inflight, &mut tracker).await;
                                    recovery_round += 1;
                                    continue 'recovery;
                                }
                                Err(join_err) => {
                                    bail!("Corona DATA task panicked: {join_err}");
                                }
                            }
                        }
                    }
                    frame_res = &mut recv_fut => {
                        match frame_res {
                            Ok(Some(bytes)) => {
                                if let Ok(ack) = decode_ack_frame(
                                    &bytes,
                                    tracker.token,
                                    tracker.send_offset,
                                    prepared.archive_bytes,
                                ) {
                                    let advanced = tracker.apply_progress(&ack)?;
                                    if advanced {
                                        last_ack_at = Instant::now();
                                        emit_phase_progress(
                                            &progress_cb,
                                            tracker.acked_offset,
                                            prepared.archive_bytes,
                                            tracker.phase,
                                            Some(resume_token.clone()),
                                        );
                                    }
                                    if tracker.phase == CoronaPhase::Error {
                                        drain_inflight(&mut inflight, &mut tracker).await;
                                        if matches!(
                                            tracker.detail,
                                            CoronaCode::Offset | CoronaCode::Busy
                                        ) && recovery_round < config.max_recovery_rounds
                                        {
                                            recovery_round += 1;
                                            continue 'recovery;
                                        }
                                        bail!("Corona receiver reported error during DATA: {}", tracker.detail);
                                    }
                                }
                            }
                            Ok(None) => {
                                // ACK wait elapsed while RPCs are still in flight: drain in-flight RPCs first,
                                // then fall through to the QUERY barrier below.
                                let rpc_failed = drain_inflight(&mut inflight, &mut tracker).await;
                                if rpc_failed {
                                    recovery_round += 1;
                                    continue 'recovery;
                                }
                            }
                            Err(err) => {
                                log::warn!("[Corona] event receive failed during DATA: {err:#}");
                                drain_inflight(&mut inflight, &mut tracker).await;
                                recovery_round += 1;
                                continue 'recovery;
                            }
                        }
                    }
                }
            } else {
                // No RPCs in flight, waiting for cumulative ACK to advance window or confirm last chunk.
                match transport.recv_frame(ack_deadline).await {
                    Ok(Some(bytes)) => {
                        if let Ok(ack) = decode_ack_frame(
                            &bytes,
                            tracker.token,
                            tracker.send_offset,
                            prepared.archive_bytes,
                        ) {
                            let advanced = tracker.apply_progress(&ack)?;
                            if advanced {
                                last_ack_at = Instant::now();
                                emit_phase_progress(
                                    &progress_cb,
                                    tracker.acked_offset,
                                    prepared.archive_bytes,
                                    tracker.phase,
                                    Some(resume_token.clone()),
                                );
                            }
                            if tracker.phase == CoronaPhase::Error {
                                if matches!(tracker.detail, CoronaCode::Offset | CoronaCode::Busy)
                                    && recovery_round < config.max_recovery_rounds
                                {
                                    recovery_round += 1;
                                    continue 'recovery;
                                }
                                bail!(
                                    "Corona receiver reported error during DATA: {}",
                                    tracker.detail
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        // ACK timed out after all in-flight RPCs completed: issue ordered QUERY write barrier.
                        let Some(query_resp) = query_with_barrier(
                            transport,
                            &mut seq_alloc,
                            tracker.token,
                            prepared.archive_bytes,
                            config.control_timeout,
                        )
                        .await
                        else {
                            recovery_round += 1;
                            continue 'recovery;
                        };

                        if query_resp.code == CoronaCode::Stale {
                            recovery_round += 1;
                            continue 'recovery;
                        }

                        let Some(progress) = query_resp.progress else {
                            recovery_round += 1;
                            continue 'recovery;
                        };

                        tracker.apply_progress(&progress)?;
                        last_ack_at = Instant::now();
                        emit_phase_progress(
                            &progress_cb,
                            tracker.acked_offset,
                            prepared.archive_bytes,
                            tracker.phase,
                            Some(resume_token.clone()),
                        );

                        if query_resp.code != CoronaCode::Ok
                            || tracker.phase == CoronaPhase::Error
                        {
                            let err_code = if query_resp.code != CoronaCode::Ok {
                                query_resp.code
                            } else {
                                tracker.detail
                            };
                            if matches!(
                                err_code,
                                CoronaCode::Offset | CoronaCode::Busy | CoronaCode::Stale
                            ) {
                                recovery_round += 1;
                                continue 'recovery;
                            }
                            bail!("Corona QUERY reported fatal error: {err_code}");
                        }

                        // If some sent frames were dropped or not written despite all RPCs completing,
                        // rotate lease via OPEN so we never blindly replay over an active lease.
                        if tracker.acked_offset < tracker.send_offset {
                            recovery_round += 1;
                            continue 'recovery;
                        }
                    }
                    Err(err) => {
                        log::warn!("[Corona] event receive failed waiting for ACK: {err:#}");
                        recovery_round += 1;
                        continue 'recovery;
                    }
                }
            }
        }

        // 4. FINISH (non-blocking acceptance of background verify/install)
        let finish_resp = match exchange_control(
            transport,
            &mut seq_alloc,
            CoronaOp::Finish,
            tracker.token,
            prepared.archive_bytes,
            config.control_timeout,
        )
        .await
        {
            Ok(resp) => resp,
            Err(err) => {
                log::warn!("[Corona] FINISH failed, recovering via QUERY/OPEN: {err:#}");
                recovery_round += 1;
                continue 'recovery;
            }
        };

        if finish_resp.code != CoronaCode::Ok {
            if matches!(
                finish_resp.code,
                CoronaCode::Offset | CoronaCode::Busy | CoronaCode::Stale
            ) {
                recovery_round += 1;
                continue 'recovery;
            }
            if matches!(finish_resp.code, CoronaCode::Hash | CoronaCode::Pack) {
                let _ = exchange_control(
                    transport,
                    &mut seq_alloc,
                    CoronaOp::Cancel,
                    tracker.token,
                    prepared.archive_bytes,
                    config.control_timeout,
                )
                .await;
            }
            bail!("Corona FINISH rejected with code {}", finish_resp.code);
        }

        if let Some(progress) = finish_resp.progress {
            emit_phase_progress(
                &progress_cb,
                progress.written_offset,
                prepared.archive_bytes,
                progress.phase,
                Some(resume_token.clone()),
            );
            if progress.phase == CoronaPhase::Installed {
                emit_phase_progress(
                    &progress_cb,
                    prepared.archive_bytes,
                    prepared.archive_bytes,
                    CoronaPhase::Installed,
                    None,
                );
                return Ok(());
            }
        }

        // 5. Poll QUERY (~1s interval) until INSTALLED / ERROR / CANCELED.
        match poll_until_installed(
            transport,
            &mut seq_alloc,
            tracker.token,
            prepared.archive_bytes,
            &resume_token,
            &config,
            &progress_cb,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(PollError::NeedReopen) => {
                recovery_round += 1;
                continue 'recovery;
            }
            Err(PollError::Fatal(err)) => return Err(err),
        }
    }
}

enum PollError {
    NeedReopen,
    Fatal(anyhow::Error),
}

async fn poll_until_installed<T: CoronaTransport>(
    transport: &mut T,
    seq_alloc: &mut SeqAlloc,
    token: u32,
    archive_bytes: u32,
    resume_token: &ResumeToken,
    config: &SessionConfig,
    progress_cb: &(impl Fn(ResourcePackProgress) + Send),
) -> Result<(), PollError> {
    let mut consecutive_timeouts = 0usize;
    for _ in 0..config.max_finish_polls {
        sleep(config.finish_poll_interval).await;
        let Some(seq) = seq_alloc.next() else {
            return Err(PollError::NeedReopen);
        };
        let req = match encode_control_request(CoronaOp::Query, seq, token) {
            Ok(r) => r,
            Err(e) => return Err(PollError::Fatal(e)),
        };
        if let Err(err) = transport.send_control(req, config.control_timeout).await {
            consecutive_timeouts += 1;
            log::warn!(
                "[Corona] FINISH poll QUERY send failed ({consecutive_timeouts}): {err:#}"
            );
            // Do not treat a single Connect timeout as terminal failure.
            if consecutive_timeouts >= 3 {
                return Err(PollError::NeedReopen);
            }
            continue;
        }

        let deadline = Instant::now() + config.control_timeout;
        let mut got_response = None;
        loop {
            match transport.recv_frame(deadline).await {
                Ok(Some(bytes)) => {
                    if let Ok(resp) = decode_control_response(
                        &bytes,
                        CoronaOp::Query,
                        seq,
                        token,
                        archive_bytes,
                    ) {
                        got_response = Some(resp);
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    log::warn!("[Corona] FINISH poll recv failed: {err:#}");
                    break;
                }
            }
        }

        let Some(resp) = got_response else {
            consecutive_timeouts += 1;
            if consecutive_timeouts >= 3 {
                return Err(PollError::NeedReopen);
            }
            continue;
        };
        consecutive_timeouts = 0;

        if resp.code == CoronaCode::Stale {
            return Err(PollError::NeedReopen);
        }

        let Some(progress) = resp.progress else {
            return Err(PollError::NeedReopen);
        };
        emit_phase_progress(
            progress_cb,
            progress.written_offset,
            archive_bytes,
            progress.phase,
            Some(resume_token.clone()),
        );

        match progress.phase {
            CoronaPhase::Verifying | CoronaPhase::Installing => continue,
            CoronaPhase::Installed => {
                emit_phase_progress(
                    progress_cb,
                    archive_bytes,
                    archive_bytes,
                    CoronaPhase::Installed,
                    None,
                );
                return Ok(());
            }
            CoronaPhase::Receiving => {
                return Err(PollError::NeedReopen);
            }
            CoronaPhase::Error => {
                let code = if progress.detail != CoronaCode::Ok {
                    progress.detail
                } else {
                    resp.code
                };
                if matches!(code, CoronaCode::Hash | CoronaCode::Pack) {
                    if let Some(cancel_seq) = seq_alloc.next() {
                        if let Ok(cancel_req) =
                            encode_control_request(CoronaOp::Cancel, cancel_seq, token)
                        {
                            let _ = transport
                                .send_control(cancel_req, config.control_timeout)
                                .await;
                        }
                    }
                }
                return Err(PollError::Fatal(anyhow!(
                    "Corona installation failed in phase ERROR (code={code})"
                )));
            }
            CoronaPhase::Canceled => {
                return Err(PollError::Fatal(anyhow!("Corona installation was canceled")));
            }
            CoronaPhase::Idle => {
                return Err(PollError::NeedReopen);
            }
        }
    }
    Err(PollError::Fatal(anyhow!(
        "Corona installation timed out waiting for INSTALLED phase"
    )))
}

async fn drain_inflight(
    inflight: &mut JoinSet<(u32, usize, anyhow::Result<()>)>,
    tracker: &mut WindowTracker,
) -> bool {
    let mut any_failed = false;
    while let Some(res) = inflight.join_next().await {
        tracker.record_rpc_completed();
        match res {
            Ok((_, _, Ok(()))) => {}
            Ok((_, _, Err(_))) | Err(_) => {
                any_failed = true;
            }
        }
    }
    any_failed
}

async fn exchange_hello<T: CoronaTransport>(
    transport: &mut T,
    seq_alloc: &mut SeqAlloc,
    timeout: Duration,
) -> anyhow::Result<HelloCaps> {
    let seq = seq_alloc
        .next()
        .context("control seq exhausted before HELLO")?;
    let req = encode_hello_request(seq, PROTOCOL_VERSION)?;
    transport.send_control(req, timeout).await?;
    let deadline = Instant::now() + timeout;
    while let Some(bytes) = transport.recv_frame(deadline).await? {
        if let Ok(caps) = decode_hello_response(&bytes, seq) {
            return Ok(caps);
        }
        // Check if it was an explicit error response to our HELLO seq
        if let Ok((hdr, _)) = super::proto::decode_frame(&bytes) {
            if hdr.op == CoronaOp::Hello.response_byte() && hdr.seq == seq {
                let code = CoronaCode::from_byte(hdr.code)?;
                bail!("Corona HELLO failed with code {code}");
            }
        }
    }
    bail!("Corona HELLO timed out")
}

async fn exchange_open_with_timeout<T: CoronaTransport>(
    transport: &mut T,
    seq_alloc: &mut SeqAlloc,
    archive_bytes: u32,
    sha256: &[u8; 32],
    open_nonce: &[u8; 8],
    timeout: Duration,
) -> anyhow::Result<ControlResponse> {
    let seq = seq_alloc
        .next()
        .context("control seq exhausted before OPEN")?;
    let req = encode_open_request(seq, archive_bytes, sha256, open_nonce)?;
    transport.send_control(req, timeout).await?;
    let deadline = Instant::now() + timeout;
    while let Some(bytes) = transport.recv_frame(deadline).await? {
        if let Ok((hdr, _)) = super::proto::decode_frame(&bytes) {
            if hdr.op == CoronaOp::Open.response_byte() && hdr.seq == seq {
                return decode_open_response(&bytes, seq, archive_bytes);
            }
        }
    }
    bail!("Corona OPEN timed out")
}

async fn exchange_control<T: CoronaTransport>(
    transport: &mut T,
    seq_alloc: &mut SeqAlloc,
    op: CoronaOp,
    token: u32,
    archive_bytes: u32,
    timeout: Duration,
) -> anyhow::Result<ControlResponse> {
    let seq = seq_alloc
        .next()
        .with_context(|| format!("control seq exhausted before {op:?}"))?;
    let req = encode_control_request(op, seq, token)?;
    transport.send_control(req, timeout).await?;
    let deadline = Instant::now() + timeout;
    while let Some(bytes) = transport.recv_frame(deadline).await? {
        if let Ok((hdr, _)) = super::proto::decode_frame(&bytes) {
            if hdr.op == op.response_byte() && hdr.seq == seq && hdr.token == token {
                return decode_control_response(&bytes, op, seq, token, archive_bytes);
            }
        }
    }
    bail!("Corona {op:?} timed out")
}

async fn query_with_barrier<T: CoronaTransport>(
    transport: &mut T,
    seq_alloc: &mut SeqAlloc,
    token: u32,
    archive_bytes: u32,
    timeout: Duration,
) -> Option<ControlResponse> {
    let seq = seq_alloc.next()?;
    let req = encode_control_request(CoronaOp::Query, seq, token).ok()?;
    if transport.send_control(req, timeout).await.is_err() {
        return None;
    }
    let deadline = Instant::now() + timeout;
    while let Ok(Some(bytes)) = transport.recv_frame(deadline).await {
        if let Ok((hdr, _)) = super::proto::decode_frame(&bytes) {
            if hdr.op == CoronaOp::Query.response_byte() && hdr.seq == seq && hdr.token == token {
                return decode_control_response(&bytes, CoronaOp::Query, seq, token, archive_bytes)
                    .ok();
            }
        }
    }
    None
}

/// Paginates `LIST` over `transport` (one item per page, restarting on `CHANGED` revision).
pub async fn run_list_session<T: CoronaTransport>(
    transport: &mut T,
    timeout: Duration,
) -> anyhow::Result<Vec<InstalledResourcePack>> {
    transport.connect_and_listen().await?;
    let mut seq_alloc = SeqAlloc::new();
    let _caps = exchange_hello(transport, &mut seq_alloc, timeout).await?;

    'revision: for _ in 0..MAX_LIST_REVISIONS {
        let mut expected_revision = 0u32;
        let mut index = 0u32;
        let mut expected_total: Option<u32> = None;
        let mut items = Vec::new();
        let mut seen_ids = HashSet::new();

        loop {
            let seq = seq_alloc
                .next()
                .context("control seq exhausted during LIST")?;
            let req = encode_list_request(seq, index, expected_revision)?;
            transport.send_control(req, timeout).await?;
            let deadline = Instant::now() + timeout;
            let mut page_opt = None;
            while let Some(bytes) = transport.recv_frame(deadline).await? {
                if let Ok((hdr, _)) = super::proto::decode_frame(&bytes) {
                    if hdr.op == CoronaOp::List.response_byte() && hdr.seq == seq {
                        page_opt = Some(decode_list_response(&bytes, seq, index)?);
                        break;
                    }
                }
            }
            let page = page_opt.context("Corona LIST timed out")?;
            match page.code {
                CoronaCode::Ok => {}
                CoronaCode::Changed => {
                    continue 'revision;
                }
                other => bail!("Corona LIST failed with code {other}"),
            }

            if index == 0 {
                expected_revision = page.revision;
                expected_total = Some(page.total);
                if page.total == 0 {
                    ensure!(
                        page.item.is_none() && page.next_index == LIST_END_INDEX,
                        "empty Corona LIST must end with nextIndex=0xffffffff"
                    );
                    return Ok(Vec::new());
                }
            } else {
                ensure!(
                    page.revision == expected_revision,
                    "Corona LIST revision changed mid-pagination without CHANGED code"
                );
                ensure!(
                    Some(page.total) == expected_total,
                    "Corona LIST total changed mid-pagination"
                );
            }

            let item = page
                .item
                .context("non-empty Corona LIST page missing entry")?;
            ensure!(
                seen_ids.insert(item.theme_id.clone()),
                "duplicate themeId in Corona LIST: {}",
                item.theme_id
            );
            if let Some(prev) = items.last() {
                let prev: &InstalledResourcePack = prev;
                ensure!(
                    prev.theme_id < item.theme_id,
                    "Corona LIST entries are not in themeId ASCII order"
                );
            }
            items.push(item);

            if page.next_index == LIST_END_INDEX {
                ensure!(
                    items.len() as u32 == page.total,
                    "Corona LIST completed with {} items, expected {}",
                    items.len(),
                    page.total
                );
                return Ok(items);
            }
            ensure!(
                page.next_index == index + 1 && page.next_index < page.total,
                "invalid Corona LIST nextIndex {} after index {}",
                page.next_index,
                index
            );
            index = page.next_index;
        }
    }
    bail!("Corona LIST aborted after repeated registry revision changes")
}
