//! Native .canopus tar packages. Never unpack or convert them into quick apps.
use super::is_valid_module_id;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
};

pub const RECEIPT_SIZE: usize = 176;
pub const ARTIFACT_MAX: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u32,
    module_id: String,
    canopus_abi: String,
    lifecycle: String,
    version_code: Option<u32>,
    artifacts: Vec<Artifact>,
    native_app: Option<NativeApp>,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    target_id: String,
    firmware_sha256: String,
    path: String,
    sha256: String,
    receipt_path: Option<String>,
    receipt_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NativeApp {
    resources: Option<Vec<serde_json::Value>>,
    resource_hashes: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub struct InstallPackage {
    pub module_id: String,
    pub receipt: Vec<u8>,
    pub artifact: Vec<u8>,
}

fn canonical_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(format!("Invalid Canopus archive path: {path:?}"));
    }
    Ok(())
}

fn read_archive(data: &[u8]) -> Result<(Manifest, HashMap<String, Vec<u8>>), String> {
    let mut archive = tar::Archive::new(data);
    let mut files = HashMap::new();
    for entry in archive
        .entries()
        .map_err(|e| format!("Invalid Canopus tar: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("Invalid Canopus tar entry: {e}"))?;
        let path = String::from_utf8(entry.path_bytes().into_owned())
            .map_err(|_| "Canopus archive path is not UTF-8".to_string())?;
        canonical_path(&path)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(format!(
                "Canopus archive entry is not a regular file: {path}"
            ));
        }
        if files.contains_key(&path) {
            return Err(format!("Duplicate Canopus archive entry: {path}"));
        }
        // No extraction, links, or allocation based on the untrusted tar size.
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Cannot read {path}: {e}"))?;
        files.insert(path, bytes);
    }
    let manifest: Manifest = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or("Canopus package has no manifest.json")?,
    )
    .map_err(|e| format!("Invalid Canopus manifest: {e}"))?;
    if manifest.schema != 1
        || !is_valid_module_id(&manifest.module_id)
        || manifest.artifacts.is_empty()
    {
        return Err("Invalid Canopus package schema or module identity".into());
    }
    Ok((manifest, files))
}

/// Content-based identification; full integrity/target validation runs before staging.
pub fn is_canopus_archive(data: &[u8]) -> bool {
    // Avoid treating ZIPs/text as tar (or scanning every unrelated resource).
    data.len() >= 512 && data.get(257..262) == Some(b"ustar") && read_archive(data).is_ok()
}

fn hash_bytes(hex: &str) -> Result<Vec<u8>, String> {
    let bytes = hex::decode(hex).map_err(|_| "Invalid Canopus SHA-256".to_string())?;
    if bytes.len() != 32 {
        return Err("Invalid Canopus SHA-256 length".into());
    }
    Ok(bytes)
}

fn verified_entry<'a>(
    files: &'a HashMap<String, Vec<u8>>,
    path: &str,
    hash: &str,
) -> Result<&'a [u8], String> {
    canonical_path(path)?;
    let bytes = files
        .get(path)
        .ok_or_else(|| format!("Canopus package is missing {path}"))?;
    if Sha256::digest(bytes).as_slice() != hash_bytes(hash)? {
        return Err(format!("Canopus SHA-256 mismatch: {path}"));
    }
    Ok(bytes)
}

fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

pub(super) fn fixed_string(bytes: &[u8]) -> Result<&str, String> {
    let end = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or("Unterminated Canopus identity")?;
    if end == 0 || bytes[end..].iter().any(|&b| b != 0) {
        return Err("Invalid Canopus identity padding".into());
    }
    std::str::from_utf8(&bytes[..end]).map_err(|_| "Canopus identity is not UTF-8".into())
}

