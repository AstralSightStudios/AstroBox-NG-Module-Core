use std::collections::HashMap;

use anyhow::{anyhow, Result};
use byteorder::{LittleEndian, WriteBytesExt};
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize, Serializer};

use crate::tools::{calc_crc32_bytes, calc_md5, to_hex_string};

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum MassDataType {
    WATCHFACE = 16,
    FIRMWARE = 32,
    NotificationIcon = 50,
    ThirdpartyApp = 64,
}

impl From<MassDataType> for u8 {
    fn from(value: MassDataType) -> Self {
        value as u8
    }
}

impl TryFrom<u8> for MassDataType {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            16 => Ok(MassDataType::WATCHFACE),
            32 => Ok(MassDataType::FIRMWARE),
            50 => Ok(MassDataType::NotificationIcon),
            64 => Ok(MassDataType::ThirdpartyApp),
            _ => Err("invalid MassDataType value"),
        }
    }
}

impl Serialize for MassDataType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8((*self).into())
    }
}

impl<'de> Deserialize<'de> for MassDataType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        MassDataType::try_from(value).map_err(|e| D::Error::custom(e))
    }
}

#[derive(Clone)]
pub struct MassPacket {
    pub data_type: MassDataType,
    pub md5: Vec<u8>,
    pub length: u32,
    pub original_file_data: Vec<u8>,
}

impl MassPacket {
    pub fn build(original_file_data: Vec<u8>, data_type: MassDataType) -> Result<Self> {
        Ok(MassPacket {
            data_type,
            md5: calc_md5(&original_file_data),
            length: original_file_data.len() as u32,
            original_file_data,
        })
    }

    /// Encode the internal payload and append CRC32 at the end.
    /// Format: comp_data (1B) | data_type (1B) | md5 (16B) | length (4B LE) |
    /// original_file_data (...) | crc32_of_previous_fields (4B LE)
    pub fn encode_with_crc32(&self) -> Vec<u8> {
        let mut crc_payload_buf =
            Vec::with_capacity(1 + 1 + self.md5.len() + 4 + self.original_file_data.len() + 4);

        crc_payload_buf.push(0x00);
        crc_payload_buf.push(self.data_type as u8);
        crc_payload_buf.extend_from_slice(&self.md5);
        crc_payload_buf
            .write_u32::<LittleEndian>(self.length)
            .unwrap();
        crc_payload_buf.extend_from_slice(&self.original_file_data);

        let crc32_val = u32::from_be_bytes(calc_crc32_bytes(&crc_payload_buf));
        crc_payload_buf
            .write_u32::<LittleEndian>(crc32_val)
            .unwrap();

        crc_payload_buf
    }
}

#[derive(Clone)]
pub struct ReverseMassPacket {
    pub file_name: String,
    pub total_part: u32,
    pub cur_part: u32,
    pub header: Vec<u8>,
    pub file: HashMap<u32, Vec<u8>>,
    pub error: bool,
    pub empty: bool,
}

impl ReverseMassPacket {
    pub fn new() -> Self {
        ReverseMassPacket {
            file_name: String::new(),
            total_part: 0,
            cur_part: 0,
            file: HashMap::new(),
            error: false,
            empty: true,
            header: Vec::new(),
        }
    }

    pub fn handle_packet(&mut self, packet: Vec<u8>) -> Result<()> {
        self.empty = false;

        if packet.len() < 12 {
            self.error = true;
            return Err(anyhow!("Invalid reverse mass packet"));
        }

        if packet[0] != 0 {
            self.error = true;
            return Err(anyhow!("Invalid packet version"));
        }

        let total = u16::from_le_bytes(packet[2..4].try_into().unwrap());
        let cur = u16::from_le_bytes(packet[4..6].try_into().unwrap());

        let skip_offset: usize;

        if self.total_part == 0 {
            let len = packet[6] as usize;
            let file_name = String::from_utf8(packet[7..7 + len].to_vec())?;
            self.file_name = file_name;
            self.header = packet[6..7 + len + 5].to_vec();
            skip_offset = 7 + len + 5;
        } else {
            if total as u32 != self.total_part {
                self.error = true;
                return Err(anyhow!("Invalid total {} != {}", total, self.total_part));
            }
            skip_offset = 6;
        }

        if cur == total {
            self.file
                .insert(cur as u32, packet[skip_offset..packet.len() - 4].to_vec());

            let mut check_data: Vec<u8> = Vec::new();
            check_data.extend(self.header.clone());
            check_data.extend(self.file(true)?);

            let crc32 = u32::from_le_bytes(packet[packet.len() - 4..].try_into().unwrap());
            let data_crc32 = u32::from_be_bytes(calc_crc32_bytes(&check_data));

            if crc32 != data_crc32 {
                log::error!(
                    "[ReverseMassPacket] Invalid crc32! check_data: {}",
                    to_hex_string(&check_data)
                );
                self.error = true;
                return Err(anyhow!("Invalid crc32 {} != {}", crc32, data_crc32));
            }
        } else {
            self.file.insert(cur as u32, packet[skip_offset..].to_vec());
        }

        self.total_part = total as u32;
        self.cur_part += 1;
        Ok(())
    }

    pub fn complete(&self) -> bool {
        self.cur_part == self.total_part && self.total_part != 0
    }

    pub fn file(&self, force: bool) -> Result<Vec<u8>> {
        if !self.complete() && !force {
            return Err(anyhow!("complete != true"));
        }

        let mut result: Vec<u8> = Vec::new();
        let total = self.total_block();
        for key in 1..=total {
            match self.file.get(&key) {
                Some(vec) => result.extend_from_slice(vec),
                None => return Err(anyhow!("block {} was not found!", key)),
            }
        }
        Ok(result)
    }

    pub fn empty(&self) -> bool {
        self.empty
    }

    pub fn error(&self) -> bool {
        self.error
    }

    pub fn current_block(&self) -> u32 {
        self.cur_part
    }

    pub fn total_block(&self) -> u32 {
        self.total_part
    }

    pub fn file_name(&self) -> String {
        self.file_name.clone()
    }
}
