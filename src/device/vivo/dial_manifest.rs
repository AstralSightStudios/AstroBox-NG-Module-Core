// Vivo 表盘 rpk 包内嵌的 manifest.json 解析。
//
// 一个典型的 rpk 是 zip：
//   META-INF/CERT
//   META-INF/build.txt
//   com.vivo.wf.watch107475.vru
//   manifest.json   ← 我们关心这个
//
// manifest.json 大致长这样：
//   {
//     "package":"com.vivo.wf.watch107475",
//     "versionName":"1.0.0.2",
//     "versionCode":10002,
//     ...
//     "router":{
//       "watchfaces":{
//         "watchface":{ "id":"107475", ... }
//       }
//     }
//   }
//
// 装表盘的 BID 1 / CID 1 `DialInstallBleRequest.dialId` 就是这个 id 字段。

use std::io::Cursor;

use serde::Deserialize;

use crate::{anyhow_site, bail_site};

#[derive(Debug, Clone)]
pub struct VivoDialManifest {
    pub package: String,
    pub version_name: String,
    pub version_code: i32,
    pub dial_id: i64,
}

pub fn parse_vivo_dial_manifest(rpk_bytes: &[u8]) -> anyhow::Result<VivoDialManifest> {
    let cursor = Cursor::new(rpk_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|err| anyhow_site!("vivo dial rpk is not a zip: {err}"))?;
    let mut manifest_text = String::new();
    {
        use std::io::Read as _;
        let mut entry = archive
            .by_name("manifest.json")
            .map_err(|err| anyhow_site!("vivo dial rpk missing manifest.json: {err}"))?;
        entry
            .read_to_string(&mut manifest_text)
            .map_err(|err| anyhow_site!("vivo dial rpk manifest.json read failed: {err}"))?;
    }

    #[derive(Deserialize)]
    struct ManifestRaw {
        #[serde(default)]
        package: String,
        #[serde(rename = "versionName", default)]
        version_name: String,
        #[serde(rename = "versionCode", default)]
        version_code: i32,
        #[serde(default)]
        router: Option<RouterRaw>,
    }
    #[derive(Deserialize)]
    struct RouterRaw {
        #[serde(default)]
        watchfaces: Option<serde_json::Map<String, serde_json::Value>>,
    }

    let parsed: ManifestRaw = serde_json::from_str(&manifest_text)
        .map_err(|err| anyhow_site!("vivo dial manifest.json parse failed: {err}"))?;

    // dial_id 优先从 router.watchfaces.<key>.id 取（Vivo 官方表盘里通常就是字符串数字）。
    // 兜底：从 package "com.vivo.wf.watch{N}" 里抽数字。
    let dial_id_from_router = parsed
        .router
        .as_ref()
        .and_then(|r| r.watchfaces.as_ref())
        .and_then(|m| m.values().next())
        .and_then(|v| v.get("id"))
        .and_then(extract_id_value);

    let dial_id_from_package = parsed
        .package
        .strip_prefix("com.vivo.wf.watch")
        .and_then(|tail| tail.parse::<i64>().ok())
        .or_else(|| {
            parsed.package.rsplit('.').next().and_then(|seg| {
                seg.trim_start_matches(|c: char| !c.is_ascii_digit())
                    .parse::<i64>()
                    .ok()
            })
        });

    let dial_id = dial_id_from_router
        .or(dial_id_from_package)
        .ok_or_else(|| {
            anyhow_site!(
                "vivo dial manifest.json does not contain a recognizable dial id; package={}",
                parsed.package
            )
        })?;

    if dial_id == 0 {
        bail_site!("vivo dial manifest reported dial_id=0 which the watch will reject");
    }

    Ok(VivoDialManifest {
        package: parsed.package,
        version_name: parsed.version_name,
        version_code: parsed.version_code,
        dial_id,
    })
}

fn extract_id_value(value: &serde_json::Value) -> Option<i64> {
    if let Some(s) = value.as_str() {
        return s.trim().parse::<i64>().ok();
    }
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(f) = value.as_f64() {
        return Some(f as i64);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_id_from_string_or_number() {
        assert_eq!(extract_id_value(&serde_json::json!("107475")), Some(107475));
        assert_eq!(extract_id_value(&serde_json::json!(107475)), Some(107475));
        assert_eq!(extract_id_value(&serde_json::json!(null)), None);
    }
}
