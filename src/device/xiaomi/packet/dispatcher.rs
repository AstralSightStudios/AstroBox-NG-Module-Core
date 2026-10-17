use std::{
    collections::HashMap,
    io::Cursor,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use pb::xiaomi::protocol::WearPacket;
use prost::Message;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::device::xiaomi::XiaomiDevice;

use super::{
    cipher::{SharedL2Cipher, ensure_l2_cipher},
    v2::{
        layer1::L1Packet,
        layer2::{L2Cipher, L2Packet},
    },
};

/// magic(2) + type|frx(1) + seq(1) + length(2) + crc(2)
const L1_HEADER_LEN: usize = 8;

static RECV_BUFFERS: OnceLock<RwLock<HashMap<String, Vec<u8>>>> = OnceLock::new();
static RX_PIPELINES: OnceLock<Mutex<HashMap<String, RxPipeline>>> = OnceLock::new();
static PACKET_OBSERVERS: OnceLock<RwLock<Vec<Arc<dyn Fn(XiaomiPacketEvent) + Send + Sync>>>> =
    OnceLock::new();

#[derive(Clone, Debug)]
pub struct XiaomiPacketEvent {
    pub device_id: String,
    pub channel_id: u32,
    pub opcode_id: u32,
    pub payload: Vec<u8>,
    pub protobuf_type_id: Option<u32>,
    pub protobuf_packet_id: Option<u32>,
}

/// 每台设备一条接收流水线：完整的 L1 帧按到达顺序进队，由唯一的消费任务依次交给 SAR 和上层（对齐官方串行接收队列）。
struct RxPipeline {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    closed: Arc<AtomicBool>,
}

fn recv_buffer_registry() -> &'static RwLock<HashMap<String, Vec<u8>>> {
    RECV_BUFFERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn rx_pipeline_registry() -> &'static Mutex<HashMap<String, RxPipeline>> {
    RX_PIPELINES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn packet_observer_registry() -> &'static RwLock<Vec<Arc<dyn Fn(XiaomiPacketEvent) + Send + Sync>>>
{
    PACKET_OBSERVERS.get_or_init(|| RwLock::new(Vec::new()))
}

pub fn register_observer(observer: Arc<dyn Fn(XiaomiPacketEvent) + Send + Sync>) {
    match packet_observer_registry().write() {
        Ok(mut guard) => guard.push(observer),
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.push(observer);
        }
    }
}

fn emit_packet_event(event: XiaomiPacketEvent) {
    let observers = match packet_observer_registry().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if observers.is_empty() {
        return;
    }
    for observer in observers {
        observer(event.clone());
    }
}

/// 传输层回调入口。组帧必须在回调里按到达顺序同步完成：BLE 上一帧常被拆成好几条通知，
/// 以前每条通知各 spawn 一个任务再去拼包，多线程运行时（尤其是 LIFO 槽）会让后到的分片先拼进去，
/// 整帧 CRC 校验失败被丢，设备只能干等超时重传——iOS 上就表现为安装时等不到 Prepare 回包。
pub fn on_packet(tk_handle: Handle, device_id: String, data: Vec<u8>) {
    let frames = extract_frames(&device_id, &data);
    if frames.is_empty() {
        return;
    }

    let mut registry = rx_pipeline_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut restarted = false;
    let mut pending = frames.into_iter();
    while let Some(frame) = pending.next() {
        let pipeline = registry
            .entry(device_id.clone())
            .or_insert_with(|| spawn_rx_pipeline(tk_handle.clone(), device_id.clone()));
        let Err(mpsc::error::SendError(frame)) = pipeline.tx.send(frame) else {
            continue;
        };
        registry.remove(&device_id);
        if restarted {
            log::error!(
                "[Dispatcher] cannot start rx pipeline for {}, dropping received frames",
                device_id
            );
            return;
        }
        // 消费任务已经退出（比如异常结束），换一条新流水线把这批帧继续送进去
        log::warn!(
            "[Dispatcher] rx pipeline for {} stopped unexpectedly, restarting it",
            device_id
        );
        restarted = true;
        let pipeline = registry
            .entry(device_id.clone())
            .or_insert_with(|| spawn_rx_pipeline(tk_handle.clone(), device_id.clone()));
        if pipeline.tx.send(frame).is_err() {
            registry.remove(&device_id);
            log::error!(
                "[Dispatcher] cannot start rx pipeline for {}, dropping received frames",
                device_id
            );
            return;
        }
    }
}

