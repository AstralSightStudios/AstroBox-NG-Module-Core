use std::{
    collections::HashSet,
    fs::File,
    io::{Cursor, Read, Seek},
    path::Path,
};

use anyhow::{Context, ensure};
use serde_json::{Map, Value};
use zip::{CompressionMethod, ZipArchive};

pub(crate) const MAX_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub(crate) const MAX_CHUNKS_PER_FILE: usize = 2_048;
const MAX_ARCHIVE_ENTRIES: usize = 16_384;
const THEME_ROOT: &str = "/data/quickapp/files/ng.lst.corona/themes/";
const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";
const RESOURCE_PACK_FORMAT: &str = "canopus-resource-pack";
const RESOURCE_PACK_VERSION: u64 = 1;

pub(crate) struct CrPackFile {
    pub(crate) path: String,
    pub(crate) data: Vec<u8>,
}

pub(crate) struct CrPack {
    pub(crate) theme_id: String,
    pub(crate) files: Vec<CrPackFile>,
    pub(crate) total_bytes: usize,
}

/// Recognize the format from the ZIP contents, never from the filename extension.
pub(crate) fn is_crpack_archive(data: &[u8]) -> bool {
    if data.len() < ZIP_MAGIC.len() || &data[..ZIP_MAGIC.len()] != ZIP_MAGIC {
        return false;
    }

    let Ok(mut archive) = ZipArchive::new(Cursor::new(data)) else {
        return false;
    };
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return false;
    }

    let mut manifest_found = false;
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index(index) else {
            continue;
        };
        if entry.name() != "canora.json" || entry.is_dir() {
            continue;
        }
        if manifest_found
            || !is_regular_file(entry.unix_mode())
            || !supported_compression(entry.compression())
        {
            return false;
        }
        let declared_size = entry.size();
        if declared_size as usize > MAX_MANIFEST_BYTES {
            return false;
        }
        let Ok(manifest_bytes) = read_entry_bounded(entry, declared_size, MAX_MANIFEST_BYTES)
        else {
            return false;
        };
        if !has_supported_marker(&manifest_bytes) {
            return false;
        }
        manifest_found = true;
    }
    manifest_found
}

pub(crate) fn load_crpack(path: &Path) -> anyhow::Result<CrPack> {
    let file = File::open(path).context("failed to open CRPack")?;
    parse_crpack_archive(file)
}

fn parse_crpack_archive<R: Read + Seek>(reader: R) -> anyhow::Result<CrPack> {
    let mut archive = ZipArchive::new(reader).context("CRPack must be a readable ZIP archive")?;
    ensure!(
        archive.len() <= MAX_ARCHIVE_ENTRIES,
        "CRPack has too many ZIP entries"
    );

    let mut files = Vec::new();
    let mut paths = HashSet::new();
    let mut total_bytes = 0usize;
    let mut manifest_bytes = None;

    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .context("invalid or encrypted ZIP entry")?;
        let raw_path = entry.name();
        ensure!(
            !raw_path.contains('\\'),
            "invalid-path: ZIP paths must not contain backslashes"
        );
        let is_directory = entry.is_dir();
        let relative_path = if is_directory {
            raw_path
                .strip_suffix('/')
                .context("invalid directory entry path")?
        } else {
            raw_path
        }
        .to_owned();
        ensure!(
            valid_relative_path(&relative_path),
            "invalid-path: {raw_path}"
        );
        ensure!(
            is_allowed_entry_type(entry.unix_mode(), is_directory),
            "ZIP symlinks and special entries are not allowed: {raw_path}"
        );
        ensure!(
            paths.insert(relative_path.clone()),
            "duplicate ZIP path: {relative_path}"
        );
        if is_directory {
            continue;
        }

        ensure!(
            supported_compression(entry.compression()),
            "unsupported ZIP compression method: {raw_path}"
        );
        ensure!(
            relative_path != "mappings.tsv",
            "CRPack v1 must not contain mappings.tsv"
        );

        let declared_size = entry.size();
        let remaining_bytes = MAX_BYTES - total_bytes;
        let mut entry_limit = remaining_bytes;
        if relative_path == "canora.json" {
            entry_limit = entry_limit.min(MAX_MANIFEST_BYTES);
        }
        let data = read_entry_bounded(entry, declared_size, entry_limit)
            .with_context(|| format!("invalid or oversized ZIP entry: {relative_path}"))?;
        total_bytes += data.len();

        if relative_path == "canora.json" {
            ensure!(
                manifest_bytes.is_none(),
                "CRPack must contain exactly one canora.json"
            );
            manifest_bytes = Some(data.clone());
        }
        files.push(CrPackFile {
            path: relative_path,
            data,
        });
    }

    let manifest_bytes = manifest_bytes.context("CRPack must contain a root canora.json")?;
    let theme_id = parse_manifest_theme_id(&manifest_bytes)?;
    ensure!(!files.is_empty(), "empty CRPack");
    for file in &files {
        let full_path = format!("{THEME_ROOT}{theme_id}/{}", file.path);
        ensure!(
            full_path.len() < 256,
            "resource path exceeds the Manager limit: {}",
            file.path
        );
    }

    Ok(CrPack {
        theme_id,
        files,
        total_bytes,
    })
}

