// Vivo 快应用 rpk 包内嵌的 manifest.json 解析。
//
// rpk 结构：
//   META-INF/CERT
//   META-INF/build.txt
//   <package>.vru
//   manifest.json   ← 我们关心这个
//
// manifest 形如：
//   {
//     "package": "com.yyh.vbook",
//     "name": "简阅",
//     "versionName": "1.1.2",
//     "versionCode": 16,
//     ...
//   }
//
// V1 BleAppInstallReq (BID 40 / CID 1) 需要的字段：
//   appId        = manifest.package
//   appVerCode   = manifest.versionCode

use std::io::Cursor;

use serde::Deserialize;

use crate::{anyhow_site, bail_site};

#[derive(Debug, Clone)]
pub struct VivoQuickAppManifest {
    pub package: String,
    pub name: String,
    pub version_name: String,
    pub version_code: i32,
}

pub fn parse_vivo_quick_app_manifest(rpk_bytes: &[u8]) -> anyhow::Result<VivoQuickAppManifest> {
    let cursor = Cursor::new(rpk_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|err| anyhow_site!("vivo quick-app rpk is not a zip: {err}"))?;
    let mut manifest_text = String::new();
    {
        use std::io::Read as _;
        let mut entry = archive
            .by_name("manifest.json")
            .map_err(|err| anyhow_site!("vivo quick-app rpk missing manifest.json: {err}"))?;
        entry
            .read_to_string(&mut manifest_text)
            .map_err(|err| anyhow_site!("vivo quick-app rpk manifest.json read failed: {err}"))?;
    }

    #[derive(Deserialize)]
    struct ManifestRaw {
        #[serde(default)]
        package: String,
        #[serde(default)]
        name: String,
        #[serde(rename = "versionName", default)]
        version_name: String,
        #[serde(rename = "versionCode", default)]
        version_code: i32,
    }

    let parsed: ManifestRaw = serde_json::from_str(&manifest_text)
        .map_err(|err| anyhow_site!("vivo quick-app manifest.json parse failed: {err}"))?;

    if parsed.package.trim().is_empty() {
        bail_site!("vivo quick-app manifest.json missing `package` field");
    }

    Ok(VivoQuickAppManifest {
        package: parsed.package,
        name: parsed.name,
        version_name: parsed.version_name,
        version_code: parsed.version_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest_json_envelope() {
        // synth a minimal zip with manifest.json
        let mut buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            let opts = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            use std::io::Write as _;
            zw.start_file("manifest.json", opts).unwrap();
            zw.write_all(
                br#"{"package":"com.example.app","name":"x","versionName":"1.0","versionCode":42}"#,
            )
            .unwrap();
            zw.finish().unwrap();
        }
        let m = parse_vivo_quick_app_manifest(&buf).unwrap();
        assert_eq!(m.package, "com.example.app");
        assert_eq!(m.version_code, 42);
    }
}