fn spawn_rx_pipeline(tk_handle: Handle, device_id: String) -> RxPipeline {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let closed = Arc::new(AtomicBool::new(false));
    let closed_flag = closed.clone();
    crate::asyncrt::spawn_with_handle(
        async move {
            while let Some(frame) = rx.recv().await {
                if closed_flag.load(Ordering::Acquire) {
                    break;
                }
                process_frame(&device_id, frame).await;
            }
        },
        tk_handle,
    );
    RxPipeline { tx, closed }
}

/// 把新数据接到该设备的接收缓冲后面，切出所有完整且 CRC 正确的 L1 帧。
fn extract_frames(device_id: &str, data: &[u8]) -> Vec<Vec<u8>> {
    let mut registry = recv_buffer_registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let buffer = registry.entry(device_id.to_string()).or_default();
    buffer.extend_from_slice(data);
    let frames = drain_frames(buffer);
    if buffer.is_empty() {
        registry.remove(device_id);
    }
    frames
}

#[inline]
fn is_plausible_header(buf: &[u8]) -> bool {
    // type 只有 0..=3，高 3 位恒为 0（bit4 是 frx）
    buf[0] == 0xa5 && buf[1] == 0xa5 && (buf[2] & 0x0f) <= 3 && (buf[2] & 0xe0) == 0
}

/// 从 `start` 开始，若能切出一帧完整且 CRC 正确的 L1 帧就返回它的总长。
fn complete_frame_len(buf: &[u8], start: usize) -> Option<usize> {
    let header = buf.get(start..start + L1_HEADER_LEN)?;
    if !is_plausible_header(header) {
        return None;
    }
    let total = L1_HEADER_LEN + u16::from_le_bytes([header[4], header[5]]) as usize;
    let payload = buf.get(start + L1_HEADER_LEN..start + total)?;
    let declared_crc = u16::from_le_bytes([header[6], header[7]]);
    (L1Packet::crc16_arc(payload) == declared_crc).then_some(total)
}

fn drain_frames(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut idx = 0usize;
    while idx + L1_HEADER_LEN <= buffer.len() {
        if !is_plausible_header(&buffer[idx..idx + L1_HEADER_LEN]) {
            idx += 1;
            continue;
        }
        let total = L1_HEADER_LEN + u16::from_le_bytes([buffer[idx + 4], buffer[idx + 5]]) as usize;
        if idx + total > buffer.len() {
            // 帧还没收全。若后面已经躺着一帧完整且校验通过的帧，说明这个“帧头”是丢包残留
            // 或负载里碰巧出现的 a5a5，跳到真帧处，别让它把后面的回包一起卡住。
            match (idx + 1..buffer.len()).find(|&next| complete_frame_len(buffer, next).is_some()) {
                Some(next) => {
                    log::warn!(
                        "[Dispatcher] dropping {} bytes of an incomplete L1 frame to resync",
                        next - idx
                    );
                    idx = next;
                    continue;
                }
                None => break,
            }
        }
        let declared_crc = u16::from_le_bytes([buffer[idx + 6], buffer[idx + 7]]);
        if L1Packet::crc16_arc(&buffer[idx + L1_HEADER_LEN..idx + total]) != declared_crc {
            // 只跳过一个字节重新找头，不能把声明长度内的字节整段吞掉，否则夹在里面的真帧也没了
            log::warn!("[Dispatcher] L1 frame CRC mismatch, resyncing");
            idx += 1;
            continue;
        }
        frames.push(buffer[idx..idx + total].to_vec());
        idx += total;
    }
    if idx > 0 {
        buffer.drain(..idx);
    }
    frames
}

