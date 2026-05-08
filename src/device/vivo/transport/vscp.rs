use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::super::crypto::{vscp_crc16, vscp_decrypt, vscp_encrypt};
use super::super::{VivoProtocolError, VivoProtocolResult};

const VSCP_V2_VERSION: u8 = 1;
const VSCP_V2_HEADER_LEN: usize = 7;
const VSCP_V2_CRC_LEN: usize = 2;
const VSCP_V2_OVERHEAD: usize = VSCP_V2_HEADER_LEN + VSCP_V2_CRC_LEN;
const VSCP_V2_FRAME0_PREFIX_LEN: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VscpMessage {
    pub bid: u8,
    pub cid: u8,
    pub encrypted: bool,
    pub payload: Vec<u8>,
}

impl VscpMessage {
    pub fn new(bid: u8, cid: u8, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            bid,
            cid,
            encrypted: false,
            payload: payload.into(),
        }
    }

    pub fn encrypted(mut self, encrypted: bool) -> Self {
        self.encrypted = encrypted;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VscpV2Pdu {
    bytes: Vec<u8>,
}

impl VscpV2Pdu {
    pub fn parse(buf: &[u8]) -> VivoProtocolResult<Self> {
        if buf.len() < VSCP_V2_OVERHEAD {
            return Err(VivoProtocolError::InvalidFrame("PDU too short"));
        }
        let payload_len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
        let total = payload_len + VSCP_V2_OVERHEAD;
        if buf.len() != total {
            return Err(VivoProtocolError::LengthMismatch {
                declared: total,
                actual: buf.len(),
            });
        }
        let pdu = Self {
            bytes: buf.to_vec(),
        };
        let version = pdu.version();
        if version != VSCP_V2_VERSION {
            return Err(VivoProtocolError::UnsupportedVersion(version));
        }
        pdu.verify_crc()?;
        Ok(pdu)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn version(&self) -> u8 {
        (self.bytes[0] >> 4) & 0x0f
    }

    pub fn payload_len(&self) -> usize {
        u16::from_le_bytes([self.bytes[1], self.bytes[2]]) as usize
    }

    pub fn frame_count(&self) -> u8 {
        self.bytes[3].wrapping_add(1)
    }

    pub fn frame_index(&self) -> u8 {
        self.bytes[4]
    }

    pub fn identifier(&self) -> u16 {
        u16::from_le_bytes([self.bytes[5], self.bytes[6]])
    }

    pub fn chunk(&self) -> &[u8] {
        let end = VSCP_V2_HEADER_LEN + self.payload_len();
        &self.bytes[VSCP_V2_HEADER_LEN..end]
    }

    pub fn is_frame0(&self) -> bool {
        self.frame_index() == 0
    }

    pub fn bid(&self) -> Option<u8> {
        (self.is_frame0() && self.payload_len() >= 1).then_some(self.bytes[7])
    }

    pub fn encrypted(&self) -> Option<bool> {
        (self.is_frame0() && self.payload_len() >= 2).then_some((self.bytes[8] & 1) == 1)
    }

    pub fn cid(&self) -> Option<u8> {
        (self.is_frame0() && self.payload_len() >= 3).then_some(self.bytes[9])
    }

    pub fn declared_crc(&self) -> u16 {
        let n = self.bytes.len();
        u16::from_le_bytes([self.bytes[n - 2], self.bytes[n - 1]])
    }

    pub fn computed_crc(&self) -> u16 {
        vscp_crc16(&self.bytes[..self.bytes.len() - VSCP_V2_CRC_LEN])
    }

    pub fn verify_crc(&self) -> VivoProtocolResult<()> {
        let declared = self.declared_crc();
        let computed = self.computed_crc();
        if declared != computed {
            return Err(VivoProtocolError::CrcMismatch { declared, computed });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VscpSarConfig {
    pub pack_size: usize,
    pub max_data_length: usize,
}

impl Default for VscpSarConfig {
    fn default() -> Self {
        Self {
            pack_size: 247,
            max_data_length: 247,
        }
    }
}

impl VscpSarConfig {
    pub fn effective_frame_payload_len(&self) -> VivoProtocolResult<usize> {
        if self.pack_size < VSCP_V2_OVERHEAD + VSCP_V2_FRAME0_PREFIX_LEN {
            return Err(VivoProtocolError::InvalidFrame(
                "VSCP pack_size must be >= 12",
            ));
        }
        Ok(self.pack_size - VSCP_V2_OVERHEAD)
    }

    pub fn max_biz_payload_len(&self) -> usize {
        max_biz_payload_len(self.pack_size, self.max_data_length)
    }
}

pub fn max_biz_payload_len(pack_size: usize, max_data_len: usize) -> usize {
    if pack_size <= VSCP_V2_OVERHEAD {
        return 0;
    }
    let full = max_data_len / pack_size;
    let rem = max_data_len % pack_size;
    let total = full * (pack_size - VSCP_V2_OVERHEAD) + rem.saturating_sub(VSCP_V2_OVERHEAD);
    total.saturating_sub(VSCP_V2_FRAME0_PREFIX_LEN)
}

#[derive(Debug, Default)]
pub struct VscpStreamDecoder {
    buffer: Vec<u8>,
    max_pack_size: usize,
}

impl VscpStreamDecoder {
    pub fn new(max_pack_size: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_pack_size,
        }
    }

    pub fn push(&mut self, data: &[u8]) -> VivoProtocolResult<Vec<VscpV2Pdu>> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        let mut idx = 0usize;

        while idx + VSCP_V2_HEADER_LEN <= self.buffer.len() {
            if ((self.buffer[idx] >> 4) & 0x0f) != VSCP_V2_VERSION {
                idx += 1;
                continue;
            }

            let payload_len =
                u16::from_le_bytes([self.buffer[idx + 1], self.buffer[idx + 2]]) as usize;
            let total = payload_len + VSCP_V2_OVERHEAD;
            if self.max_pack_size > 0 && total > self.max_pack_size {
                idx += 1;
                continue;
            }
            if idx + total > self.buffer.len() {
                break;
            }

            let candidate = &self.buffer[idx..idx + total];
            match VscpV2Pdu::parse(candidate) {
                Ok(pdu) => {
                    out.push(pdu);
                    idx += total;
                }
                Err(VivoProtocolError::CrcMismatch { .. }) => {
                    idx += 1;
                }
                Err(err) => return Err(err),
            }
        }

        if idx > 0 {
            self.buffer.drain(0..idx);
        }

        Ok(out)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

#[derive(Debug, Default)]
pub struct VscpReassembler {
    sessions: HashMap<u16, ReassemblySession>,
}

#[derive(Debug)]
struct ReassemblySession {
    frames: Vec<Option<VscpV2Pdu>>,
}

impl VscpReassembler {
    pub fn push(&mut self, pdu: VscpV2Pdu) -> VivoProtocolResult<Option<VscpMessage>> {
        if pdu.frame_index() >= pdu.frame_count() {
            return Err(VivoProtocolError::InvalidFrame("frame index out of range"));
        }
        let id = pdu.identifier();
        let frame_count = pdu.frame_count() as usize;
        if frame_count == 1 {
            return reassemble_frames(vec![pdu]).map(Some);
        }

        let session = self
            .sessions
            .entry(id)
            .or_insert_with(|| ReassemblySession {
                frames: vec![None; frame_count],
            });
        if session.frames.len() != frame_count {
            self.sessions.remove(&id);
            return Err(VivoProtocolError::InvalidFrame(
                "frame count changed within session",
            ));
        }

        let frame_index = pdu.frame_index() as usize;
        session.frames[frame_index] = Some(pdu);
        if session.frames.iter().any(Option::is_none) {
            return Ok(None);
        }

        let session = self
            .sessions
            .remove(&id)
            .ok_or(VivoProtocolError::InvalidFrame(
                "missing reassembly session",
            ))?;
        let frames = session
            .frames
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(VivoProtocolError::InvalidFrame(
                "incomplete reassembly session",
            ))?;
        reassemble_frames(frames).map(Some)
    }
}

fn reassemble_frames(frames: Vec<VscpV2Pdu>) -> VivoProtocolResult<VscpMessage> {
    let first = frames
        .first()
        .ok_or(VivoProtocolError::InvalidFrame("no frames to reassemble"))?;
    if !first.is_frame0() {
        return Err(VivoProtocolError::InvalidFrame(
            "first frame is not frame 0",
        ));
    }
    let bid = first
        .bid()
        .ok_or(VivoProtocolError::InvalidFrame("frame 0 missing bid"))?;
    let cid = first
        .cid()
        .ok_or(VivoProtocolError::InvalidFrame("frame 0 missing cid"))?;
    let encrypted = first.encrypted().ok_or(VivoProtocolError::InvalidFrame(
        "frame 0 missing control byte",
    ))?;

    let mut stream = Vec::new();
    for frame in frames {
        stream.extend_from_slice(frame.chunk());
    }
    if stream.len() < VSCP_V2_FRAME0_PREFIX_LEN {
        return Err(VivoProtocolError::InvalidFrame(
            "reassembled stream too short",
        ));
    }
    let body = &stream[VSCP_V2_FRAME0_PREFIX_LEN..];
    let payload = if encrypted {
        vscp_decrypt(body)?
    } else {
        body.to_vec()
    };

    Ok(VscpMessage {
        bid,
        cid,
        encrypted,
        payload,
    })
}

#[derive(Debug)]
pub struct VscpSar {
    config: VscpSarConfig,
    next_identifier: u16,
    decoder: VscpStreamDecoder,
    reassembler: VscpReassembler,
}

impl VscpSar {
    pub fn new(config: VscpSarConfig) -> Self {
        Self {
            decoder: VscpStreamDecoder::new(config.pack_size),
            config,
            next_identifier: 0,
            reassembler: VscpReassembler::default(),
        }
    }

    pub fn config(&self) -> &VscpSarConfig {
        &self.config
    }

    pub fn update_config(&mut self, config: VscpSarConfig) {
        self.decoder.max_pack_size = config.pack_size;
        self.config = config;
    }

    pub fn encode_message(&mut self, message: &VscpMessage) -> VivoProtocolResult<Vec<VscpV2Pdu>> {
        let id = self.next_identifier;
        self.next_identifier = self.next_identifier.wrapping_add(1);
        encode_message_with_identifier(&self.config, id, message)
    }

    pub fn push_bytes(&mut self, data: &[u8]) -> VivoProtocolResult<Vec<VscpMessage>> {
        let pdus = self.decoder.push(data)?;
        let mut messages = Vec::new();
        for pdu in pdus {
            if let Some(message) = self.reassembler.push(pdu)? {
                messages.push(message);
            }
        }
        Ok(messages)
    }
}

pub fn encode_message_with_identifier(
    config: &VscpSarConfig,
    identifier: u16,
    message: &VscpMessage,
) -> VivoProtocolResult<Vec<VscpV2Pdu>> {
    let effective = config.effective_frame_payload_len()?;
    let payload = if message.encrypted {
        vscp_encrypt(&message.payload)?
    } else {
        message.payload.clone()
    };
    let mut stream = vec![0u8; VSCP_V2_FRAME0_PREFIX_LEN];
    stream.extend_from_slice(&payload);
    let frame_count = stream.len().div_ceil(effective);
    if frame_count == 0 || frame_count > 256 {
        return Err(VivoProtocolError::InvalidFrame(
            "unsupported VSCP frame count",
        ));
    }

    let mut frames = Vec::with_capacity(frame_count);
    for (idx, chunk) in stream.chunks(effective).enumerate() {
        let mut bytes = vec![0u8; chunk.len() + VSCP_V2_OVERHEAD];
        bytes[0] = VSCP_V2_VERSION << 4;
        let len = (chunk.len() as u16).to_le_bytes();
        bytes[1] = len[0];
        bytes[2] = len[1];
        bytes[3] = (frame_count - 1) as u8;
        bytes[4] = idx as u8;
        let id = identifier.to_le_bytes();
        bytes[5] = id[0];
        bytes[6] = id[1];
        bytes[VSCP_V2_HEADER_LEN..VSCP_V2_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        if idx == 0 {
            bytes[7] = message.bid;
            bytes[8] = u8::from(message.encrypted);
            bytes[9] = message.cid;
        }
        let crc = vscp_crc16(&bytes[..bytes.len() - VSCP_V2_CRC_LEN]).to_le_bytes();
        let n = bytes.len();
        bytes[n - 2] = crc[0];
        bytes[n - 1] = crc[1];
        frames.push(VscpV2Pdu::parse(&bytes)?);
    }

    Ok(frames)
}
