use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, broadcast},
    time::{Instant, timeout_at},
};

use crate::{
    device::crpack::{MAX_CHUNKS_PER_FILE, load_crpack},
    ecs::Component,
    events::{CoreEvent, InterconnectMessage},
};

pub const MANAGER_PACKAGE: &str = "ng.lst.corona";
const MAX_TEXT_CHARS: usize = 18_000;
const ACK_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_ATTEMPTS: usize = 4;

#[derive(Component, Default)]
pub struct ResourcePackComponent {
    session: Arc<Mutex<()>>,
}

struct PackFile {
    path: String,
    data: Vec<u8>,
}

struct Pack {
    theme: String,
    files: Vec<PackFile>,
    total: usize,
    digest: String,
}

fn load_pack(path: &Path) -> anyhow::Result<Pack> {
    let crpack = load_crpack(path)?;
    let theme = crpack.theme_id;
    let files: Vec<PackFile> = crpack
        .files
        .into_iter()
        .map(|file| PackFile {
            path: file.path,
            data: file.data,
        })
        .collect();
    let total = crpack.total_bytes;

    let mut hash = Sha256::new();
    hash.update(theme.as_bytes());
    for file in &files {
        hash.update((file.path.len() as u64).to_le_bytes());
        hash.update(file.path.as_bytes());
        hash.update((file.data.len() as u64).to_le_bytes());
        hash.update(&file.data);
    }
    let digest = hex::encode(hash.finalize());

    Ok(Pack {
        theme,
        files,
        total,
        digest,
    })
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeToken {
    digest: String,
    chunk_size: usize,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcePackProgress {
    pub progress: f32,
    pub status: String,
    pub resume: Option<ResumeToken>,
}

pub async fn install(
    addr: String,
    path: PathBuf,
    resume: Option<ResumeToken>,
    progress: impl Fn(ResourcePackProgress) + Send,
) -> anyhow::Result<()> {
    let device_addr = addr.clone();
    let lock = crate::ecs::with_rt_mut(move |rt| {
        rt.component_ref::<ResourcePackComponent>(&device_addr)
            .map(|comp| comp.session.clone())
            .context("res_pack is not supported by this device")
    })
    .await?;
    let _guard = lock
        .clone()
        .try_lock_owned()
        .context("a resource pack transfer is already running")?;
    let pack = tokio::task::spawn_blocking(move || load_pack(&path)).await??;
    if let Some(token) = &resume {
        ensure!(
            token.digest == pack.digest,
            "res_pack changed; remove the old task and start a replacement"
        );
    }
    let apps = super::resource::request_quick_app_list_json(addr.clone()).await?;
    ensure!(
        apps.as_array()
            .is_some_and(|apps| apps.iter().any(|app| app["packageName"] == MANAGER_PACKAGE)),
        "required resource is not installed: {MANAGER_PACKAGE}"
    );
    let mut transport = Transport {
        addr: addr.clone(),
        theme: pack.theme.clone(),
        max_chars: MAX_TEXT_CHARS,
        events: crate::events::subscribe(),
        session: lock,
    };
    super::thirdparty_app::launch(addr, MANAGER_PACKAGE.into(), String::new()).await?;
    let hello = transport
        .exchange(
            'H',
            json!({"version":1,"maxTextChars":MAX_TEXT_CHARS}),
            |kind, _| kind == b'H',
        )
        .await?;
    ensure!(
        hello["version"] == 1,
        "unsupported res_pack protocol version"
    );
    transport.max_chars = (hello["maxTextChars"]
        .as_u64()
        .context("invalid maxTextChars")? as usize)
        .min(MAX_TEXT_CHARS);
    ensure!(transport.max_chars >= 256, "maxTextChars is too small");
    let max_chunk = 12_000.min((transport.max_chars - 11) * 13 / 16);
    let chunk_size = resume.as_ref().map_or(max_chunk, |token| token.chunk_size);
    ensure!(
        chunk_size > 0 && chunk_size <= max_chunk,
        "resume chunk size exceeds negotiated message limit"
    );
    validate_chunk_limits(&pack.files, chunk_size)?;
    let max_window = hello["maxWindow"].as_u64().unwrap_or(4).clamp(1, 4) as usize;
    let mode = if resume.is_some() {
        "resume"
    } else {
        "replace"
    };
    transport.exchange('T', json!({"operation":"begin","themeId":pack.theme,"mode":mode,"fileCount":pack.files.len(),"totalBytes":pack.total}), |kind, value| kind == b'T' && value["operation"] == "ack" && value["itemType"] == "begin").await?;
    let token = ResumeToken {
        digest: pack.digest.clone(),
        chunk_size,
    };
    // Keep the exact source identity and chunk geometry even if the manifest is interrupted.
    progress(ResourcePackProgress {
        progress: 0.0,
        status: "manifest".into(),
        resume: Some(token.clone()),
    });
    for (index, file) in pack.files.iter().enumerate() {
        transport.exchange('T', json!({"operation":"file","themeId":pack.theme,"fileIndex":index,"relativePath":file.path,"sizeBytes":file.data.len()}), |kind, value| kind == b'T' && value["operation"] == "ack" && value["itemType"] == "file" && value["fileIndex"] == index).await?;
    }
    transport
        .exchange(
            'T',
            json!({"operation":"end","themeId":pack.theme}),
            is_ready,
        )
        .await?;
    let mut completed = 0;
    for (index, file) in pack.files.iter().enumerate() {
        let count = file.data.len().div_ceil(chunk_size);
        let state = transport.exchange('P', json!({"themeId":pack.theme,"fileIndex":index,"sizeBytes":file.data.len(),"chunkSizeBytes":chunk_size,"chunkCount":count}), |kind, value| kind == b'P' && value["fileIndex"] == index && matches!(value["status"].as_str(), Some("ready" | "resume" | "complete"))).await?;
        let window = state["window"]
            .as_u64()
            .unwrap_or(max_window as u64)
            .clamp(1, max_window as u64) as usize;
        let mut received = received_chunks(&state, count)?;
        let mut finished = false;
        for _ in 0..MAX_ATTEMPTS {
            transport
                .send_chunks(index, file, chunk_size, window, &mut received, |bytes| {
                    progress(ResourcePackProgress {
                        progress: (completed + bytes) as f32 / pack.total.max(1) as f32 * 0.99,
                        status: file.path.clone(),
                        resume: Some(token.clone()),
                    });
                })
                .await?;
            let message = format!("C{index:04x}");
            let response = transport
                .exchange_text(&message, |text| {
                    text == message
                        || parse_control(text).is_some_and(|(kind, value)| {
                            kind == b'P' && value["fileIndex"] == index
                        })
                })
                .await?;
            if response == message {
                finished = true;
                break;
            }
            let (_, state) = parse_control(&response).context("invalid completion status")?;
            received = received_chunks(&state, count)?;
        }
        ensure!(finished, "file completion failed: {}", file.path);
        completed += file.data.len();
    }
    transport
        .exchange(
            'T',
            json!({"operation":"finish","themeId":pack.theme}),
            is_ready,
        )
        .await?;
    progress(ResourcePackProgress {
        progress: 1.0,
        status: "finished".into(),
        resume: None,
    });
    Ok(())
}

fn validate_chunk_limits(files: &[PackFile], chunk_size: usize) -> anyhow::Result<()> {
    ensure!(chunk_size > 0, "chunk size must be non-zero");
    for file in files {
        let chunk_count = file.data.len().div_ceil(chunk_size);
        ensure!(
            chunk_count <= MAX_CHUNKS_PER_FILE,
            "{} exceeds the Manager's 2,048-chunk limit",
            file.path
        );
    }
    Ok(())
}

fn is_ready(kind: u8, value: &Value) -> bool {
    kind == b'T' && value["operation"] == "status" && value["status"] == "ready"
}

fn parse_control(text: &str) -> Option<(u8, Value)> {
    let kind = *text.as_bytes().first()?;
    if !matches!(kind, b'H' | b'T' | b'P' | b'E') {
        return None;
    }
    Some((kind, serde_json::from_str(&text[1..]).ok()?))
}

fn received_chunks(state: &Value, count: usize) -> anyhow::Result<Vec<bool>> {
    ensure!(
        matches!(
            state["status"].as_str(),
            Some("ready" | "resume" | "complete")
        ),
        "invalid file status"
    );
    let mut received = vec![state["status"] == "complete"; count];
    if let Some(ranges) = state.get("receivedRanges") {
        for range in ranges.as_array().context("invalid receivedRanges")? {
            let range = range.as_array().context("invalid received range")?;
            ensure!(range.len() == 2, "invalid received range");
            let start = range[0].as_u64().context("invalid received range")? as usize;
            let end = range[1].as_u64().context("invalid received range")? as usize;
            ensure!(start <= end && end < count, "received range out of bounds");
            received[start..=end].fill(true);
        }
    }
    Ok(received)
}

struct Transport {
    addr: String,
    theme: String,
    max_chars: usize,
    events: broadcast::Receiver<CoreEvent>,
    session: Arc<Mutex<()>>,
}

impl Transport {
    async fn send(&self, text: &str) -> anyhow::Result<()> {
        ensure!(
            text.chars().count() <= self.max_chars,
            "interconnect message exceeds maxTextChars"
        );
        let addr = self.addr.clone();
        let session = self.session.clone();
        let current = crate::ecs::with_rt_mut(move |rt| {
            rt.component_ref::<ResourcePackComponent>(&addr)
                .is_some_and(|comp| Arc::ptr_eq(&comp.session, &session))
        })
        .await;
        ensure!(
            current,
            "device_not_connected: resource pack connection changed"
        );
        super::thirdparty_app::send_message(
            self.addr.clone(),
            MANAGER_PACKAGE.into(),
            text.as_bytes().to_vec(),
        )
        .await
    }

    async fn receive(&mut self, deadline: Instant) -> anyhow::Result<Option<String>> {
        loop {
            let event = match timeout_at(deadline, self.events.recv()).await {
                Err(_) => return Ok(None),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(result) => result.context("interconnect event channel closed")?,
            };
            let CoreEvent::InterconnectMessage(InterconnectMessage {
                device_addr,
                pkg_name,
                payload,
            }) = event
            else {
                continue;
            };
            if device_addr != self.addr || pkg_name != MANAGER_PACKAGE {
                continue;
            }
            ensure!(
                payload.len() <= MAX_TEXT_CHARS * 4,
                "oversized interconnect response"
            );
            let text = String::from_utf8(payload).context("invalid interconnect text")?;
            if let Some((kind, value)) = parse_control(&text) {
                if value
                    .get("themeId")
                    .is_some_and(|theme| theme != &self.theme)
                {
                    continue;
                }
                if kind == b'E' || value["status"] == "reject" {
                    bail!(
                        "res_pack: {}",
                        value["errorCode"].as_str().unwrap_or("rejected")
                    );
                }
            }
            return Ok(Some(text));
        }
    }

    async fn exchange(
        &mut self,
        kind: char,
        value: Value,
        accept: impl Fn(u8, &Value) -> bool,
    ) -> anyhow::Result<Value> {
        let text = format!("{kind}{value}");
        let response = self
            .exchange_text(&text, |text| {
                parse_control(text).is_some_and(|(kind, value)| accept(kind, &value))
            })
            .await?;
        Ok(parse_control(&response)
            .context("invalid control response")?
            .1)
    }

    async fn exchange_text(
        &mut self,
        text: &str,
        accept: impl Fn(&str) -> bool,
    ) -> anyhow::Result<String> {
        for _ in 0..MAX_ATTEMPTS {
            self.send(text).await?;
            let deadline = Instant::now() + ACK_TIMEOUT;
            while let Some(response) = self.receive(deadline).await? {
                if accept(&response) {
                    return Ok(response);
                }
            }
        }
        bail!("res_pack acknowledgement timed out ({})", &text[..1])
    }

    async fn send_chunks(
        &mut self,
        file_index: usize,
        file: &PackFile,
        chunk_size: usize,
        window: usize,
        received: &mut [bool],
        progress: impl Fn(usize),
    ) -> anyhow::Result<()> {
        let mut pending = BTreeMap::<usize, (Instant, usize)>::new();
        loop {
            let bytes = received
                .iter()
                .enumerate()
                .filter(|(_, done)| **done)
                .map(|(index, _)| chunk_size.min(file.data.len() - index * chunk_size))
                .sum();
            progress(bytes);
            if received.iter().all(|done| *done) {
                return Ok(());
            }
            for (index, done) in received.iter().enumerate() {
                if pending.len() >= window {
                    break;
                }
                if *done || pending.contains_key(&index) {
                    continue;
                }
                self.send_chunk(file_index, index, file, chunk_size).await?;
                pending.insert(index, (Instant::now() + ACK_TIMEOUT, 1));
            }
            let deadline = pending
                .values()
                .map(|(deadline, _)| *deadline)
                .min()
                .context("empty transfer window")?;
            if let Some(text) = self.receive(deadline).await? {
                if text.len() == 9 && text.starts_with('A') && text.is_ascii() {
                    if let (Ok(fi), Ok(ci)) = (
                        usize::from_str_radix(&text[1..5], 16),
                        usize::from_str_radix(&text[5..9], 16),
                    ) {
                        if fi == file_index && pending.remove(&ci).is_some() {
                            received[ci] = true;
                        }
                    }
                }
            }
            for (&index, (deadline, attempts)) in &mut pending {
                if *deadline <= Instant::now() {
                    ensure!(
                        *attempts < MAX_ATTEMPTS,
                        "res_pack chunk acknowledgement timed out: {file_index}:{index}"
                    );
                    self.send_chunk(file_index, index, file, chunk_size).await?;
                    *deadline = Instant::now() + ACK_TIMEOUT;
                    *attempts += 1;
                }
            }
        }
    }

    async fn send_chunk(
        &self,
        file_index: usize,
        index: usize,
        file: &PackFile,
        chunk_size: usize,
    ) -> anyhow::Result<()> {
        let start = index * chunk_size;
        let end = (start + chunk_size).min(file.data.len());
        self.send(&format!(
            "F{file_index:04x}{index:04x}{}",
            base91(&file.data[start..end])
        ))
        .await
    }
}

// Joachim Henke's basE91 alphabet; each chunk starts a fresh encoder.
fn base91(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 91] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&()*+,./:;<=>?@[]^_`{|}~\"";
    let (mut queue, mut bits) = (0u32, 0u32);
    let mut output = Vec::with_capacity(bytes.len() * 16 / 13 + 2);
    for &byte in bytes {
        queue |= (byte as u32) << bits;
        bits += 8;
        if bits > 13 {
            let mut value = queue & 8191;
            if value > 88 {
                queue >>= 13;
                bits -= 13;
            } else {
                value = queue & 16383;
                queue >>= 14;
                bits -= 14;
            }
            output.push(ALPHABET[(value % 91) as usize]);
            output.push(ALPHABET[(value / 91) as usize]);
        }
    }
    if bits > 0 {
        output.push(ALPHABET[(queue % 91) as usize]);
        if bits > 7 || queue > 90 {
            output.push(ALPHABET[(queue / 91) as usize]);
        }
    }
    String::from_utf8(output).expect("basE91 alphabet is ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MANIFEST: &[u8] = br#"{"format":"canopus-resource-pack","formatVersion":1,"themeId":"dark","name":"Dark","mappings":[]}"#;

    #[test]
    fn standard_base91_vectors_and_message_bound() {
        assert_eq!(base91(b""), "");
        assert_eq!(base91(b"test"), "fPNKd");
        assert_eq!(base91(b"Hello World!"), ">OwJh>Io0Tv!8PE");
        assert!(base91(&vec![255; 12_000]).len() + 9 <= MAX_TEXT_CHARS);
    }

    #[test]
    fn preflights_the_managers_chunk_limit_before_transfer() {
        let allowed = PackFile {
            path: "allowed.bin".into(),
            data: vec![0; MAX_CHUNKS_PER_FILE],
        };
        let rejected = PackFile {
            path: "too-large.bin".into(),
            data: vec![0; MAX_CHUNKS_PER_FILE + 1],
        };
        assert!(validate_chunk_limits(&[allowed], 1).is_ok());
        assert!(validate_chunk_limits(&[rejected], 1).is_err());
    }

    #[test]
    fn load_pack_from_crpack_zip_without_requiring_an_extension() {
        let zip_path = std::env::temp_dir().join(format!("test_dark_{}.zip", std::process::id()));
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);

        let options = zip::write::FileOptions::default();
        zip.start_file("canora.json", options).unwrap();
        zip.write_all(MANIFEST).unwrap();
        zip.start_file("app/test.bin", options).unwrap();
        zip.write_all(b"binary_payload").unwrap();
        zip.finish().unwrap();

        let result = load_pack(&zip_path);
        let _ = std::fs::remove_file(&zip_path);
        let pack = result.unwrap();
        assert_eq!(pack.theme, "dark");
        assert_eq!(pack.files.len(), 2);
        assert_eq!(pack.files[0].path, "canora.json");
        assert_eq!(pack.files[1].path, "app/test.bin");
        assert_eq!(pack.files[1].data, b"binary_payload");
        assert_eq!(pack.total, MANIFEST.len() + b"binary_payload".len());
    }
}
