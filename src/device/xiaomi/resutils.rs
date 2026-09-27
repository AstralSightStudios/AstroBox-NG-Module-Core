use serde_repr::Serialize_repr;
use std::io::{Cursor, Read, Write};

use crate::device::{
    crpack::is_crpack_archive,
    xiaomi::{config::ResConfig, packet::mass::MassDataType},
};

const VALID_WATCHFACE_ID_LENGTHS: [usize; 2] = [9, 12];
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

#[inline]
pub fn is_zip(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..4] == ZIP_MAGIC
}

fn is_valid_watchface_id_str(s: &str) -> bool {
    let bytes = s.as_bytes();
    VALID_WATCHFACE_ID_LENGTHS.contains(&bytes.len())
        && bytes.iter().all(u8::is_ascii_alphanumeric)
}

fn extract_xml_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag);
    let close_tag = format!("</{}>", tag);
    let start_pos = xml.find(&open_tag)?;
    let content_start = start_pos + open_tag.len();
    let end_pos = xml[content_start..].find(&close_tag)?;
    Some(xml[content_start..content_start + end_pos].trim().to_string())
}

fn replace_xml_tag_text(xml: &str, tag: &str, new_val: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag);
    let close_tag = format!("</{}>", tag);
    let start_pos = xml.find(&open_tag)?;
    let content_start = start_pos + open_tag.len();
    let end_pos = xml[content_start..].find(&close_tag)?;
    let actual_end = content_start + end_pos;
    let mut result = String::with_capacity(xml.len() + new_val.len());
    result.push_str(&xml[..content_start]);
    result.push_str(new_val);
    result.push_str(&xml[actual_end..]);
    Some(result)
}

fn extract_manifest_id(xml: &str) -> Option<String> {
    let id_pos = xml.find(" id=\"")?;
    let val_start = id_pos + 5;
    let quote_end = xml[val_start..].find('"')?;
    Some(xml[val_start..val_start + quote_end].trim().to_string())
}

fn replace_manifest_id(xml: &str, new_id: &str) -> String {
    if let Some(id_pos) = xml.find(" id=\"") {
        let val_start = id_pos + 5;
        if let Some(quote_end) = xml[val_start..].find('"') {
            let val_end = val_start + quote_end;
            let mut result = String::with_capacity(xml.len() + new_id.len());
            result.push_str(&xml[..val_start]);
            result.push_str(new_id);
            result.push_str(&xml[val_end..]);
            return result;
        }
    }
    xml.to_string()
}

pub fn get_watchface_id(data: &[u8], config: &ResConfig) -> Option<String> {
    if is_zip(data) {
        return get_mwz_watchface_id(data, config);
    }
    get_bin_watchface_id(data, config)
}

fn get_bin_watchface_id(data: &[u8], config: &ResConfig) -> Option<String> {
    let offset = config.watchface_id_offset;
    let field_len = config.watchface_id_field_len;
    if data.len() < offset + field_len {
        return None;
    }
    let field = &data[offset..offset + field_len];

    // 表盘 ID 可能是 9 位或 12 位的字母数字组合，前面可能存在非 ID 的填充字节。
    // 扫描字段中的字母数字连续段，返回第一个长度合法的段作为 ID。
    let mut i = 0;
    while i < field.len() {
        if !(field[i] as char).is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < field.len() && (field[i] as char).is_ascii_alphanumeric() {
            i += 1;
        }
        if VALID_WATCHFACE_ID_LENGTHS.contains(&(i - run_start)) {
            return Some(field[run_start..i].iter().map(|&b| b as char).collect());
        }
    }

    None
}

