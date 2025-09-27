use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum L1DataType {
    Nak = 0,
    Ack = 1,
    Cmd = 2,
    Data = 3,
}

impl core::convert::TryFrom<u8> for L1DataType {
    type Error = L1Error;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(L1DataType::Nak),
            1 => Ok(L1DataType::Ack),
            2 => Ok(L1DataType::Cmd),
            3 => Ok(L1DataType::Data),
            _ => Err(L1Error::InvalidType(v)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct L1Packet {
    pub pkt_type: L1DataType,
    pub frx: bool,
    pub seq: u8,
    pub length: u16,
    pub crc: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L1Error {
    TooShort,
    BadMagic { found: u16 },
    InvalidType(u8),
    LengthMismatch { declared: u16, actual: usize },
    CrcMismatch { declared: u16, computed: u16 },
}

impl fmt::Display for L1Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            L1Error::TooShort => write!(f, "Packet too short"),
            L1Error::BadMagic { found } => write!(f, "Bad magic value: found {:#06x}", found),
            L1Error::InvalidType(t) => write!(f, "Invalid type: {}", t),
            L1Error::LengthMismatch { declared, actual } => {
                write!(
                    f,
                    "Length mismatch: declared {}, actual {}",
                    declared, actual
                )
            }
            L1Error::CrcMismatch { declared, computed } => {
                write!(
                    f,
                    "CRC mismatch: declared {}, computed {}",
                    declared, computed
                )
            }
        }
    }
}

impl L1Packet {
    pub const MAGIC: u16 = 0xA5A5;
    const TYPE_MASK: u8 = 0x0F;
    const FRX_MASK: u8 = 0x10;

    pub fn new(pkt_type: L1DataType, frx: bool, seq: u8, payload: Vec<u8>) -> Self {
        let mut pkt = Self {
            pkt_type,
            frx,
            seq,
            length: payload.len() as u16,
            crc: 0,
            payload,
        };
        pkt.update_crc();
        pkt
    }

    pub fn crc16_arc(data: &[u8]) -> u16 {
        let mut crc: u16 = 0x0000;
        for &byte in data {
            crc ^= byte as u16;
            for _ in 0..8 {
                if (crc & 0x0001) != 0 {
                    crc = (crc >> 1) ^ 0xA001;
                } else {
                    crc >>= 1;
                }
            }
        }
        crc
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + 1 + 1 + 2 + 2 + self.payload.len());

        // magic
        out.extend_from_slice(&Self::MAGIC.to_le_bytes());

        // type|frx
        out.push(Self::pack_type_frx(self.pkt_type, self.frx));

        // seq
        out.push(self.seq);

        // length
        out.extend_from_slice(&self.length.to_le_bytes());

        // crc
        out.extend_from_slice(&self.crc.to_le_bytes());

        // payload
        out.extend_from_slice(&self.payload);

        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, L1Error> {
        // min len = 2(magic)+1(type|frx)+1(seq)+2(len)+2(crc) = 8
        if buf.len() < 8 {
            return Err(L1Error::TooShort);
        }

        // magic
        let magic = u16::from_le_bytes([buf[0], buf[1]]);
        if magic != Self::MAGIC {
            return Err(L1Error::BadMagic { found: magic });
        }

        // type|frx
        let tf = buf[2];
        let (pkt_type, frx) = Self::unpack_type_frx(tf)?;

        // seq
        let seq = buf[3];

        // length
        let length = u16::from_le_bytes([buf[4], buf[5]]);

        // crc
        let declared_crc = u16::from_le_bytes([buf[6], buf[7]]);

        // payload
        let expected_total = 8usize + length as usize;
        if buf.len() < expected_total {
            return Err(L1Error::LengthMismatch {
                declared: length,
                actual: buf.len().saturating_sub(8),
            });
        }
        let payload = buf[8..8 + length as usize].to_vec();

        // 校验一下CRC，看看包是否完整
        // 小米笑转之传错包
        let computed_crc = Self::crc16_arc(&payload);

        if declared_crc != computed_crc {
            return Err(L1Error::CrcMismatch {
                declared: declared_crc,
                computed: computed_crc,
            });
        }

        Ok(Self {
            pkt_type,
            frx,
            seq,
            length,
            crc: declared_crc,
            payload,
        })
    }

    // 更新crc（基于当前字段）
    pub fn update_crc(&mut self) {
        self.length = self.payload.len() as u16;
        self.crc = Self::crc16_arc(&self.payload);
    }

    // 同上但不修改自身
    pub fn computed_crc(&self) -> u16 {
        Self::crc16_arc(&self.payload)
    }

    pub fn verify_crc(&self) -> bool {
        self.crc == self.computed_crc()
    }

    #[inline]
    pub fn pack_type_frx(t: L1DataType, frx: bool) -> u8 {
        let mut b = (t as u8) & Self::TYPE_MASK;
        if frx {
            b |= Self::FRX_MASK;
        }
        b // 高位 bit5..bit7 置 0
    }

    #[inline]
    pub fn unpack_type_frx(b: u8) -> Result<(L1DataType, bool), L1Error> {
        let ty = L1DataType::try_from(b & Self::TYPE_MASK)?;
        let frx = (b & Self::FRX_MASK) != 0; // 仅看 bit4
        Ok((ty, frx))
    }
}