/// Verify all declared artifact/receipt hashes, then select exactly one target.
/// StageBegin rechecks the receipt's firmware hash against the actual firmware.
/// Resources are not supported by the stage service and must never be dropped.
pub fn prepare_install(data: &[u8], target_id: &str) -> Result<InstallPackage, String> {
    if target_id.is_empty() {
        return Err(
            "Canopus device did not report an exact target_id; native install unsupported".into(),
        );
    }
    let (manifest, files) = read_archive(data)?;
    // Packages carry the Canopus public ABI major they were built for; the Supervisor refuses
    // modules of another major, so a package of an older ABI is turned away here.
    if manifest.canopus_abi != "2" {
        return Err(format!(
            "Unsupported Canopus package ABI: {}",
            manifest.canopus_abi
        ));
    }
    if manifest.native_app.as_ref().is_some_and(|app| {
        app.resources.as_ref().is_some_and(|r| !r.is_empty())
            || app.resource_hashes.as_ref().is_some_and(|r| !r.is_empty())
    }) {
        return Err("Canopus native install does not support package resources yet".into());
    }
    let lifecycle = match manifest.lifecycle.as_str() {
        "removable" => 0,
        "resident-after-activation" => 1,
        "always-resident" => 2,
        "patch-reboot-required" => 3,
        _ => return Err("Unknown Canopus package lifecycle".into()),
    };
    let mut targets = HashSet::new();
    let mut selected = None;
    for artifact in &manifest.artifacts {
        if !targets.insert(&artifact.target_id) {
            return Err(format!("Duplicate Canopus target: {}", artifact.target_id));
        }
        let elf = verified_entry(&files, &artifact.path, &artifact.sha256)?;
        if !elf.starts_with(b"\x7fELF") || elf.len() > ARTIFACT_MAX {
            return Err("Canopus artifact must be an ELF of at most 4 MiB".into());
        }
        let firmware_hash = hash_bytes(&artifact.firmware_sha256)?;
        let receipt = match (&artifact.receipt_path, &artifact.receipt_sha256) {
            (Some(path), Some(hash)) => verified_entry(&files, path, hash)?,
            _ => {
                return Err(format!(
                    "Canopus target {} has no install receipt",
                    artifact.target_id
                ));
            }
        };
        if receipt.len() != RECEIPT_SIZE
            || &receipt[..4] != b"CMI2"
            || word(receipt, 4) != 2
            || word(receipt, 8) != RECEIPT_SIZE as u32
            || word(receipt, 12) != 0
            || word(receipt, 28) != 0
            || word(receipt, 16) != lifecycle
            || word(receipt, 24) as usize != elf.len()
            || manifest
                .version_code
                .is_some_and(|v| v != word(receipt, 20))
            || fixed_string(&receipt[32..64])? != manifest.module_id
            || fixed_string(&receipt[64..112])? != artifact.target_id
            || receipt[112..144] != firmware_hash
            || receipt[144..176] != Sha256::digest(elf)[..]
        {
            return Err(format!(
                "Canopus receipt does not match artifact/manifest: {}",
                artifact.target_id
            ));
        }
        if artifact.target_id == target_id {
            selected = Some(InstallPackage {
                module_id: manifest.module_id.clone(),
                receipt: receipt.to_vec(),
                artifact: elf.to_vec(),
            });
        }
    }
    selected.ok_or_else(|| format!("Canopus package has no artifact for exact target {target_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(target: &str, resources: bool, corrupt_receipt: bool) -> Vec<u8> {
        let elf = b"\x7fELFtest";
        let mut receipt = vec![0; RECEIPT_SIZE];
        for (offset, value) in [
            (0, 0x32494d43u32),
            (4, 2),
            (8, 176),
            (20, 7),
            (24, elf.len() as u32),
        ] {
            receipt[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        receipt[32..37].copy_from_slice(b"hello");
        receipt[64..64 + target.len()].copy_from_slice(target.as_bytes());
        receipt[112..144].fill(1);
        receipt[144..176].copy_from_slice(&Sha256::digest(elf));
        if corrupt_receipt {
            receipt[24] += 1;
        }
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "module_id": "hello", "canopus_abi": "2", "lifecycle": "removable", "version_code": 7,
            "native_app": { "resources": if resources { vec![serde_json::json!({"path":"icon.bin"})] } else { vec![] } },
            "artifacts": [{"target_id": target, "firmware_sha256": hex::encode([1;32]),
                "path": "module.elf", "sha256": hex::encode(Sha256::digest(elf)),
                "receipt_path": "module.cmi", "receipt_sha256": hex::encode(Sha256::digest(&receipt))}]
        })).unwrap();
        let mut tar = tar::Builder::new(Vec::new());
        for (path, bytes) in [
            ("manifest.json", manifest.as_slice()),
            ("module.elf", elf.as_slice()),
            ("module.cmi", receipt.as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        tar.into_inner().unwrap()
    }

    #[test]
    fn native_tar_requires_exact_target_and_verified_receipt() {
        let bytes = package("xiaomi-band-11-1.1.139", false, false);
        assert!(is_canopus_archive(&bytes));
        use crate::device::xiaomi::{components::install::InstallSystem, packet::mass::MassDataType, resutils::{get_file_type, FileType}};
        assert_eq!(get_file_type(&bytes), FileType::Canopus);
        // An explicit quick-app picker/type must not bypass native dispatch.
        let error = InstallSystem::default().send_install_request(MassDataType::ThirdPartyApp, bytes.clone(), None).err().unwrap();
        assert!(error.to_string().contains("Canopus CPC2 staging"));
        let result = prepare_install(&bytes, "xiaomi-band-11-1.1.139").unwrap();
        assert_eq!(result.module_id, "hello");
        assert_eq!(result.receipt.len(), 176);
        assert!(prepare_install(&bytes, "xiaomi-band-11-1.1.138").is_err());
        assert!(prepare_install(&package("t", false, true), "t").is_err());
        assert!(
            prepare_install(&package("t", true, false), "t")
                .unwrap_err()
                .contains("resources")
        );
        let mut tampered = bytes;
        let pos = tampered
            .windows(8)
            .position(|b| b == b"\x7fELFtest")
            .unwrap();
        tampered[pos + 7] ^= 1;
        assert!(prepare_install(&tampered, "xiaomi-band-11-1.1.139").is_err());
        assert!(!is_canopus_archive(b"PK\x03\x04quickapp toolkit"));
    }
}
