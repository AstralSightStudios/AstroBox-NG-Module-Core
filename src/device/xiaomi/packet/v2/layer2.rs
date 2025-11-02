use core::convert::TryFrom;
use pb::xiaomi::protocol::WearPacket;
use prost::Message;
use std::fmt;

use crate::device::xiaomi::packet::v2::layer1::{L1DataType, L1Packet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum L2Channel {
    Pb = 1,          // TRANSPORT_CHANNEL_PB
    Mass = 2,        // TRANSPORT_CHANNEL_MASS
    MassVoice = 3,   // TRANSPORT_CHANNEL_MASS_VOICE
    FileSensor = 4,  // TRANSPORT_CHANNEL_FILE_SENSOR
    FileFitness = 5, // TRANSPORT_CHANNEL_FILE_FITNESS
    Ota = 6,         // TRANSPORT_CHANNEL_OTA
    Network = 7,     // TRANSPORT_CHANNEL_NETWORK
    Lyra = 8,        // TRANSPORT_CHANNEL_LYRA
    Research = 9,    // TRANSPORT_CHANNEL_RESEARCH
}

impl TryFrom<u8> for L2Channel {
    type Error = L2Error;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        use L2Channel::*;
        Ok(match v {
            1 => Pb,
            2 => Mass,
            3 => MassVoice,
            4 => FileSensor,
            5 => FileFitness,
            6 => Ota,
            7 => Network,
            8 => Lyra,
            9 => Research,
            _ => return Err(L2Error::InvalidChannel(v)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum L2OpCode {
    Write = 1,    // TRANSPORT_OPCODE_WRITE
    WriteEnc = 2, // TRANSPORT_OPCODE_WRITE_ENC
    Read = 3,     // TRANSPORT_OPCODE_READ
}

impl TryFrom<u8> for L2OpCode {
    type Error = L2Error;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        use L2OpCode::*;
        Ok(match v {
            1 => Write,
            2 => WriteEnc,
            3 => Read,
            _ => return Err(L2Error::InvalidOpCode(v)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L2Error {
    TooShort,
    InvalidChannel(u8),
    InvalidOpCode(u8),
    LengthMismatch { expected: usize, actual: usize }, // 目前不会出现（L2 自身没有显式长度），但是留着，因为可扩展性这一块。
    DecryptFailed,
}

impl fmt::Display for L2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            L2Error::TooShort => write!(f, "Packet too short"),
            L2Error::InvalidChannel(ch) => write!(f, "Invalid channel: {}", ch),
            L2Error::InvalidOpCode(op) => write!(f, "Invalid opcode: {}", op),
            L2Error::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "Length mismatch: expected {}, actual {}",
                    expected, actual
                )
            }
            L2Error::DecryptFailed => write!(f, "Decryption failed"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct L2Packet {
    pub channel: L2Channel,
    pub opcode: L2OpCode,
    pub payload: Vec<u8>,
}

impl L2Packet {
    pub fn new(channel: L2Channel, opcode: L2OpCode, payload: Vec<u8>) -> Self {
        Self {
            channel,
            opcode,
            payload,
        }
    }

    /// 1B channel | 1B opcode | N bytes payload
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.payload.len());
        out.push(self.channel as u8);
        out.push(self.opcode as u8);
        out.extend_from_slice(&self.payload);
        out
    }

    /// 支持可选解密：
    /// `opcode == WriteEnc` 且提供了 `cipher`，则对 payload 解密后返回明文。
    /// `opcode == WriteEnc` 但没有提供 `cipher`，则保留密文原样放在 payload 中，由上层决定何时解密。
    pub fn from_bytes(buf: &[u8], cipher: Option<&dyn L2Cipher>) -> Result<Self, L2Error> {
        if buf.len() < 2 {
            return Err(L2Error::TooShort);
        }
        let ch = L2Channel::try_from(buf[0])?;
        let op = L2OpCode::try_from(buf[1])?;
        let body = &buf[2..];

        let payload = match (op, cipher) {
            (L2OpCode::WriteEnc, Some(c)) => c.decrypt(body).map_err(|_| L2Error::DecryptFailed)?,
            _ => body.to_vec(),
        };

        Ok(Self {
            channel: ch,
            opcode: op,
            payload,
        })
    }

    pub fn pb_write(packet: WearPacket) -> Self {
        #[cfg(not(target_os = "espidf"))]
        log::info!("l2_pb_write: {}", serde_json::to_string(&packet).unwrap());
        Self::new(L2Channel::Pb, L2OpCode::Write, packet.encode_to_vec())
    }

    pub fn pb_write_enc(packet: WearPacket, cipher: &dyn L2Cipher) -> Result<Self, L2Error> {
        #[cfg(not(target_os = "espidf"))]
        log::info!(
            "l2_pb_write_enc: {}",
            serde_json::to_string(&packet).unwrap()
        );
        let ct = cipher
            .encrypt(&packet.encode_to_vec())
            .map_err(|_| L2Error::DecryptFailed)?;
        Ok(Self::new(L2Channel::Pb, L2OpCode::WriteEnc, ct))
    }

    pub fn into_l1(self, seq: u8, frx: bool) -> L1Packet {
        L1Packet::new(L1DataType::Data, frx, seq, self.to_bytes())
    }

    pub fn from_l1(l1: &L1Packet, cipher: Option<&dyn L2Cipher>) -> Result<Self, L2Error> {
        match l1.pkt_type {
            L1DataType::Data => L2Packet::from_bytes(&l1.payload, cipher),
            _ => Err(L2Error::InvalidOpCode(0)),
        }
    }
}

pub trait L2Cipher {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, ()>;
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, ()>;
}
