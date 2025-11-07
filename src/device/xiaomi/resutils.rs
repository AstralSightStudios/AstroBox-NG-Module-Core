use serde_repr::Serialize_repr;

use crate::device::xiaomi::{config::ResConfig, packet::mass::MassDataType};

pub fn get_watchface_id(data: &[u8], config: &ResConfig) -> Option<String> {
    let offset = config.watchface_id_offset;
    let field_len = config.watchface_id_field_len;
    if data.len() < offset + field_len {
        return None;
    }
    let field = &data[offset..offset + field_len];

    let start = field.iter().position(|&b| (b as char).is_ascii_digit())?;
    let digits: String = field[start..]
        .iter()
        .take_while(|&&b| (b as char).is_ascii_digit())
        .map(|&b| b as char)
        .collect();

    Some(digits)
}

#[derive(Clone, Copy, Serialize_repr, PartialEq)]
#[repr(u8)]
pub enum FileType {
    Text,
    Zip,
    Binary,
    Null,
    // 又接暗广我服了。
    Abp = 91,
    WatchFace = MassDataType::Watchface as u8,
    Firmware = MassDataType::Firmare as u8,
    ThirdPartyApp = MassDataType::ThirdPartyApp as u8,
}
pub fn get_file_type(data: &[u8]) -> FileType {
    if data.is_empty() {
        return FileType::Null;
    }
    // 1. 检查是不是 ZIP 格式
    if data.len() >= 4 && &data[..4] == [0x50, 0x4B, 0x03, 0x04] {
        /* // 检查扩展名 abp
        if let Some(ext) = filename.extension() {
            if ext == "abp" {
                return Ok("abp".to_string());
            }
        } */
        // 检查尾部是否包含 quickapp 字样
        let tail = if data.len() > 256 {
            &data[data.len() - 256..]
        } else {
            &data[..]
        };
        if String::from_utf8_lossy(tail).contains("toolkit") {
            return FileType::ThirdPartyApp;
        } else {
            return FileType::Zip;
        }
    }

    // 2. 检查是不是文本（utf8）
    if std::str::from_utf8(data).is_ok() {
        return FileType::Text;
    }

    // 3. 检查小米表盘魔数 5a a5 34 12
    if data.len() >= 4 && &data[..4] == [0x5a, 0xa5, 0x34, 0x12] {
        return FileType::WatchFace;
    }

    // 4. 其它都认为是二进制
    FileType::Binary
}
