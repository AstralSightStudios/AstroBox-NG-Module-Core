use serde_repr::Serialize_repr;

use crate::device::xiaomi::{config::ResConfig, packet::mass::MassDataType};

const VALID_WATCHFACE_ID_LENGTHS: [usize; 2] = [9, 12];

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

pub fn set_watchface_id(data: &mut [u8], config: &ResConfig, new_id: &str) -> Result<(), String> {
    if !new_id.as_bytes().iter().all(u8::is_ascii_digit) {
        return Err("Watchface ID must contain only digits".to_string());
    }
    if !VALID_WATCHFACE_ID_LENGTHS.contains(&new_id.len()) {
        return Err("Watchface ID must be 9 or 12 digits".to_string());
    }

    let offset = config.watchface_id_offset;
    let field_len = config.watchface_id_field_len;
    let field_end = offset
        .checked_add(field_len)
        .ok_or("Watchface ID field range overflow")?;

    if data.len() < field_end {
        return Err("Data too short to contain watchface ID field".to_string());
    }

    let field = &data[offset..field_end];
    let start = field
        .iter()
        .position(|&b| (b as char).is_ascii_digit())
        .ok_or("No digits found in watchface ID field")?;

    if start + new_id.len() > field_len {
        return Err(format!(
            "Watchface ID field is too short for {} digits",
            new_id.len()
        ));
    }

    let id_start = offset + start;
    let id_bytes = new_id.as_bytes();
    let id_end = id_start + id_bytes.len();
    data[id_start..id_end].copy_from_slice(id_bytes);
    data[id_end..field_end].fill(0);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ResConfig {
        ResConfig {
            watchface_id_offset: 4,
            watchface_id_field_len: 24,
        }
    }

    fn data_with_field(field: &[u8]) -> Vec<u8> {
        let config = test_config();
        let mut data = vec![0xaa; config.watchface_id_offset + config.watchface_id_field_len];
        data[config.watchface_id_offset
            ..config.watchface_id_offset + config.watchface_id_field_len]
            .fill(0);
        data[config.watchface_id_offset..config.watchface_id_offset + field.len()]
            .copy_from_slice(field);
        data
    }

    #[test]
    fn set_watchface_id_expands_9_to_12_digits() {
        let config = test_config();
        let mut data = data_with_field(b"123456789");

        set_watchface_id(&mut data, &config, "987654321012").unwrap();

        assert_eq!(
            get_watchface_id(&data, &config),
            Some("987654321012".to_string())
        );
        assert!(data[16..28].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn set_watchface_id_shrinks_12_to_9_digits_and_clears_tail() {
        let config = test_config();
        let mut data = data_with_field(b"123456789012");

        set_watchface_id(&mut data, &config, "987654321").unwrap();

        assert_eq!(
            get_watchface_id(&data, &config),
            Some("987654321".to_string())
        );
        assert_eq!(data[13], 0);
        assert!(data[13..28].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn set_watchface_id_preserves_prefix_before_digit_run() {
        let config = test_config();
        let mut field = b"ab\0".to_vec();
        field.extend_from_slice(b"123456789");
        let mut data = data_with_field(&field);

        set_watchface_id(&mut data, &config, "111222333444").unwrap();

        assert_eq!(&data[4..7], b"ab\0");
        assert_eq!(&data[7..19], b"111222333444");
    }

    #[test]
    fn set_watchface_id_rejects_unsupported_length() {
        let config = test_config();
        let mut data = data_with_field(b"123456789");

        let err = set_watchface_id(&mut data, &config, "123").unwrap_err();

        assert_eq!(err, "Watchface ID must be 9 or 12 digits");
    }
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
        let tail = &data[..];

        if String::from_utf8_lossy(tail).contains("toolkit")
            || String::from_utf8_lossy(tail).contains("manifest-watch.json")
        {
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
