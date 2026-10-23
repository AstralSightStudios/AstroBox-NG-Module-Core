//! Corona CanopusConnect binary protocol frame codec and types.
//!
//! All multi-byte integers are little-endian and decoded via byte slices
//! (`from_le_bytes`) without unaligned pointer casts.
//!
//! Frame layout (at most 480 bytes per CanopusConnect binary frame):
//! - `0`: `op: u8` (response sets high bit `0x80`)
//! - `1`: `code: u8` (request = 0, response = status code `0..=11`)
//! - `2..4`: `seq: u16` (control request sequence echoed in response; DATA and unsolicited ACK = 0)
//! - `4..8`: `token: u32` (active transfer lease; HELLO, OPEN request, and LIST = 0)
//! - `8..12`: `arg: u32` (DATA offset, ACK/QUERY/OPEN/FINISH/CANCEL writtenOffset, OPEN archiveBytes, LIST index; otherwise 0)
//! - `12..`: `payload` (at most 468 bytes, format determined by `op`)

use anyhow::{Context, bail, ensure};

use super::InstalledResourcePack;

pub const CORONA_MODULE_ID: &str = "corona";
pub const PROTOCOL_VERSION: u16 = 1;
pub const FRAME_MAX_BYTES: usize = 480;
pub const HEADER_BYTES: usize = 12;
pub const MAX_PAYLOAD_BYTES: usize = FRAME_MAX_BYTES - HEADER_BYTES; // 468
pub const DEFAULT_CHUNK_BYTES: u16 = 468;
pub const DEFAULT_MAX_RPC: u8 = 2;
pub const MAX_SUPPORTED_RPC: u8 = 3;
pub const DEFAULT_WINDOW_FRAMES: u8 = 8;
pub const DEFAULT_ACK_EVERY: u8 = 4;
pub const DEFAULT_CHECKPOINT_BYTES: u32 = 65_536;
pub const MAX_ARCHIVE_BYTES_LIMIT: u32 = 64 * 1024 * 1024;
pub const MAX_UNPACKED_BYTES_LIMIT: u32 = 64 * 1024 * 1024;
pub const HELLO_PAYLOAD_BYTES: usize = 20;
pub const OPEN_PAYLOAD_BYTES: usize = 40;
pub const PROGRESS_PAYLOAD_BYTES: usize = 8;
pub const LIST_REQUEST_PAYLOAD_BYTES: usize = 4;
pub const LIST_RESPONSE_PREFIX_BYTES: usize = 12;
pub const LIST_ITEM_HEADER_BYTES: usize = 14;
pub const LIST_MAX_FRAME_BYTES: usize = 422;
pub const LIST_END_INDEX: u32 = 0xffff_ffff;
pub const RESPONSE_BIT: u8 = 0x80;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CoronaOp {
    Hello = 1,
    Open = 2,
    Data = 3,
    Query = 4,
    Finish = 5,
    Cancel = 6,
    List = 7,
}

impl CoronaOp {
    pub fn from_request_byte(byte: u8) -> anyhow::Result<Self> {
        Ok(match byte {
            1 => Self::Hello,
            2 => Self::Open,
            3 => Self::Data,
            4 => Self::Query,
            5 => Self::Finish,
            6 => Self::Cancel,
            7 => Self::List,
            other => bail!("unknown Corona request op: 0x{other:02x}"),
        })
    }

    pub fn from_response_byte(byte: u8) -> anyhow::Result<Self> {
        ensure!(
            byte & RESPONSE_BIT != 0,
            "Corona response op missing 0x80 bit: 0x{byte:02x}"
        );
        Self::from_request_byte(byte & !RESPONSE_BIT)
            .with_context(|| format!("unknown Corona response op: 0x{byte:02x}"))
    }