async fn process_frame(device_id: &str, frame: Vec<u8>) {
    let l1 = match L1Packet::from_bytes(&frame) {
        Ok(p) => p,
        Err(err) => {
            log::warn!("Decode L1 Packet Err: {}", err.to_string());
            return;
        }
    };

    let (deliver_up, sar_version) = crate::ecs::with_rt_mut({
        let device_id_lookup = device_id.to_string();
        let l1_clone = l1.clone();
        move |rt| {
            rt.with_device_mut(&device_id_lookup, |world, entity| {
                if let Some(dev) = world.get_mut::<XiaomiDevice>(entity) {
                    let version = dev.sar_version;
                    if version == 2 {
                        return (dev.sar.lock().on_l1_packet(&l1_clone), Some(version));
                    }
                    return (false, Some(version));
                }
                (false, None)
            })
            .unwrap_or((false, None))
        }
    })
    .await;

    if !deliver_up {
        return;
    }

    let shared_cipher: Option<SharedL2Cipher> = match sar_version {
        Some(version) => ensure_l2_cipher(device_id, version).await,
        None => None,
    };
    let cipher_ref = shared_cipher.as_ref().map(|c| c.as_ref() as &dyn L2Cipher);
    let Ok(l2p) = L2Packet::from_l1(&l1, cipher_ref) else {
        return;
    };

    let ch = l2p.channel;
    let op = l2p.opcode;
    let payload = l2p.payload;
    let (protobuf_type_id, protobuf_packet_id) = if ch == super::v2::layer2::L2Channel::Pb {
        match WearPacket::decode(Cursor::new(&payload)) {
            Ok(packet) => (u32::try_from(packet.r#type).ok(), Some(packet.id)),
            Err(err) => {
                log::debug!(
                    "failed to decode observed Xiaomi PB packet ({} bytes): {}",
                    payload.len(),
                    err
                );
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    emit_packet_event(XiaomiPacketEvent {
        device_id: device_id.to_string(),
        channel_id: ch as u32,
        opcode_id: op as u32,
        payload: payload.clone(),
        protobuf_type_id,
        protobuf_packet_id,
    });

    crate::ecs::with_rt_mut({
        let device_id_dispatch = device_id.to_string();
        move |rt| {
            let _ = rt.with_device_mut(&device_id_dispatch, |world, entity| {
                if let Some(dev) = world.get::<XiaomiDevice>(entity) {
                    if dev.sar_version == 2 {
                        let _ = crate::device::xiaomi::system::dispatch_xiaomi_system_ext_on_l2packet(
                            world, entity, ch, op, &payload,
                        );
                    }
                }
            });
        }
    })
    .await;
}

pub fn clear_recv_buffer(device_id: &str) {
    match recv_buffer_registry().write() {
        Ok(mut registry) => {
            registry.remove(device_id);
        }
        Err(poisoned) => {
            let mut registry = poisoned.into_inner();
            registry.remove(device_id);
        }
    }
    let pipeline = rx_pipeline_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(device_id);
    if let Some(pipeline) = pipeline {
        // 旧连接还没处理完的帧不能再喂给同地址的新会话
        pipeline.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::xiaomi::packet::v2::layer1::L1DataType;

    fn frame(seq: u8, payload: &[u8]) -> Vec<u8> {
        L1Packet::new(L1DataType::Data, false, seq, payload.to_vec()).to_bytes()
    }

    #[test]
    fn reassembles_frames_split_across_notifications() {
        let a = frame(0, &[1; 300]);
        let b = frame(1, &[2; 10]);
        let mut stream = a.clone();
        stream.extend_from_slice(&b);

        let mut buffer = Vec::new();
        let mut frames = Vec::new();
        for chunk in stream.chunks(244) {
            buffer.extend_from_slice(chunk);
            frames.extend(drain_frames(&mut buffer));
        }
        assert_eq!(frames, vec![a, b]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn corrupted_frame_does_not_swallow_following_frames() {
        let mut bad = frame(0, &[7; 64]);
        bad[20] ^= 0xff;
        let good = frame(1, &[3; 16]);
        let mut buffer = bad;
        buffer.extend_from_slice(&good);

        assert_eq!(drain_frames(&mut buffer), vec![good]);
    }

    #[test]
    fn truncated_frame_is_skipped_once_a_later_frame_is_complete() {
        // 丢了一条通知：第一帧只剩前半截，后面紧跟一帧完整的回包
        let lost = frame(0, &[9; 400]);
        let good = frame(1, &[4; 12]);
        let mut buffer = lost[..244].to_vec();
        buffer.extend_from_slice(&good);

        assert_eq!(drain_frames(&mut buffer), vec![good]);
    }

    #[test]
    fn incomplete_frame_waits_for_more_data() {
        let whole = frame(0, &[5; 100]);
        let mut buffer = whole[..50].to_vec();
        assert!(drain_frames(&mut buffer).is_empty());
        assert_eq!(buffer.len(), 50);
        buffer.extend_from_slice(&whole[50..]);
        assert_eq!(drain_frames(&mut buffer), vec![whole]);
    }
}
