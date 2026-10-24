//! Native .canopus tar packages. Never unpack or convert them into quick apps.
//!
//! Package resources (`resources/**` entries declared in `manifest.resources`,
//! `manifest.native_app.resources` or `manifest.native_app.resource_hashes`)
//! are hash-verified here and staged to the device as one flat file per
//! resource: the device extracts each one into the module data directory as
//! `/canopus/m.<module_id>/<basename>` (see `stage_package_with`).
use super::is_valid_module_id;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
};

pub const RECEIPT_SIZE: usize = 176;
pub const ARTIFACT_MAX: usize = 4 * 1024 * 1024;
pub const RESOURCE_MAX: usize = 4 * 1024 * 1024;
pub const RESOURCE_MAX_COUNT: usize = 256;
/// The flat extraction name travels as a single length-prefixed byte string.
const RESOURCE_NAME_MAX: usize = u8::MAX as usize;

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u32,
    module_id: String,
    canopus_abi: String,
    lifecycle: String,
    version_code: Option<u32>,
    artifacts: Vec<Artifact>,
    native_app: Option<NativeApp>,
    resources: Option<Vec<ResourceEntry>>,
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

/// One declared package resource (`manifest.resources` /
/// `manifest.native_app.resources`). `resource_hashes` carries the same
/// declarations as a plain `path -> sha256` map.
#[derive(Debug, Deserialize)]
struct ResourceEntry {
    path: String,
    sha256: String,
    media_type: String,
    target_backend: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NativeApp {
    resources: Option<Vec<ResourceEntry>>,
    resource_hashes: Option<HashMap<String, String>>,
}

#[derive(Debug)]
pub struct InstallPackage {
    pub module_id: String,
    pub receipt: Vec<u8>,
    pub artifact: Vec<u8>,
    /// Resources this install delivers to the module data directory, one flat
    /// file each (`/canopus/m.<module_id>/<name>`).
    pub resources: Vec<InstallResource>,
}

#[derive(Debug)]
pub struct InstallResource {
    /// Flat extraction name = basename of the declared `path`.
    pub name: String,
    /// Verified SHA-256 of `data` (32 raw bytes).
    pub sha256: Vec<u8>,
    pub data: Vec<u8>,
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

struct Declaration<'a> {
    path: &'a str,
    sha256: &'a str,
    target_backend: Option<&'a str>,
}

/// `resources/common/**` ships to every target and is the fallback layer;
/// `resources/<target_id>/**` is the per-target override layer. The extraction
/// name is the entry's basename either way (flat module data directory).
/// Returns `(target scope, flat name)`; `None` scope means common.
fn resource_scope(path: &str) -> Result<(Option<&str>, &str), String> {
    let rest = path
        .strip_prefix("resources/")
        .ok_or_else(|| format!("Canopus resource is not under resources/: {path}"))?;
    let name = rest.rsplit('/').next().unwrap_or("");
    let dir = &rest[..rest.len() - name.len()];
    let scope = match dir.split('/').next() {
        Some(first) if !first.is_empty() && first != "common" => Some(first),
        _ => None,
    };
    if name.is_empty() || name.len() > RESOURCE_NAME_MAX {
        return Err(format!("Invalid Canopus resource name in path: {path:?}"));
    }
    Ok((scope, name))
}

/// Parse and hash-verify every declared resource (`manifest.resources`,
/// `manifest.native_app.resources`, `manifest.native_app.resource_hashes`)
/// against the archive, rejecting missing/mismatched/undeclared entries, then
/// select what this exact target receives: the common layer plus the selected
/// target's layer. Each selected resource becomes one flat file; the per-target
/// file overrides the common file with the same basename, and same-layer
/// basename collisions are ambiguous and rejected. Resources of non-selected
/// targets are still verified but never delivered to this device.
fn verify_resources(
    manifest: &Manifest,
    files: &HashMap<String, Vec<u8>>,
    target_id: &str,
) -> Result<Vec<InstallResource>, String> {
    let native_app = manifest.native_app.as_ref();
    let mut declarations: Vec<Declaration> = Vec::new();
    for entry in manifest.resources.iter().flatten() {
        declarations.push(Declaration {
            path: &entry.path,
            sha256: &entry.sha256,
            target_backend: entry.target_backend.as_deref(),
        });
        if entry.media_type.is_empty() {
            return Err(format!(
                "Canopus resource has an empty media_type: {}",
                entry.path
            ));
        }
    }
    for entry in native_app.into_iter().flat_map(|app| app.resources.iter().flatten()) {
        declarations.push(Declaration {
            path: &entry.path,
            sha256: &entry.sha256,
            target_backend: entry.target_backend.as_deref(),
        });
        if entry.media_type.is_empty() {
            return Err(format!(
                "Canopus resource has an empty media_type: {}",
                entry.path
            ));
        }
    }
    // HashMap iteration order is not deterministic; sort for a stable error order.
    let mut hash_map_decls: Vec<(&String, &String)> = native_app
        .and_then(|app| app.resource_hashes.as_ref())
        .map(|map| map.iter().collect())
        .unwrap_or_default();
    hash_map_decls.sort();
    for (path, sha256) in hash_map_decls {
        declarations.push(Declaration {
            path,
            sha256,
            target_backend: None,
        });
    }
    if declarations.len() > RESOURCE_MAX_COUNT {
        return Err("Canopus package declares too many resources".into());
    }

    // Deduplicate identical declarations; conflicting ones are a mismatch.
    let mut unique: Vec<(Declaration, Vec<u8>)> = Vec::new();
    let mut seen: HashMap<&str, Vec<u8>> = HashMap::new();
    for decl in declarations {
        canonical_path(decl.path)?;
        let (scope, _) = resource_scope(decl.path)?;
        match (decl.target_backend, scope) {
            (Some(declared), Some(actual)) if declared == actual => {}
            (Some(_), _) => {
                return Err(format!(
                    "Canopus resource target_backend does not match its path: {}",
                    decl.path
                ));
            }
            (None, _) => {}
        }
        let digest = hash_bytes(decl.sha256)?;
        if let Some(previous) = seen.insert(decl.path, digest.clone()) {
            if previous != digest {
                return Err(format!(
                    "Canopus package declares conflicting hashes for {}",
                    decl.path
                ));
            }
            continue;
        }
        unique.push((decl, digest));
    }

    // Hash-verify each declared entry against the archive.
    let mut verified: Vec<(Declaration, Vec<u8>, Vec<u8>)> = Vec::new();
    for (decl, digest) in unique {
        let bytes = verified_entry(files, decl.path, decl.sha256)?;
        if bytes.len() > RESOURCE_MAX {
            return Err(format!("Canopus resource is too large: {}", decl.path));
        }
        verified.push((decl, digest, bytes.to_vec()));
    }

    // Every archive entry in the resource tree must be declared: never drop
    // anything silently, and never deliver anything unverified.
    let declared: HashSet<&str> = verified.iter().map(|(decl, _, _)| decl.path).collect();
    for path in files.keys() {
        if (path == "resources" || path.starts_with("resources/"))
            && !declared.contains(path.as_str())
        {
            return Err(format!(
                "Canopus package resource is not declared in the manifest: {path}"
            ));
        }
    }

    // Common layer first, then the selected target's override layer.
    let mut out: Vec<InstallResource> = Vec::new();
    let mut slots: HashMap<&str, usize> = HashMap::new();
    let mut slot_overridable: Vec<bool> = Vec::new();
    for target_layer in [false, true] {
        for (decl, digest, data) in &verified {
            let (scope, name) = resource_scope(decl.path)?;
            let in_layer = if target_layer {
                scope == Some(target_id)
            } else {
                scope.is_none()
            };
            if !in_layer {
                continue;
            }
            match slots.get(name) {
                // A per-target file replaces the common file of the same name.
                Some(&slot) if target_layer && slot_overridable[slot] => {
                    out[slot] = InstallResource {
                        name: name.to_string(),
                        sha256: digest.clone(),
                        data: data.clone(),
                    };
                    slot_overridable[slot] = false;
                }
                Some(_) => {
                    return Err(format!(
                        "Canopus resources collide on the flat extraction name: {name}"
                    ));
                }
                None => {
                    slots.insert(name, out.len());
                    slot_overridable.push(!target_layer);
                    out.push(InstallResource {
                        name: name.to_string(),
                        sha256: digest.clone(),
                        data: data.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Verify all declared artifact/receipt/resource hashes, then select exactly
/// one target. StageBegin rechecks the receipt's firmware hash against the
/// actual firmware; the selected resources ride the same staging transaction
/// and are extracted on the device into `/canopus/m.<module_id>/<name>`.
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
    let lifecycle = match manifest.lifecycle.as_str() {
        "removable" => 0,
        "resident-after-activation" => 1,
        "always-resident" => 2,
        "patch-reboot-required" => 3,
        _ => return Err("Unknown Canopus package lifecycle".into()),
    };
    let resources = verify_resources(&manifest, &files, target_id)?;
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
                resources: Vec::new(),
            });
        }
    }
    let mut package =
        selected.ok_or_else(|| format!("Canopus package has no artifact for exact target {target_id}"))?;
    package.resources = resources;
    Ok(package)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_for(target: &str, elf: &[u8], corrupt: bool) -> Vec<u8> {
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
        if corrupt {
            receipt[24] += 1;
        }
        receipt
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, *bytes).unwrap();
        }
        tar.into_inner().unwrap()
    }

    fn base_manifest(target: &str) -> serde_json::Value {
        serde_json::json!({
            "schema": 1, "module_id": "hello", "canopus_abi": "2", "lifecycle": "removable", "version_code": 7,
            "artifacts": [{"target_id": target}]
        })
    }

    /// Manifest + ELF + receipt, plus whatever resource declarations and
    /// resource entries the test crafts. The artifact declarations always hash
    /// the exact bytes packed into the tar.
    fn build_package(
        target: &str,
        corrupt_receipt: bool,
        mut manifest: serde_json::Value,
        resources: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let elf: &[u8] = b"\x7fELFtest";
        let receipt = receipt_for(target, elf, corrupt_receipt);
        manifest["artifacts"] = serde_json::json!([{
            "target_id": target, "firmware_sha256": hex::encode([1;32]),
            "path": "module.elf", "sha256": hex::encode(Sha256::digest(elf)),
            "receipt_path": "module.cmi", "receipt_sha256": hex::encode(Sha256::digest(&receipt))
        }]);
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut entries: Vec<(&str, &[u8])> =
            vec![("manifest.json", manifest.as_slice()), ("module.elf", elf)];
        entries.push(("module.cmi", receipt.as_slice()));
        entries.extend_from_slice(resources);
        tar_of(&entries)
    }

    fn package(target: &str, corrupt_receipt: bool) -> Vec<u8> {
        build_package(target, corrupt_receipt, base_manifest(target), &[])
    }

    fn declaration(path: &str, sha256: &str, target_backend: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "path": path, "sha256": sha256,
            "media_type": "application/octet-stream", "target_backend": target_backend
        })
    }

    #[test]
    fn native_tar_requires_exact_target_and_verified_receipt() {
        let bytes = package("xiaomi-band-11-1.1.139", false);
        assert!(is_canopus_archive(&bytes));
        use crate::device::xiaomi::{components::install::InstallSystem, packet::mass::MassDataType, resutils::{get_file_type, FileType}};
        assert_eq!(get_file_type(&bytes), FileType::Canopus);
        // An explicit quick-app picker/type must not bypass native dispatch.
        let error = InstallSystem::default().send_install_request(MassDataType::ThirdPartyApp, bytes.clone(), None).err().unwrap();
        assert!(error.to_string().contains("Canopus CPC2 staging"));
        let result = prepare_install(&bytes, "xiaomi-band-11-1.1.139").unwrap();
        assert_eq!(result.module_id, "hello");
        assert_eq!(result.receipt.len(), 176);
        assert!(result.resources.is_empty());
        assert!(prepare_install(&bytes, "xiaomi-band-11-1.1.138").is_err());
        assert!(prepare_install(&package("t", true), "t").is_err());
        let mut tampered = bytes;
        let pos = tampered
            .windows(8)
            .position(|b| b == b"\x7fELFtest")
            .unwrap();
        tampered[pos + 7] ^= 1;
        assert!(prepare_install(&tampered, "xiaomi-band-11-1.1.139").is_err());
        assert!(!is_canopus_archive(b"PK\x03\x04quickapp toolkit"));
    }

    #[test]
    fn package_abi_is_aligned_to_2_only() {
        // The ABI-2 device rejects old major descriptors at load time, so the
        // sender refuses them early instead of shipping a doomed package.
        for abi in ["1", "3", "1.2", ""] {
            let mut manifest = base_manifest("t");
            manifest["canopus_abi"] = serde_json::json!(abi);
            let bytes = build_package("t", false, manifest, &[]);
            let error = prepare_install(&bytes, "t").unwrap_err();
            assert!(error.contains("Unsupported Canopus package ABI"), "{abi:?}: {error}");
        }
        let mut manifest = base_manifest("t");
        manifest["canopus_abi"] = serde_json::json!("2");
        assert!(prepare_install(&build_package("t", false, manifest, &[]), "t").is_ok());
    }

    #[test]
    fn resources_are_verified_and_extract_flat_per_target() {
        let common_logo = b"common logo";
        let target_logo = b"target logo";
        let other_icon = b"other target icon";
        let native_icon = b"native app icon";
        let hashed_icon = b"hashed map icon";
        let mut manifest = base_manifest("t");
        let resources_json = serde_json::json!([
            declaration("resources/common/logo.bin", &hex::encode(Sha256::digest(common_logo)), serde_json::Value::Null),
            declaration("resources/t/logo.bin", &hex::encode(Sha256::digest(target_logo)), serde_json::json!("t")),
            declaration("resources/other/icon.bin", &hex::encode(Sha256::digest(other_icon)), serde_json::json!("other")),
        ]);
        manifest["resources"] = resources_json.clone();
        manifest["native_app"] = serde_json::json!({
            "resources": [declaration("resources/common/native.bin", &hex::encode(Sha256::digest(native_icon)), serde_json::Value::Null)],
            "resource_hashes": {"resources/common/hashed.bin": hex::encode(Sha256::digest(hashed_icon))}
        });
        let bytes = build_package(
            "t",
            false,
            manifest,
            &[
                ("resources/common/logo.bin", common_logo),
                ("resources/t/logo.bin", target_logo),
                ("resources/other/icon.bin", other_icon),
                ("resources/common/native.bin", native_icon),
                ("resources/common/hashed.bin", hashed_icon),
            ],
        );
        let install = prepare_install(&bytes, "t").unwrap();
        // One flat file per delivered resource: common layer (declaration
        // order, native_app lists appended, hash map sorted) with the selected
        // target overriding the same basename; "other" targets never ship.
        let names: Vec<&str> = install.resources.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["logo.bin", "native.bin", "hashed.bin"]);
        assert_eq!(install.resources[0].data, target_logo);
        assert_eq!(install.resources[1].data, native_icon);
        assert_eq!(install.resources[2].data, hashed_icon);
        for resource in &install.resources {
            assert_eq!(resource.sha256, Sha256::digest(&resource.data)[..]);
            assert_eq!(resource.sha256.len(), 32);
        }
        // Installing another target keeps the common file and drops the rest.
        let mut manifest_other = base_manifest("other");
        manifest_other["resources"] = resources_json;
        let bytes = build_package(
            "other",
            false,
            manifest_other,
            &[
                ("resources/common/logo.bin", common_logo),
                ("resources/t/logo.bin", target_logo),
                ("resources/other/icon.bin", other_icon),
            ],
        );
        let install = prepare_install(&bytes, "other").unwrap();
        let names: Vec<&str> = install.resources.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["logo.bin", "icon.bin"]);
        assert_eq!(install.resources[0].data, common_logo);
        assert_eq!(install.resources[1].data, other_icon);
    }

    #[test]
    fn resource_declarations_must_match_the_archive() {
        let data = b"resource bytes";
        let good = hex::encode(Sha256::digest(data));

        // Declared hash mismatch.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([declaration(
            "resources/common/a.bin",
            &hex::encode(Sha256::digest(b"other bytes")),
            serde_json::Value::Null
        )]);
        let bytes = build_package("t", false, manifest, &[("resources/common/a.bin", data)]);
        assert!(prepare_install(&bytes, "t")
            .unwrap_err()
            .contains("SHA-256 mismatch"));

        // Declared but missing from the archive.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([declaration(
            "resources/common/ghost.bin",
            &good,
            serde_json::Value::Null
        )]);
        let bytes = build_package("t", false, manifest, &[]);
        assert!(prepare_install(&bytes, "t").unwrap_err().contains("missing"));

        // Present in the archive but declared nowhere: never dropped silently.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([]);
        let bytes = build_package(
            "t",
            false,
            manifest,
            &[("resources/common/stray.bin", data)],
        );
        assert!(prepare_install(&bytes, "t")
            .unwrap_err()
            .contains("not declared"));

        // The same path declared with two different hashes is a mismatch.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([declaration(
            "resources/common/a.bin",
            &good,
            serde_json::Value::Null
        )]);
        manifest["native_app"] = serde_json::json!({
            "resource_hashes": {"resources/common/a.bin": hex::encode(Sha256::digest(b"other bytes"))}
        });
        let bytes = build_package("t", false, manifest, &[("resources/common/a.bin", data)]);
        assert!(prepare_install(&bytes, "t")
            .unwrap_err()
            .contains("conflicting"));

        // target_backend must agree with the declared path.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([declaration(
            "resources/common/a.bin",
            &good,
            serde_json::json!("t")
        )]);
        let bytes = build_package("t", false, manifest, &[("resources/common/a.bin", data)]);
        assert!(prepare_install(&bytes, "t")
            .unwrap_err()
            .contains("target_backend"));

        // Two files with the same flat name in one layer are ambiguous.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([
            declaration("resources/common/a.bin", &good, serde_json::Value::Null),
            declaration("resources/common/sub/a.bin", &good, serde_json::Value::Null),
        ]);
        let bytes = build_package(
            "t",
            false,
            manifest,
            &[("resources/common/a.bin", data), ("resources/common/sub/a.bin", data)],
        );
        assert!(prepare_install(&bytes, "t")
            .unwrap_err()
            .contains("collide"));

        // A good package still installs untouched.
        let mut manifest = base_manifest("t");
        manifest["resources"] = serde_json::json!([declaration(
            "resources/common/a.bin",
            &good,
            serde_json::Value::Null
        )]);
        let bytes = build_package("t", false, manifest, &[("resources/common/a.bin", data)]);
        let install = prepare_install(&bytes, "t").unwrap();
        assert_eq!(install.resources.len(), 1);
        assert_eq!(install.resources[0].name, "a.bin");
        assert_eq!(install.resources[0].data, data);
    }
}