fn read_entry_bounded<R: Read>(
    reader: R,
    declared_size: u64,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let declared_size = usize::try_from(declared_size).context("ZIP entry size is too large")?;
    ensure!(
        declared_size <= max_bytes,
        "ZIP entry exceeds its size limit"
    );

    let capacity = declared_size.min(max_bytes);
    let read_limit = u64::try_from(max_bytes)
        .context("ZIP entry limit is too large")?
        .saturating_add(1);
    let mut data = Vec::with_capacity(capacity);
    reader.take(read_limit).read_to_end(&mut data)?;
    ensure!(
        data.len() <= max_bytes,
        "actual decompressed size exceeds its limit"
    );
    ensure!(
        data.len() == declared_size,
        "ZIP entry size does not match its contents"
    );
    Ok(data)
}

fn valid_relative_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    !path.is_empty()
        && !path.starts_with('/')
        && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        && !path.contains(['\\', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn is_regular_file(mode: Option<u32>) -> bool {
    mode.is_none_or(|mode| {
        let file_type = mode & 0o170000;
        file_type == 0 || file_type == 0o100000
    })
}

fn is_allowed_entry_type(mode: Option<u32>, is_directory: bool) -> bool {
    mode.is_none_or(|mode| {
        let file_type = mode & 0o170000;
        file_type == 0 || file_type == if is_directory { 0o040000 } else { 0o100000 }
    })
}

fn supported_compression(method: CompressionMethod) -> bool {
    matches!(
        method,
        CompressionMethod::Stored | CompressionMethod::Deflated
    )
}

fn has_supported_marker(data: &[u8]) -> bool {
    if data.starts_with(&[0xef, 0xbb, 0xbf]) {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(data) else {
        return false;
    };
    value.get("format").and_then(Value::as_str) == Some(RESOURCE_PACK_FORMAT)
        && value.get("formatVersion").and_then(Value::as_u64) == Some(RESOURCE_PACK_VERSION)
}

// The Manager owns mapping-rule validation; the sender only parses fields needed to route the transfer.
fn parse_manifest_theme_id(data: &[u8]) -> anyhow::Result<String> {
    ensure!(
        !data.is_empty() && data.len() <= MAX_MANIFEST_BYTES,
        "canora.json must be between 1 byte and 64 KiB"
    );
    ensure!(
        !data.starts_with(&[0xef, 0xbb, 0xbf]),
        "canora.json must be UTF-8 without a BOM"
    );
    let value: Value =
        serde_json::from_slice(data).context("canora.json is not valid UTF-8 JSON")?;
    let object = value
        .as_object()
        .context("canora.json root must be an object")?;
    ensure!(
        required_string(object, "format")? == RESOURCE_PACK_FORMAT,
        "unsupported CRPack format"
    );
    ensure!(
        object.get("formatVersion").and_then(Value::as_u64) == Some(RESOURCE_PACK_VERSION),
        "unsupported CRPack formatVersion"
    );

    let theme_id = required_string(object, "themeId")?;
    ensure!(valid_theme_id(theme_id), "invalid CRPack themeId");
    Ok(theme_id.to_owned())
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> anyhow::Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("canora.json {key} must be a string"))
}

fn valid_theme_id(id: &str) -> bool {
    (1..=12).contains(&id.len())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID_MANIFEST: &[u8] = br#"{"format":"canopus-resource-pack","formatVersion":1,"themeId":"dark","name":"Dark","mappings":[{"source":"/resource/app/settings/","destination":"app/settings/"}]}"#;
    const EMPTY_MAPPING_MANIFEST: &[u8] = br#"{"format":"canopus-resource-pack","formatVersion":1,"themeId":"dark","name":"Dark","mappings":[]}"#;

    fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipArchiveWriter::new(Cursor::new(Vec::new()));
        let options =
            zip::write::FileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    use zip::ZipWriter as ZipArchiveWriter;

    #[test]
    fn recognizes_and_loads_crpack_without_extension_assumptions() {
        let bytes = zip_with_entries(&[
            ("canora.json", VALID_MANIFEST),
            ("app/settings/launcher.bin", b"asset"),
        ]);
        assert!(is_crpack_archive(&bytes));

        let pack = parse_crpack_archive(Cursor::new(bytes)).unwrap();
        assert_eq!(pack.theme_id, "dark");
        assert_eq!(pack.files.len(), 2);
        assert_eq!(pack.files[0].path, "canora.json");
        assert_eq!(pack.files[1].path, "app/settings/launcher.bin");
        assert_eq!(pack.files[1].data, b"asset");
        assert_eq!(pack.total_bytes, VALID_MANIFEST.len() + 5);
    }

    #[test]
    fn rejects_non_root_manifest_wrong_marker_and_unsupported_version() {
        let wrapped = zip_with_entries(&[("dark/canora.json", VALID_MANIFEST)]);
        assert!(!is_crpack_archive(&wrapped));
        assert!(parse_crpack_archive(Cursor::new(wrapped)).is_err());

        let wrong_format = br#"{"format":"other","formatVersion":1}"#;
        let wrong_version = br#"{"format":"canopus-resource-pack","formatVersion":2}"#;
        for manifest in [wrong_format.as_slice(), wrong_version.as_slice()] {
            let bytes = zip_with_entries(&[("canora.json", manifest)]);
            assert!(!is_crpack_archive(&bytes));
            assert!(parse_crpack_archive(Cursor::new(bytes)).is_err());
        }
    }

    #[test]
    fn rejects_legacy_mappings_duplicates_and_unsafe_paths() {
        let legacy = zip_with_entries(&[("mappings.tsv", b"old"), ("canora.json", VALID_MANIFEST)]);
        assert!(parse_crpack_archive(Cursor::new(legacy)).is_err());

        let duplicate = zip_with_entries(&[
            ("canora.json", VALID_MANIFEST),
            ("canora.json", VALID_MANIFEST),
        ]);
        assert!(parse_crpack_archive(Cursor::new(duplicate)).is_err());

        let traversal =
            zip_with_entries(&[("canora.json", VALID_MANIFEST), ("../evil.bin", b"bad")]);
        assert!(parse_crpack_archive(Cursor::new(traversal)).is_err());

        let backslash =
            zip_with_entries(&[("canora.json", VALID_MANIFEST), ("icons\\evil.bin", b"bad")]);
        assert!(parse_crpack_archive(Cursor::new(backslash)).is_err());

        let drive_path =
            zip_with_entries(&[("canora.json", VALID_MANIFEST), ("C:/evil.bin", b"bad")]);
        assert!(parse_crpack_archive(Cursor::new(drive_path)).is_err());
    }

    #[test]
    fn accepts_more_than_128_files_and_rejects_actual_size_overruns() {
        let mut writer = ZipArchiveWriter::new(Cursor::new(Vec::new()));
        let options =
            zip::write::FileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("canora.json", options).unwrap();
        writer.write_all(EMPTY_MAPPING_MANIFEST).unwrap();
        for index in 0..256 {
            writer
                .start_file(format!("assets/{index}.bin"), options)
                .unwrap();
            writer.write_all(b"x").unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        // The container imposes no file-count cap; only the Interconnect transfer is bounded.
        assert!(parse_crpack_archive(Cursor::new(bytes)).is_ok());
        assert!(read_entry_bounded(&b"123456"[..], 5, 5).is_err());
    }

    #[test]
    fn leaves_mapping_semantics_to_the_device_manager() {
        let invalid_mapping_manifest = br#"{"format":"canopus-resource-pack","formatVersion":1,"themeId":"dark","name":"Dark","mappings":[{"source":"not-absolute","destination":"../outside/"}]}"#;
        let bytes = zip_with_entries(&[
            ("canora.json", invalid_mapping_manifest),
            ("app/settings/launcher.bin", b"asset"),
        ]);
        assert!(is_crpack_archive(&bytes));
        assert!(parse_crpack_archive(Cursor::new(bytes)).is_ok());
    }
}