fn get_mwz_watchface_id(data: &[u8], config: &ResConfig) -> Option<String> {
    let reader = Cursor::new(data);
    let mut zip = zip::ZipArchive::new(reader).ok()?;

    // 1. 优先尝试从 description.xml 中读取 <_id> 或 <id>
    for i in 0..zip.len() {
        let Ok(mut entry) = zip.by_index(i) else { continue };
        let lower = entry.name().to_ascii_lowercase();
        if lower.ends_with("description.xml") {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                if let Some(id) = extract_xml_tag_text(&text, "_id")
                    .or_else(|| extract_xml_tag_text(&text, "id"))
                {
                    let trimmed = id.trim();
                    if is_valid_watchface_id_str(trimmed) {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    // 2. 尝试从 manifest.xml 中读取 id="..."
    for i in 0..zip.len() {
        let Ok(mut entry) = zip.by_index(i) else { continue };
        let lower = entry.name().to_ascii_lowercase();
        if lower.ends_with("manifest.xml") {
            let mut text = String::new();
            if entry.read_to_string(&mut text).is_ok() {
                if let Some(id) = extract_manifest_id(&text) {
                    let trimmed = id.trim();
                    if is_valid_watchface_id_str(trimmed) {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }

    // 3. 尝试从内部的 .bin 文件中读取
    let mut root_bin: Option<usize> = None;
    let mut any_bin: Option<usize> = None;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else { continue };
        if entry.is_dir() { continue; }
        let name = entry.name().to_ascii_lowercase();
        if name.ends_with(".bin") {
            if !name.contains('/') {
                root_bin.get_or_insert(i);
            }
            any_bin.get_or_insert(i);
        }
    }

    let bin_idx = root_bin.or(any_bin)?;
    let mut entry = zip.by_index(bin_idx).ok()?;
    let mut bin_buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bin_buf).ok()?;
    get_bin_watchface_id(&bin_buf, config)
}

pub fn set_watchface_id(data: &mut [u8], config: &ResConfig, new_id: &str) -> Result<(), String> {
    if is_zip(data) {
        let mut vec = data.to_vec();
        set_watchface_id_vec(&mut vec, config, new_id)?;
        if vec.len() == data.len() {
            data.copy_from_slice(&vec);
            return Ok(());
        } else {
            return Err("MWZ repack size changed, please use set_watchface_id_vec".to_string());
        }
    }
    set_bin_watchface_id(data, config, new_id)
}

fn set_bin_watchface_id(data: &mut [u8], config: &ResConfig, new_id: &str) -> Result<(), String> {
    let offset = config.watchface_id_offset;
    let field_len = config.watchface_id_field_len;
    let field_end = offset
        .checked_add(field_len)
        .ok_or("Watchface ID field range overflow")?;

    if data.len() < field_end {
        return Err("Data too short to contain watchface ID field".to_string());
    }

    let field = &data[offset..field_end];
    // 原始资源文件中的 ID 始终为数字，借此定位 ID 在字段中的起始位置。
    let start = field
        .iter()
        .position(|&b| (b as char).is_ascii_digit())
        .ok_or("No digits found in watchface ID field")?;

    if start + new_id.len() > field_len {
        return Err(format!(
            "Watchface ID field is too short for {} bytes",
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

pub fn set_watchface_id_vec(
    data: &mut Vec<u8>,
    config: &ResConfig,
    new_id: &str,
) -> Result<(), String> {
    if !is_zip(data) {
        return set_bin_watchface_id(data.as_mut_slice(), config, new_id);
    }

    let old_id = get_watchface_id(data, config);
    let reader = Cursor::new(&data);
    let mut zip_in = zip::ZipArchive::new(reader)
        .map_err(|e| format!("Failed to read MWZ zip archive: {}", e))?;

    struct EntryData {
        name: String,
        content: Vec<u8>,
        compression: zip::CompressionMethod,
        unix_mode: Option<u32>,
    }

    let mut entries = Vec::with_capacity(zip_in.len());
    for i in 0..zip_in.len() {
        let mut entry = zip_in
            .by_index(i)
            .map_err(|e| format!("Failed to read zip entry {}: {}", i, e))?;
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut content)
            .map_err(|e| format!("Failed to extract zip entry {}: {}", entry.name(), e))?;
        entries.push(EntryData {
            name: entry.name().to_string(),
            content,
            compression: entry.compression(),
            unix_mode: entry.unix_mode(),
        });
    }

    // 修改 entries 内容
    for entry in &mut entries {
        let lower_name = entry.name.to_ascii_lowercase();
        if lower_name.ends_with(".bin") {
            let _ = set_bin_watchface_id(&mut entry.content, config, new_id);
            if let Some(ref old) = old_id {
                if !old.is_empty() && entry.name.contains(old) {
                    entry.name = entry.name.replace(old, new_id);
                }
            }
        } else if lower_name.ends_with("description.xml") {
            if let Ok(text) = std::str::from_utf8(&entry.content) {
                let mut updated = text.to_string();
                if let Some(r) = replace_xml_tag_text(&updated, "_id", new_id) {
                    updated = r;
                }
                if let Some(r) = replace_xml_tag_text(&updated, "id", new_id) {
                    updated = r;
                }
                entry.content = updated.into_bytes();
            }
        } else if lower_name.ends_with("manifest.xml") {
            if let Ok(text) = std::str::from_utf8(&entry.content) {
                let updated = replace_manifest_id(text, new_id);
                entry.content = updated.into_bytes();
            }
        }
    }

    // 重新打包 zip
    let mut out_buf = Cursor::new(Vec::with_capacity(data.len()));
    {
        let mut zip_out = zip::ZipWriter::new(&mut out_buf);
        for entry in entries {
            let mut options = zip::write::FileOptions::default()
                .compression_method(entry.compression);
            if let Some(mode) = entry.unix_mode {
                options = options.unix_permissions(mode);
            }
            zip_out
                .start_file(&entry.name, options)
                .map_err(|e| format!("Failed to create zip entry {}: {}", entry.name, e))?;
            zip_out
                .write_all(&entry.content)
                .map_err(|e| format!("Failed to write zip entry {}: {}", entry.name, e))?;
        }
        zip_out
            .finish()
            .map_err(|e| format!("Failed to finalize MWZ zip: {}", e))?;
    }

    *data = out_buf.into_inner();
    Ok(())
}

/// 小米可穿戴固件的最小可信大小（字节）。小于此值的文件不会被识别为固件。
pub const MIN_FIRMWARE_SIZE: usize = 1_000_000;
/// 从文件头部读取的最大字节数，用于固件类型判断。
pub const MAX_FIRMWARE_SCAN_BYTES: usize = 200_000_000;

const FACTORY_MAGIC: &[u8] = b"\x60ZZ~";
const DDELTA_MAGIC_PREFIX: &[u8] = b"DDELTA";
const BSDIFF_MAGIC_PREFIX: &[u8] = b"ENDSLEY/BSDIFF";
const CONBINE_VERSION_PREFIX: &[u8] = b"CONBINE_";

/// 判断一段数据是否为小米可穿戴固件。
///
/// 同时覆盖两种形态：
/// - 工厂裸镜像：以 `\x60ZZ~` 开头，32 字节描述字段为数字版本号或 `CONBINE_`
///   描述符，含 `vela_ap.bin` 且出现多于一个 `PK\x03\x04`；
/// - OTA JAR：以 `PK\x03\x04` 开头，ZIP 条目中存在全量 `vela_ap.bin`，或存在
///   带版本化 `DDELTA` 或 `ENDSLEY/BSDIFF` 魔数的增量 `vela_ap.patch` 及对应
///   OTA 运行文件。
///
/// `full_size` 为文件原始大小，用于排除过小的文件。若传入 `None`，则使用 `data.len()`。
pub fn is_xiaomi_firmware(data: &[u8], full_size: Option<usize>) -> bool {
    let size = full_size.unwrap_or(data.len());
    if size < MIN_FIRMWARE_SIZE {
        return false;
    }

    let scan = &data[..data.len().min(MAX_FIRMWARE_SCAN_BYTES)];
    is_miwear_factory(scan) || is_miwear_ota(data)
}

/// 判断一段数据是否为小米可穿戴工厂裸镜像。
///
/// 匹配规则：
/// - 以 `\x60ZZ~` 开头；
/// - 紧跟 32 字节的描述字段，是仅由数字与 `.` 组成的版本号，或以 `CONBINE_` 开头；
/// - 数据中含 `vela_ap.bin`；
/// - 数据中出现多于一个 ZIP 本地文件头 `PK\x03\x04`。
fn is_miwear_factory(data: &[u8]) -> bool {
    if data.len() < FACTORY_MAGIC.len() + 32 {
        return false;
    }
    if &data[..FACTORY_MAGIC.len()] != FACTORY_MAGIC {
        return false;
    }

    let ver_field = &data[FACTORY_MAGIC.len()..FACTORY_MAGIC.len() + 32];
    let ver: Vec<u8> = ver_field.iter().take_while(|&&b| b != 0).copied().collect();
    let is_numeric_version = ver.iter().all(|&b| b.is_ascii_digit() || b == b'.');
    let is_conbine_descriptor = ver.starts_with(CONBINE_VERSION_PREFIX);
    if ver.is_empty() || (!is_numeric_version && !is_conbine_descriptor) {
        return false;
    }

    if !contains_subsequence(data, b"vela_ap.bin") {
        return false;
    }

    count_subsequence(data, ZIP_MAGIC) > 1
}

/// 判断一段数据是否为小米可穿戴 OTA ZIP/JAR。
///
/// 匹配规则：
/// - 以 `PK\x03\x04` 开头；
/// - 能作为 ZIP 打开；
/// - ZIP 条目中存在文件名为 `vela_ap.bin` 的全量镜像；或
/// - DDELTA 增量同时存在 `ota.sh`、`vela_ota.bin`，且 `vela_ap.patch` 以
///   `DDELTA` 加两位数字开头；或
/// - BSDIFF 增量同时存在 `ota.sh`，且 `vela_ap.patch` 以 `ENDSLEY/BSDIFF`
///   加两位数字开头（其 `bspatch` 由系统提供，无 `vela_ota.bin`）。
fn is_miwear_ota(data: &[u8]) -> bool {
    if data.len() < ZIP_MAGIC.len() {
        return false;
    }
    if &data[..ZIP_MAGIC.len()] != ZIP_MAGIC {
        return false;
    }

    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(data)) else {
        return false;
    };

    let mut has_ota_script = false;
    let mut has_ota_updater = false;
    let mut has_ddelta_ap_patch = false;
    let mut has_bsdiff_ap_patch = false;

    for index in 0..archive.len() {
        let Ok(file) = archive.by_index(index) else {
            continue;
        };
        let Some(name) = file.name().rsplit('/').next() else {
            continue;
        };

        match name {
            "vela_ap.bin" => return true,
            "ota.sh" => has_ota_script = true,
            "vela_ota.bin" => has_ota_updater = true,
            "vela_ap.patch" => {
                let mut magic = Vec::with_capacity(16);
                if file.take(16).read_to_end(&mut magic).is_ok() {
                    let ddelta_len = magic.len().min(8);
                    has_ddelta_ap_patch |=
                        is_versioned_magic(&magic[..ddelta_len], DDELTA_MAGIC_PREFIX);
                    has_bsdiff_ap_patch |= is_versioned_magic(&magic, BSDIFF_MAGIC_PREFIX);
                }
            }
            _ => {}
        }
    }

    has_ota_script && (has_bsdiff_ap_patch || (has_ota_updater && has_ddelta_ap_patch))
}

fn is_versioned_magic(magic: &[u8], prefix: &[u8]) -> bool {
    magic.len() == prefix.len() + 2
        && magic.starts_with(prefix)
        && magic[prefix.len()..].iter().all(u8::is_ascii_digit)
}

fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn count_subsequence(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return haystack.len() + 1;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

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
    fn set_watchface_id_accepts_arbitrary_utf8_string() {
        let config = test_config();
        let mut data = data_with_field(b"123456789");
        let new_id = "自定义/watchface-v2";

        set_watchface_id(&mut data, &config, new_id).unwrap();

        let id_start = config.watchface_id_offset;
        let id_end = id_start + new_id.len();
        assert_eq!(&data[id_start..id_end], new_id.as_bytes());
        assert!(data[id_end..28].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn set_watchface_id_rejects_only_when_field_is_too_short() {
        let config = test_config();
        let mut data = data_with_field(b"123456789");
        let new_id = "x".repeat(config.watchface_id_field_len + 1);

        let err = set_watchface_id(&mut data, &config, &new_id).unwrap_err();

        assert_eq!(err, "Watchface ID field is too short for 25 bytes");
    }

    #[test]
    fn set_watchface_id_accepts_alphanumeric_9() {
        let config = test_config();
        let mut data = data_with_field(b"123456789");

        set_watchface_id(&mut data, &config, "aB3dE6gH9").unwrap();

        assert_eq!(
            get_watchface_id(&data, &config),
            Some("aB3dE6gH9".to_string())
        );
    }

    #[test]
    fn set_watchface_id_accepts_alphanumeric_12() {
        let config = test_config();
        let mut data = data_with_field(b"123456789");

        set_watchface_id(&mut data, &config, "aB3dE6gH9jK2").unwrap();

        assert_eq!(
            get_watchface_id(&data, &config),
            Some("aB3dE6gH9jK2".to_string())
        );
    }

    fn firmware_sized(payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; MIN_FIRMWARE_SIZE];
        let len = payload.len().min(data.len());
        data[..len].copy_from_slice(&payload[..len]);
        data
    }

    fn factory_firmware(version: &[u8]) -> Vec<u8> {
        assert!(version.len() <= 32);
        let mut payload = Vec::new();
        payload.extend_from_slice(FACTORY_MAGIC);
        payload.extend_from_slice(version);
        payload.resize(FACTORY_MAGIC.len() + 32, 0);
        payload.extend_from_slice(ZIP_MAGIC);
        payload.extend_from_slice(b"vela_ap.bin");
        payload.extend_from_slice(ZIP_MAGIC);
        firmware_sized(&payload)
    }

    fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn zip_with_entry(name: &str, data_len: usize) -> Vec<u8> {
        zip_with_entries(&[(name, &vec![0u8; data_len])])
    }

    fn zip_with_unreadable_first_entry() -> Vec<u8> {
        let mut data = zip_with_entries(&[
            ("unsupported.bin", b"not firmware"),
            ("vela_ap.bin", &vec![0u8; MIN_FIRMWARE_SIZE]),
        ]);
        data[6..8].copy_from_slice(&1u16.to_le_bytes());
        let central_header = data
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        data[central_header + 8..central_header + 10].copy_from_slice(&1u16.to_le_bytes());
        data
    }

    #[test]
    fn recognizes_miwear_factory_raw() {
        let data = factory_firmware(b"1.0.0");

        assert!(is_xiaomi_firmware(&data, Some(data.len())));
    }

    #[test]
    fn recognizes_miwear_conbine_incremental_container() {
        let data = factory_firmware(b"CONBINE_LTALM057_T1.1.182_112815");

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn recognizes_miwear_ota_jar() {
        let data = zip_with_entry("vela_ap.bin", 1);

        assert!(is_xiaomi_firmware(&data, Some(MIN_FIRMWARE_SIZE)));
    }

    #[test]
    fn rejects_too_small() {
        let payload = b"PK\x03\x04vela_ap.binvela_bl2.binota.sh";
        assert!(!is_xiaomi_firmware(payload, Some(payload.len())));
    }

    #[test]
    fn rejects_factory_with_single_zip() {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x60ZZ~");
        payload.extend_from_slice(b"1.0.0");
        payload.resize(payload.len() + (32 - 5), 0);
        payload.extend_from_slice(b"PK\x03\x04");
        payload.extend_from_slice(b"vela_ap.bin");

        let data = firmware_sized(&payload);

        assert!(!is_xiaomi_firmware(&data, Some(data.len())));
    }

    #[test]
    fn rejects_conbine_container_with_single_zip() {
        let mut data = factory_firmware(b"CONBINE_LTALM057_T1.1.182_112815");
        let last_zip = data
            .windows(ZIP_MAGIC.len())
            .rposition(|window| window == ZIP_MAGIC)
            .unwrap();
        data[last_zip..last_zip + ZIP_MAGIC.len()].fill(0);

        assert!(!is_xiaomi_firmware(&data, Some(data.len())));
    }

    #[test]
    fn rejects_factory_with_bad_version() {
        let data = factory_firmware(b"not_a_miwear_version");

        assert!(!is_xiaomi_firmware(&data, Some(data.len())));
    }

    #[test]
    fn get_file_type_recognizes_crpack_by_manifest_contents() {
        let manifest = br#"{"format":"canopus-resource-pack","formatVersion":1,"themeId":"dark","name":"Dark","mappings":[]}"#;
        let data = zip_with_entries(&[
            ("canora.json", manifest),
            ("assets/toolkit.bin", b"toolkit"),
        ]);

        assert_eq!(get_file_type(&data), FileType::ResourcePack);
    }

    #[test]
    fn get_file_type_does_not_recognize_wrong_crpack_version() {
        let manifest = br#"{"format":"canopus-resource-pack","formatVersion":2}"#;
        let data = zip_with_entries(&[("canora.json", manifest)]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_recognizes_miwear_ota_as_firmware() {
        let data = zip_with_entry("vela_ap.bin", MIN_FIRMWARE_SIZE);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_recognizes_zip_with_vela_ap_entry_as_firmware() {
        let data = zip_with_entry("firmware/vela_ap.bin", MIN_FIRMWARE_SIZE);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_skips_unreadable_entries_before_full_firmware_image() {
        let data = zip_with_unreadable_first_entry();

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_recognizes_miwear_incremental_ota_as_firmware() {
        let mut patch = b"DDELTA50".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", &patch),
            ("vela_ota.bin", b"ota updater"),
        ]);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_recognizes_ddelta60_incremental_ota_as_firmware() {
        let mut patch = b"DDELTA60".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", &patch),
            ("vela_ota.bin", b"ota updater"),
        ]);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_recognizes_bsdiff_incremental_ota_without_updater() {
        let mut patch = b"ENDSLEY/BSDIFF50".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"bspatch /dev/ap /data/ota_tmp/vela_ap.tmp /data/ota_tmp/vela_ap.patch lz4",
            ),
            ("vela_ap.patch", &patch),
        ]);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_rejects_bsdiff_patch_without_ota_script() {
        let mut patch = b"ENDSLEY/BSDIFF50".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[("vela_ap.patch", &patch)]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_rejects_non_versioned_bsdiff_patch() {
        let mut patch = b"ENDSLEY/BSDIFFXX".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"bspatch /dev/ap /data/ota_tmp/vela_ap.tmp /data/ota_tmp/vela_ap.patch lz4",
            ),
            ("vela_ap.patch", &patch),
        ]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_rejects_non_versioned_ddelta_patch() {
        let mut patch = b"DDELTAXX".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", &patch),
            ("vela_ota.bin", b"ota updater"),
        ]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_rejects_single_digit_ddelta_version() {
        let mut patch = b"DDELTA5\0".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", &patch),
            ("vela_ota.bin", b"ota updater"),
        ]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_rejects_incremental_ota_with_invalid_patch_magic() {
        let patch = vec![0u8; MIN_FIRMWARE_SIZE];
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", &patch),
            ("vela_ota.bin", b"ota updater"),
        ]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_rejects_ddelta_zip_without_miwear_ota_files() {
        let mut patch = b"DDELTA50".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[("vela_ap.patch", &patch)]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_rejects_ddelta_ota_without_updater() {
        let mut patch = b"DDELTA50".to_vec();
        patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", &patch),
        ]);

        assert_eq!(get_file_type(&data), FileType::Zip);
    }

    #[test]
    fn get_file_type_recognizes_eight_byte_ddelta_patch() {
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"ddelta_apply /dev/ap /data/ota_tmp/ /ota/vela_ap.patch",
            ),
            ("vela_ap.patch", b"DDELTA50"),
            ("vela_ota.bin", b"ota updater"),
            ("padding.bin", &vec![0u8; MIN_FIRMWARE_SIZE]),
        ]);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn get_file_type_accepts_valid_bsdiff_after_invalid_duplicate_patch() {
        let mut valid_patch = b"ENDSLEY/BSDIFF50".to_vec();
        valid_patch.resize(MIN_FIRMWARE_SIZE, 0);
        let data = zip_with_entries(&[
            (
                "ota.sh",
                b"bspatch /dev/ap /data/ota_tmp/vela_ap.tmp /data/ota_tmp/vela_ap.patch lz4",
            ),
            ("vela_ap.patch", b"not a patch"),
            ("vela_ap.patch", &valid_patch),
        ]);

        assert_eq!(get_file_type(&data), FileType::Firmware);
    }

    #[test]
    fn test_get_and_set_watchface_id_mwz() {
        let config = test_config();
        let bin_bytes = data_with_field(b"123456789");
        let description_xml = br#"<?xml version="1.0" encoding="utf-8"?>
<watch>
    <name>TestFace</name>
    <_id>123456789</_id>
</watch>"#;
        let manifest_xml = br#"<?xml version="1.0" encoding="utf-8"?>
<Watchface name="TestFace" width="466" height="466" id="123456789">
</Watchface>"#;

        let mut mwz_data = zip_with_entries(&[
            ("description.xml", description_xml),
            ("manifest.xml", manifest_xml),
            ("123456789.bin", &bin_bytes),
        ]);

        // 验证读取初始 ID
        assert_eq!(get_watchface_id(&mwz_data, &config), Some("123456789".to_string()));

        // 修改 ID 为 987654321012 (12位)
        set_watchface_id_vec(&mut mwz_data, &config, "987654321012").unwrap();

        // 验证修改后能够正确读取出新 ID
        assert_eq!(get_watchface_id(&mwz_data, &config), Some("987654321012".to_string()));

        // 验证 ZIP 结构完好且内部各条目内容均已被替换
        let reader = Cursor::new(&mwz_data);
        let mut zip = zip::ZipArchive::new(reader).expect("MWZ must remain a valid zip");

        let mut desc_found = false;
        let mut manifest_found = false;
        let mut bin_found = false;

        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).unwrap();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();

            if entry.name() == "description.xml" {
                desc_found = true;
                let text = String::from_utf8(content).unwrap();
                assert!(text.contains("<_id>987654321012</_id>"));
            } else if entry.name() == "manifest.xml" {
                manifest_found = true;
                let text = String::from_utf8(content).unwrap();
                assert!(text.contains("id=\"987654321012\""));
            } else if entry.name().ends_with(".bin") {
                bin_found = true;
                assert_eq!(entry.name(), "987654321012.bin");
                assert_eq!(get_bin_watchface_id(&content, &config), Some("987654321012".to_string()));
            }
        }

        assert!(desc_found, "description.xml should be preserved");
        assert!(manifest_found, "manifest.xml should be preserved");
        assert!(bin_found, "renamed bin should be preserved");
    }
}

#[derive(Clone, Copy, Debug, Serialize_repr, PartialEq)]
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
    ResourcePack = 92,
}
pub fn get_file_type(data: &[u8]) -> FileType {
    if data.is_empty() {
        return FileType::Null;
    }
    // CRPack is identified by its root manifest marker, not by its extension.
    if is_crpack_archive(data) {
        return FileType::ResourcePack;
    }
    // 0. 检查是不是小米可穿戴固件（OTA JAR 也 PK 开头，必须优先判断）
    if is_xiaomi_firmware(data, Some(data.len())) {
        return FileType::Firmware;
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