    pub const fn response_byte(self) -> u8 {
        (self as u8) | RESPONSE_BIT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CoronaCode {
    #[default]
    Ok = 0,
    Bad = 1,
    Busy = 2,
    Stale = 3,
    Offset = 4,
    Io = 5,
    Hash = 6,
    Pack = 7,
    Limit = 8,
    Active = 9,
    Changed = 10,
    Unsupported = 11,
}

impl CoronaCode {
    pub fn from_byte(byte: u8) -> anyhow::Result<Self> {
        Ok(match byte {
            0 => Self::Ok,
            1 => Self::Bad,
            2 => Self::Busy,
            3 => Self::Stale,
            4 => Self::Offset,
            5 => Self::Io,
            6 => Self::Hash,
            7 => Self::Pack,
            8 => Self::Limit,
            9 => Self::Active,
            10 => Self::Changed,
            11 => Self::Unsupported,
            other => bail!("unknown Corona status code: {other}"),
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Bad => "BAD",
            Self::Busy => "BUSY",
            Self::Stale => "STALE",
            Self::Offset => "OFFSET",
            Self::Io => "IO",
            Self::Hash => "HASH",
            Self::Pack => "PACK",
            Self::Limit => "LIMIT",
            Self::Active => "ACTIVE",
            Self::Changed => "CHANGED",
            Self::Unsupported => "UNSUPPORTED",
        }
    }
}

impl std::fmt::Display for CoronaCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CoronaPhase {
    #[default]
    Idle = 0,
    Receiving = 1,
    Verifying = 2,
    Installing = 3,
    Installed = 4,
    Error = 5,
    Canceled = 6,
}

impl CoronaPhase {
    pub fn from_byte(byte: u8) -> anyhow::Result<Self> {
        Ok(match byte {
            0 => Self::Idle,
            1 => Self::Receiving,
            2 => Self::Verifying,
            3 => Self::Installing,
            4 => Self::Installed,
            5 => Self::Error,
            6 => Self::Canceled,
            other => bail!("unknown Corona phase: {other}"),
        })
    }

    pub const fn status_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Receiving => "uploading",
            Self::Verifying => "verifying",
            Self::Installing => "installing",
            Self::Installed => "installed",
            Self::Error => "error",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub op: u8,
    pub code: u8,
    pub seq: u16,
    pub token: u32,
    pub arg: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut out = [0u8; HEADER_BYTES];
        out[0] = self.op;
        out[1] = self.code;
        out[2..4].copy_from_slice(&self.seq.to_le_bytes());
        out[4..8].copy_from_slice(&self.token.to_le_bytes());
        out[8..12].copy_from_slice(&self.arg.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            bytes.len() >= HEADER_BYTES,
            "Corona frame too short: {} bytes (need at least {HEADER_BYTES})",
            bytes.len()
        );
        ensure!(
            bytes.len() <= FRAME_MAX_BYTES,
            "Corona frame exceeds {FRAME_MAX_BYTES} bytes: {}",
            bytes.len()
        );
        Ok(Self {
            op: bytes[0],
            code: bytes[1],
            seq: u16::from_le_bytes([bytes[2], bytes[3]]),
            token: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            arg: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        })
    }
}

pub fn encode_frame(header: FrameHeader, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(
        payload.len() <= MAX_PAYLOAD_BYTES,
        "Corona frame payload exceeds {MAX_PAYLOAD_BYTES} bytes: {}",
        payload.len()
    );
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> anyhow::Result<(FrameHeader, &[u8])> {
    let header = FrameHeader::decode(bytes)?;
    Ok((header, &bytes[HEADER_BYTES..]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelloCaps {
    pub version: u16,
    pub chunk_bytes: u16,
    pub max_rpc: u8,
    pub window_frames: u8,
    pub ack_every: u8,
    pub flags: u8,
    pub max_archive_bytes: u32,
    pub max_unpacked_bytes: u32,
    pub checkpoint_bytes: u32,
}

impl Default for HelloCaps {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            max_rpc: DEFAULT_MAX_RPC,
            window_frames: DEFAULT_WINDOW_FRAMES,
            ack_every: DEFAULT_ACK_EVERY,
            flags: 1,
            max_archive_bytes: MAX_ARCHIVE_BYTES_LIMIT,
            max_unpacked_bytes: MAX_UNPACKED_BYTES_LIMIT,
            checkpoint_bytes: DEFAULT_CHECKPOINT_BYTES,
        }
    }
}

impl HelloCaps {
    pub fn encode_payload(&self) -> [u8; HELLO_PAYLOAD_BYTES] {
        let mut out = [0u8; HELLO_PAYLOAD_BYTES];
        out[0..2].copy_from_slice(&self.version.to_le_bytes());
        out[2..4].copy_from_slice(&self.chunk_bytes.to_le_bytes());
        out[4] = self.max_rpc;
        out[5] = self.window_frames;
        out[6] = self.ack_every;
        out[7] = self.flags;
        out[8..12].copy_from_slice(&self.max_archive_bytes.to_le_bytes());
        out[12..16].copy_from_slice(&self.max_unpacked_bytes.to_le_bytes());
        out[16..20].copy_from_slice(&self.checkpoint_bytes.to_le_bytes());
        out
    }

    pub fn decode_payload(payload: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            payload.len() == HELLO_PAYLOAD_BYTES,
            "HELLO response payload must be {HELLO_PAYLOAD_BYTES} bytes, got {}",
            payload.len()
        );
        let version = u16::from_le_bytes([payload[0], payload[1]]);
        ensure!(
            version == PROTOCOL_VERSION,
            "unsupported Corona protocol version: {version}"
        );
        let chunk_bytes = u16::from_le_bytes([payload[2], payload[3]]);
        ensure!(
            chunk_bytes > 0 && usize::from(chunk_bytes) <= MAX_PAYLOAD_BYTES,
            "invalid negotiated chunkBytes: {chunk_bytes}"
        );
        let max_rpc = payload[4];
        ensure!(
            (1..=8).contains(&max_rpc),
            "invalid negotiated maxRpc: {max_rpc}"
        );
        let window_frames = payload[5];
        ensure!(
            (1..=64).contains(&window_frames),
            "invalid negotiated windowFrames: {window_frames}"
        );
        let ack_every = payload[6];
        ensure!(
            (1..=window_frames).contains(&ack_every),
            "invalid negotiated ackEvery: {ack_every} (windowFrames={window_frames})"
        );
        let flags = payload[7];
        ensure!(flags & !0x01 == 0, "unsupported HELLO flags: 0x{flags:02x}");
        let max_archive_bytes = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
        let max_unpacked_bytes =
            u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
        let checkpoint_bytes =
            u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]);
        ensure!(
            max_archive_bytes > 0 && max_archive_bytes <= MAX_ARCHIVE_BYTES_LIMIT,
            "invalid maxArchiveBytes: {max_archive_bytes}"
        );
        ensure!(
            max_unpacked_bytes > 0 && max_unpacked_bytes <= MAX_UNPACKED_BYTES_LIMIT,
            "invalid maxUnpackedBytes: {max_unpacked_bytes}"
        );
        Ok(Self {
            version,
            chunk_bytes,
            max_rpc,
            window_frames,
            ack_every,
            flags,
            max_archive_bytes,
            max_unpacked_bytes,
            checkpoint_bytes,
        })
    }

    pub fn effective_max_rpc(&self, host_cap: u8) -> usize {
        self.max_rpc
            .min(MAX_SUPPORTED_RPC)
            .min(host_cap.max(1))
            .max(1) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressState {
    pub token: u32,
    pub written_offset: u32,
    pub durable_offset: u32,
    pub window_frames: u8,
    pub ack_every: u8,
    pub phase: CoronaPhase,
    pub detail: CoronaCode,
}

impl ProgressState {
    pub fn encode_payload(&self) -> [u8; PROGRESS_PAYLOAD_BYTES] {
        let mut out = [0u8; PROGRESS_PAYLOAD_BYTES];
        out[0..4].copy_from_slice(&self.durable_offset.to_le_bytes());
        out[4] = self.window_frames;
        out[5] = self.ack_every;
        out[6] = self.phase as u8;
        out[7] = self.detail as u8;
        out
    }

    pub fn decode(
        token: u32,
        written_offset: u32,
        payload: &[u8],
        archive_bytes: Option<u32>,
    ) -> anyhow::Result<Self> {
        ensure!(
            payload.len() == PROGRESS_PAYLOAD_BYTES,
            "Corona progress payload must be {PROGRESS_PAYLOAD_BYTES} bytes, got {}",
            payload.len()
        );
        let durable_offset = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        ensure!(
            durable_offset <= written_offset,
            "Corona durableOffset ({durable_offset}) exceeds writtenOffset ({written_offset})"
        );
        if let Some(total) = archive_bytes {
            ensure!(
                written_offset <= total,
                "Corona writtenOffset ({written_offset}) exceeds archiveBytes ({total})"
            );
        }
        let window_frames = payload[4];
        let ack_every = payload[5];
        let phase = CoronaPhase::from_byte(payload[6])?;
        let detail = CoronaCode::from_byte(payload[7])?;
        Ok(Self {
            token,
            written_offset,
            durable_offset,
            window_frames,
            ack_every,
            phase,
            detail,
        })
    }
}

pub fn encode_hello_request(seq: u16, version: u16) -> anyhow::Result<Vec<u8>> {
    ensure!(seq != 0, "control request seq must be non-zero");
    encode_frame(
        FrameHeader {
            op: CoronaOp::Hello as u8,
            code: 0,
            seq,
            token: 0,
            arg: 0,
        },
        &version.to_le_bytes(),
    )
}

pub fn decode_hello_response(bytes: &[u8], expected_seq: u16) -> anyhow::Result<HelloCaps> {
    let (header, payload) = decode_frame(bytes)?;
    let op = CoronaOp::from_response_byte(header.op)?;
    ensure!(
        op == CoronaOp::Hello,
        "expected HELLO response (0x81), got 0x{:02x}",
        header.op
    );
    ensure!(
        header.seq == expected_seq,
        "HELLO response seq mismatch: expected {expected_seq}, got {}",
        header.seq
    );
    let code = CoronaCode::from_byte(header.code)?;
    ensure!(code == CoronaCode::Ok, "HELLO failed with code {code}");
    ensure!(
        header.token == 0 && header.arg == 0,
        "HELLO response must have token=0 and arg=0"
    );
    HelloCaps::decode_payload(payload)
}

pub fn encode_open_request(
    seq: u16,
    archive_bytes: u32,
    sha256: &[u8; 32],
    open_nonce: &[u8; 8],
) -> anyhow::Result<Vec<u8>> {
    ensure!(seq != 0, "control request seq must be non-zero");
    ensure!(
        archive_bytes > 0 && archive_bytes <= MAX_ARCHIVE_BYTES_LIMIT,
        "invalid archiveBytes for OPEN: {archive_bytes}"
    );
    let mut payload = [0u8; OPEN_PAYLOAD_BYTES];
    payload[0..32].copy_from_slice(sha256);
    payload[32..40].copy_from_slice(open_nonce);
    encode_frame(
        FrameHeader {
            op: CoronaOp::Open as u8,
            code: 0,
            seq,
            token: 0,
            arg: archive_bytes,
        },
        &payload,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlResponse {
    pub code: CoronaCode,
    pub token: u32,
    pub arg: u32,
    pub progress: Option<ProgressState>,
}

pub fn decode_open_response(
    bytes: &[u8],
    expected_seq: u16,
    archive_bytes: u32,
) -> anyhow::Result<ControlResponse> {
    let (header, payload) = decode_frame(bytes)?;
    let op = CoronaOp::from_response_byte(header.op)?;
    ensure!(
        op == CoronaOp::Open,
        "expected OPEN response (0x82), got 0x{:02x}",
        header.op
    );
    ensure!(
        header.seq == expected_seq,
        "OPEN response seq mismatch: expected {expected_seq}, got {}",
        header.seq
    );
    let code = CoronaCode::from_byte(header.code)?;
    if code == CoronaCode::Ok {
        ensure!(header.token != 0, "OPEN OK response must assign non-zero token");
        let progress = ProgressState::decode(
            header.token,
            header.arg,
            payload,
            Some(archive_bytes),
        )?;
        Ok(ControlResponse {
            code,
            token: header.token,
            arg: header.arg,
            progress: Some(progress),
        })
    } else {
        let progress = if payload.is_empty() {
            None
        } else {
            Some(ProgressState::decode(
                header.token,
                header.arg,
                payload,
                Some(archive_bytes),
            )?)
        };
        Ok(ControlResponse {
            code,
            token: header.token,
            arg: header.arg,
            progress,
        })
    }
}

pub fn encode_data_frame(
    token: u32,
    offset: u32,
    data: &[u8],
    archive_bytes: u32,
    chunk_bytes: u16,
) -> anyhow::Result<Vec<u8>> {
    ensure!(token != 0, "DATA frame requires non-zero token");
    ensure!(
        !data.is_empty() && data.len() <= usize::from(chunk_bytes) && data.len() <= MAX_PAYLOAD_BYTES,
        "invalid DATA payload length: {} (chunkBytes={chunk_bytes})",
        data.len()
    );
    let len_u32 = u32::try_from(data.len()).context("DATA payload length overflow")?;
    let end = offset
        .checked_add(len_u32)
        .context("DATA offset + length overflow")?;
    ensure!(
        end <= archive_bytes,
        "DATA range {offset}..{end} exceeds archiveBytes {archive_bytes}"
    );
    encode_frame(
        FrameHeader {
            op: CoronaOp::Data as u8,
            code: 0,
            seq: 0,
            token,
            arg: offset,
        },
        data,
    )
}

pub fn decode_ack_frame(
    bytes: &[u8],
    expected_token: u32,
    max_sent_offset: u32,
    archive_bytes: u32,
) -> anyhow::Result<ProgressState> {
    let (header, payload) = decode_frame(bytes)?;
    let op = CoronaOp::from_response_byte(header.op)?;
    ensure!(
        op == CoronaOp::Data,
        "expected DATA ACK (0x83), got 0x{:02x}",
        header.op
    );
    ensure!(header.seq == 0, "unsolicited DATA ACK must have seq=0");
    let code = CoronaCode::from_byte(header.code)?;
    ensure!(code == CoronaCode::Ok, "DATA ACK must have code=OK, got {code}");
    ensure!(
        header.token == expected_token,
        "DATA ACK token mismatch: expected {expected_token}, got {}",
        header.token
    );
    ensure!(
        header.arg <= max_sent_offset && header.arg <= archive_bytes,
        "DATA ACK writtenOffset ({}) exceeds sent ({max_sent_offset}) or archiveBytes ({archive_bytes})",
        header.arg
    );
    ProgressState::decode(header.token, header.arg, payload, Some(archive_bytes))
}

pub fn encode_control_request(op: CoronaOp, seq: u16, token: u32) -> anyhow::Result<Vec<u8>> {
    ensure!(
        matches!(op, CoronaOp::Query | CoronaOp::Finish | CoronaOp::Cancel),
        "unsupported token control op: {op:?}"
    );
    ensure!(seq != 0, "control request seq must be non-zero");
    ensure!(token != 0, "token control request requires non-zero token");
    encode_frame(
        FrameHeader {
            op: op as u8,
            code: 0,
            seq,
            token,
            arg: 0,
        },
        &[],
    )
}

pub fn decode_control_response(
    bytes: &[u8],
    expected_op: CoronaOp,
    expected_seq: u16,
    expected_token: u32,
    archive_bytes: u32,
) -> anyhow::Result<ControlResponse> {
    let (header, payload) = decode_frame(bytes)?;
    let op = CoronaOp::from_response_byte(header.op)?;
    ensure!(
        op == expected_op,
        "expected {expected_op:?} response (0x{:02x}), got 0x{:02x}",
        expected_op.response_byte(),
        header.op
    );
    ensure!(
        header.seq == expected_seq,
        "{expected_op:?} response seq mismatch: expected {expected_seq}, got {}",
        header.seq
    );
    ensure!(
        header.token == expected_token,
        "{expected_op:?} response token mismatch: expected {expected_token}, got {}",
        header.token
    );
    let code = CoronaCode::from_byte(header.code)?;
    if code == CoronaCode::Stale && payload.is_empty() {
        return Ok(ControlResponse {
            code,
            token: header.token,
            arg: header.arg,
            progress: None,
        });
    }
    if expected_op == CoronaOp::Query {
        // Task-associated QUERY responses (including error QUERY) must carry the 8B progress structure.
        let progress = ProgressState::decode(
            header.token,
            header.arg,
            payload,
            Some(archive_bytes),
        )?;
        return Ok(ControlResponse {
            code,
            token: header.token,
            arg: header.arg,
            progress: Some(progress),
        });
    }
    let progress = if payload.is_empty() {
        ensure!(
            code != CoronaCode::Ok,
            "{expected_op:?} OK response must include progress payload"
        );
        None
    } else {
        Some(ProgressState::decode(
            header.token,
            header.arg,
            payload,
            Some(archive_bytes),
        )?)
    };
    Ok(ControlResponse {
        code,
        token: header.token,
        arg: header.arg,
        progress,
    })
}

pub fn encode_list_request(seq: u16, index: u32, revision: u32) -> anyhow::Result<Vec<u8>> {
    ensure!(seq != 0, "LIST request seq must be non-zero");
    encode_frame(
        FrameHeader {
            op: CoronaOp::List as u8,
            code: 0,
            seq,
            token: 0,
            arg: index,
        },
        &revision.to_le_bytes(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListResponseFrame {
    pub code: CoronaCode,
    pub index: u32,
    pub revision: u32,
    pub total: u32,
    pub next_index: u32,
    pub item: Option<InstalledResourcePack>,
}

pub fn decode_list_response(
    bytes: &[u8],
    expected_seq: u16,
    expected_index: u32,
) -> anyhow::Result<ListResponseFrame> {
    ensure!(
        bytes.len() <= LIST_MAX_FRAME_BYTES,
        "LIST response frame exceeds {LIST_MAX_FRAME_BYTES} bytes: {}",
        bytes.len()
    );
    let (header, payload) = decode_frame(bytes)?;
    let op = CoronaOp::from_response_byte(header.op)?;
    ensure!(
        op == CoronaOp::List,
        "expected LIST response (0x87), got 0x{:02x}",
        header.op
    );
    ensure!(
        header.seq == expected_seq,
        "LIST response seq mismatch: expected {expected_seq}, got {}",
        header.seq
    );
    ensure!(
        header.token == 0,
        "LIST response must have token=0, got {}",
        header.token
    );
    ensure!(
        header.arg == expected_index,
        "LIST response arg (index) mismatch: expected {expected_index}, got {}",
        header.arg
    );
    let code = CoronaCode::from_byte(header.code)?;
    if code != CoronaCode::Ok {
        return Ok(ListResponseFrame {
            code,
            index: header.arg,
            revision: 0,
            total: 0,
            next_index: LIST_END_INDEX,
            item: None,
        });
    }
    ensure!(
        payload.len() >= LIST_RESPONSE_PREFIX_BYTES,
        "LIST response payload too short: {} bytes",
        payload.len()
    );
    let revision = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let total = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let next_index = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let rest = &payload[LIST_RESPONSE_PREFIX_BYTES..];

    if rest.is_empty() {
        ensure!(
            total == 0 && expected_index == 0 && next_index == LIST_END_INDEX,
            "empty LIST response only valid when total=0, index=0, nextIndex=0xffffffff"
        );
        return Ok(ListResponseFrame {
            code,
            index: header.arg,
            revision,
            total,
            next_index,
            item: None,
        });
    }

    ensure!(
        total > 0 && expected_index < total,
        "non-empty LIST entry out of bounds: index={expected_index}, total={total}"
    );
    ensure!(
        rest.len() >= LIST_ITEM_HEADER_BYTES,
        "LIST entry header too short: {} bytes",
        rest.len()
    );
    let flags = rest[0];
    ensure!(flags & !0b111 == 0, "invalid LIST entry flags: 0x{flags:02x}");
    let has_version = flags & 0b001 != 0;
    let has_version_code = flags & 0b010 != 0;
    let has_author = flags & 0b100 != 0;

    let id_len = rest[1] as usize;
    let name_len = rest[2] as usize;
    let version_len = rest[3] as usize;
    let author_len = rest[4] as usize;
    let metadata = rest[5];
    let version_code_raw = u64::from_le_bytes([
        rest[6], rest[7], rest[8], rest[9], rest[10], rest[11], rest[12], rest[13],
    ]);

    ensure!((1..=64).contains(&id_len), "invalid LIST idLen: {id_len}");
    ensure!(
        (1..=128).contains(&name_len),
        "invalid LIST nameLen: {name_len}"
    );
    ensure!(version_len <= 64, "invalid LIST versionLen: {version_len}");
    ensure!(author_len <= 128, "invalid LIST authorLen: {author_len}");

    if !has_version {
        ensure!(
            version_len == 0,
            "LIST versionLen must be 0 when version flag is unset"
        );
    } else {
        ensure!(
            version_len > 0,
            "LIST versionLen must be non-zero when version flag is set"
        );
    }
    if !has_version_code {
        ensure!(
            version_code_raw == 0,
            "LIST versionCode must be 0 when versionCode flag is unset"
        );
    } else {
        ensure!(
            version_code_raw <= MAX_SAFE_INTEGER,
            "LIST versionCode exceeds safe integer range: {version_code_raw}"
        );
    }
    if !has_author {
        ensure!(
            author_len == 0,
            "LIST authorLen must be 0 when author flag is unset"
        );
    } else {
        ensure!(
            author_len > 0,
            "LIST authorLen must be non-zero when author flag is set"
        );
    }

    let expected_strings_len = id_len + name_len + version_len + author_len;
    ensure!(
        rest.len() == LIST_ITEM_HEADER_BYTES + expected_strings_len,
        "LIST entry payload length mismatch: expected {}, got {}",
        LIST_ITEM_HEADER_BYTES + expected_strings_len,
        rest.len()
    );

    let strings = &rest[LIST_ITEM_HEADER_BYTES..];
    let (id_bytes, strings) = strings.split_at(id_len);
    let (name_bytes, strings) = strings.split_at(name_len);
    let (version_bytes, author_bytes) = strings.split_at(version_len);

    let theme_id = decode_utf8_no_nul(id_bytes, "themeId")?;
    ensure!(
        crate::device::crpack::valid_theme_id(&theme_id),
        "invalid LIST themeId: {theme_id}"
    );
    let name = decode_utf8_no_nul(name_bytes, "name")?;
    let version = if has_version {
        Some(decode_utf8_no_nul(version_bytes, "version")?)
    } else {
        None
    };
    let version_code = if has_version_code {
        Some(version_code_raw)
    } else {
        None
    };
    let author = if has_author {
        Some(decode_utf8_no_nul(author_bytes, "author")?)
    } else {
        None
    };

    let metadata_status = match metadata {
        0 => "ok".to_string(),
        1 => {
            ensure!(
                name == theme_id && version.is_none() && version_code.is_none() && author.is_none(),
                "unavailable LIST metadata must fallback name=themeId and omit optional fields"
            );
            "unavailable".to_string()
        }
        other => bail!("invalid LIST metadata status byte: {other}"),
    };

    Ok(ListResponseFrame {
        code,
        index: header.arg,
        revision,
        total,
        next_index,
        item: Some(InstalledResourcePack {
            theme_id,
            name,
            version,
            version_code,
            author,
            metadata_status,
        }),
    })
}

fn decode_utf8_no_nul(bytes: &[u8], field: &str) -> anyhow::Result<String> {
    ensure!(
        !bytes.contains(&0),
        "LIST {field} must not contain NUL bytes"
    );
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .with_context(|| format!("LIST {field} is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_hello_roundtrip_little_endian() {
        let req = encode_hello_request(7, PROTOCOL_VERSION).unwrap();
        assert_eq!(req.len(), 14);
        assert_eq!(req[0], 1);
        assert_eq!(req[1], 0);
        assert_eq!(u16::from_le_bytes([req[2], req[3]]), 7);
        assert_eq!(u16::from_le_bytes([req[12], req[13]]), 1);

        let caps = HelloCaps::default();
        let resp = encode_frame(
            FrameHeader {
                op: CoronaOp::Hello.response_byte(),
                code: CoronaCode::Ok as u8,
                seq: 7,
                token: 0,
                arg: 0,
            },
            &caps.encode_payload(),
        )
        .unwrap();
        assert_eq!(decode_hello_response(&resp, 7).unwrap(), caps);
        assert!(decode_hello_response(&resp, 8).is_err());
    }

    #[test]
    fn data_frame_enforces_480_and_468_limits_and_offset_bounds() {
        let ok = encode_data_frame(0x11223344, 0, &[0xaa; 468], 1000, 468).unwrap();
        assert_eq!(ok.len(), 480);
        assert!(encode_data_frame(0x11223344, 0, &[0xaa; 469], 1000, 468).is_err());
        assert!(encode_data_frame(0, 0, &[0xaa; 10], 1000, 468).is_err());
        assert!(encode_data_frame(1, 995, &[0xaa; 10], 1000, 468).is_err());
        assert!(encode_data_frame(1, u32::MAX - 2, &[0xaa; 4], u32::MAX, 468).is_err());
    }

    #[test]
    fn ack_and_error_query_preserve_progress_structure() {
        let progress = ProgressState {
            token: 42,
            written_offset: 1872,
            durable_offset: 936,
            window_frames: 8,
            ack_every: 4,
            phase: CoronaPhase::Receiving,
            detail: CoronaCode::Ok,
        };
        let ack_wire = encode_frame(
            FrameHeader {
                op: CoronaOp::Data.response_byte(),
                code: 0,
                seq: 0,
                token: 42,
                arg: 1872,
            },
            &progress.encode_payload(),
        )
        .unwrap();
        let decoded = decode_ack_frame(&ack_wire, 42, 1872, 4096).unwrap();
        assert_eq!(decoded, progress);
        // Rejects ACK beyond sent offset or wrong token.
        assert!(decode_ack_frame(&ack_wire, 43, 1872, 4096).is_err());
        assert!(decode_ack_frame(&ack_wire, 42, 1404, 4096).is_err());

        // Error QUERY still carries progress structure and valid writtenOffset.
        let err_progress = ProgressState {
            token: 42,
            written_offset: 1872,
            durable_offset: 936,
            window_frames: 8,
            ack_every: 4,
            phase: CoronaPhase::Error,
            detail: CoronaCode::Offset,
        };
        let query_err_wire = encode_frame(
            FrameHeader {
                op: CoronaOp::Query.response_byte(),
                code: CoronaCode::Offset as u8,
                seq: 9,
                token: 42,
                arg: 1872,
            },
            &err_progress.encode_payload(),
        )
        .unwrap();
        let resp = decode_control_response(&query_err_wire, CoronaOp::Query, 9, 42, 4096).unwrap();
        assert_eq!(resp.code, CoronaCode::Offset);
        assert_eq!(resp.progress.unwrap().written_offset, 1872);
    }

    #[test]
    fn list_response_validates_worst_case_422_and_flag_invariants() {
        let theme_id = "a".repeat(64);
        let name = "b".repeat(128);
        let version = "c".repeat(64);
        let author = "d".repeat(128);

        let mut payload = Vec::new();
        payload.extend_from_slice(&3u32.to_le_bytes()); // revision
        payload.extend_from_slice(&1u32.to_le_bytes()); // total
        payload.extend_from_slice(&LIST_END_INDEX.to_le_bytes()); // nextIndex
        payload.push(0b111); // flags: version + versionCode + author
        payload.push(64);
        payload.push(128);
        payload.push(64);
        payload.push(128);
        payload.push(0); // metadata = ok
        payload.extend_from_slice(&100u64.to_le_bytes());
        payload.extend_from_slice(theme_id.as_bytes());
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(version.as_bytes());
        payload.extend_from_slice(author.as_bytes());

        let wire = encode_frame(
            FrameHeader {
                op: CoronaOp::List.response_byte(),
                code: CoronaCode::Ok as u8,
                seq: 12,
                token: 0,
                arg: 0,
            },
            &payload,
        )
        .unwrap();
        assert_eq!(wire.len(), LIST_MAX_FRAME_BYTES);
        let decoded = decode_list_response(&wire, 12, 0).unwrap();
        assert_eq!(decoded.revision, 3);
        assert_eq!(decoded.total, 1);
        assert_eq!(decoded.next_index, LIST_END_INDEX);
        let item = decoded.item.unwrap();
        assert_eq!(item.theme_id, theme_id);
        assert_eq!(item.version_code, Some(100));

        // Unset versionCode flag with non-zero versionCode must fail.
        let mut bad_payload = payload.clone();
        bad_payload[12] = 0b101;
        let bad_wire = encode_frame(
            FrameHeader {
                op: CoronaOp::List.response_byte(),
                code: 0,
                seq: 12,
                token: 0,
                arg: 0,
            },
            &bad_payload,
        )
        .unwrap();
        assert!(decode_list_response(&bad_wire, 12, 0).is_err());
    }
}
