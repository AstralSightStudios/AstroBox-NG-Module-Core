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

mod list;
pub use list::InstalledResourcePack;
use list::ListCollector;

pub const MANAGER_PACKAGE: &str = "ng.lst.corona";
const MAX_TEXT_CHARS: usize = 18_000;
const ACK_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_RETRY: Duration = Duration::from_millis(750);
const MAX_ATTEMPTS: usize = 4;
static REQUEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const LIST_ATTEMPTS: usize = 2;
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGER_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

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

/// Query a complete installed snapshot without closing a user-opened Manager.
/// The device session lock also prevents a query from interrupting an upload.
pub async fn list_installed(addr: String) -> anyhow::Result<Vec<InstalledResourcePack>> {
    let device_addr = addr.clone();
    let session = crate::ecs::with_rt_mut(move |rt| {
        rt.component_ref::<ResourcePackComponent>(&device_addr)
            .map(|comp| comp.session.clone())
            .context("res_pack is not supported by this device")
    })
    .await?;
    let _guard = session
        .clone()
        .try_lock_owned()
        .context("a resource pack session is already running")?;
    let mut transport = Transport {
        addr: addr.clone(),
        theme: String::new(),
        max_chars: MAX_TEXT_CHARS,
        events: crate::events::subscribe(),
        session,
    };
    // A responsive Manager already belongs to the user. Do not navigate or
    // claim its lifetime, even if the subsequent list query fails.
    if let Some(hello) = transport.handshake_for(MANAGER_PROBE_TIMEOUT).await? {
        transport.max_chars = parse_handshake_limits(&hello)?.0;
        return transport.list_installed().await;
    }

    // A timeout does not prove the app was closed. Only a cold-start token
    // acknowledged by Manager gives this check permission to send Q.
    let launch_token = new_request_id();
    let mut owned = false;
    let result = async {
        transport.launch_for_update_check(&launch_token).await?;
        let hello = transport.handshake().await?;
        transport.max_chars = parse_handshake_limits(&hello)?.0;
        owned = confirms_launch_ownership(&hello, &launch_token);
        transport.list_installed().await
    }
    .await;
    // Unknown/legacy launch contexts, failed handshakes and an already-running
    // Manager are left open. Retain the session lock through owned cleanup.
    if owned {
        if let Err(error) = transport.quit(&launch_token).await {
            log::debug!("[ResourcePack] Manager cleanup failed: {error:#}");
        }
    }
    result
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
    transport.launch().await?;
    let hello = transport.handshake().await?;
    let (max_chars, max_window) = parse_handshake_limits(&hello)?;
    transport.max_chars = max_chars;
    let max_chunk = 12_000.min((transport.max_chars - 11) * 13 / 16);
    let chunk_size = resume.as_ref().map_or(max_chunk, |token| token.chunk_size);
    ensure!(
        chunk_size > 0 && chunk_size <= max_chunk,
        "resume chunk size exceeds negotiated message limit"
    );
    validate_chunk_limits(&pack.files, chunk_size)?;
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

fn new_request_id() -> String {
    let nonce = crate::tools::generate_random_bytes(16);
    let count = REQUEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}_{count:x}", hex::encode(nonce))
}

fn is_announce(text: &str) -> bool {
    parse_control(text).is_some_and(|(kind, value)| {
        kind == b'H' && value["version"] == 2 && value["type"] == "announce"
    })
}

fn is_handshake_response(text: &str, request_id: &str) -> bool {
    parse_control(text).is_some_and(|(kind, value)| {
        kind == b'H'
            && value["version"] == 2
            && value["type"] == "response"
            && value["replyTo"] == request_id
    })
}

fn parse_handshake_limits(value: &Value) -> anyhow::Result<(usize, usize)> {
    let max_chars = value["maxTextChars"]
        .as_u64()
        .context("invalid maxTextChars")?;
    ensure!(
        (256..=MAX_TEXT_CHARS as u64).contains(&max_chars),
        "maxTextChars is outside the supported range"
    );
    let max_window = value["maxWindow"].as_u64().context("invalid maxWindow")?;
    ensure!((1..=4).contains(&max_window), "maxWindow is outside 1..=4");
    Ok((max_chars as usize, max_window as usize))
}

fn is_ready(kind: u8, value: &Value) -> bool {
    kind == b'T' && value["operation"] == "status" && value["status"] == "ready"
}

enum ManagerPacket {
    Text(String),
    List(Value),
}

fn decode_manager_packet(payload: &[u8], max_chars: usize) -> anyhow::Result<ManagerPacket> {
    let envelope: Value =
        serde_json::from_slice(payload).context("invalid Manager message envelope")?;
    let packet = envelope
        .get("msg")
        .and_then(Value::as_str)
        .context("Manager message envelope is missing string msg")?;
    if packet == "L" {
        ensure!(
            serde_json::to_string(&envelope)?.chars().count() <= max_chars,
            "Manager list response exceeds maxTextChars"
        );
        return Ok(ManagerPacket::List(envelope));
    }
    ensure!(
        packet.chars().count() <= max_chars,
        "Manager protocol packet exceeds maxTextChars"
    );
    Ok(ManagerPacket::Text(packet.to_owned()))
}

#[cfg(test)]
fn decode_manager_envelope(payload: &[u8]) -> anyhow::Result<String> {
    match decode_manager_packet(payload, MAX_TEXT_CHARS)? {
        ManagerPacket::Text(text) => Ok(text),
        ManagerPacket::List(_) => bail!("structured list is not a text packet"),
    }
}

fn parse_control(text: &str) -> Option<(u8, Value)> {
    let kind = *text.as_bytes().first()?;
    if !matches!(kind, b'H' | b'T' | b'P' | b'E' | b'Q') {
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

fn confirms_launch_ownership(hello: &Value, launch_token: &str) -> bool {
    hello["launchToken"].as_str() == Some(launch_token)
}

fn update_check_launch_uri(launch_token: &str) -> String {
    format!("hap://app/{MANAGER_PACKAGE}/pages/index?astroboxCheckToken={launch_token}")
}

enum ManagerCommand {
    Launch(String),
    Send(Vec<u8>),
}

// Validate connection ownership and enqueue in one ECS turn. In particular an
// old query must never send Q to a newly connected device at the same address.
fn enqueue_manager_command(
    rt: &mut crate::ecs::runtime::Runtime,
    addr: &str,
    expected_session: &Arc<Mutex<()>>,
    deadline: Instant,
    command: ManagerCommand,
) -> anyhow::Result<()> {
    use super::xiaomi::components::{
        resource::ResourceComponent,
        thirdparty_app::{AppInfo, ThirdpartyAppSystem},
    };
    rt.with_device_mut(addr, |world, entity| {
        ensure!(
            world
                .get::<ResourcePackComponent>(entity)
                .is_some_and(|comp| Arc::ptr_eq(&comp.session, expected_session)),
            "device_not_connected: resource pack connection changed"
        );
        // ECS jobs survive cancellation of their awaiting future. Suppress a
        // timed-out launch/send rather than performing a late external effect.
        ensure!(
            Instant::now() < deadline,
            "Manager command deadline expired"
        );
        let resources = world
            .get::<ResourceComponent>(entity)
            .context("Xiaomi resource component not found")?;
        let app = resources
            .quick_apps
            .iter()
            .find(|app| app.package_name == MANAGER_PACKAGE)
            .context("resource Manager is not installed")?;
        let info = AppInfo {
            package_name: app.package_name.clone(),
            fingerprint: app.fingerprint.clone(),
        };
        let mut system = world
            .get_mut::<ThirdpartyAppSystem>(entity)
            .context("Xiaomi thirdparty app system not found")?;
        match command {
            ManagerCommand::Launch(uri) => system.launch_app(&info, &uri),
            ManagerCommand::Send(payload) => system.send_phone_message(&info, payload),
        }
    })
    .context("device_not_connected: device not found")?
}

struct Transport {
    addr: String,
    theme: String,
    max_chars: usize,
    events: broadcast::Receiver<CoreEvent>,
    session: Arc<Mutex<()>>,
}

impl Transport {
    async fn handshake(&mut self) -> anyhow::Result<Value> {
        self.handshake_for(HANDSHAKE_TIMEOUT)
            .await?
            .context("res_pack handshake timed out")
    }

    async fn handshake_for(&mut self, duration: Duration) -> anyhow::Result<Option<Value>> {
        let request_id = new_request_id();
        let text = format!(
            "H{}",
            json!({"version":2,"type":"request","requestId":request_id,"maxTextChars":MAX_TEXT_CHARS})
        );
        let deadline = Instant::now() + duration;
        self.send_handshake_request(&text, deadline).await?;
        let mut next_send = Instant::now() + HANDSHAKE_RETRY;
        loop {
            let receive_deadline = next_send.min(deadline);
            if let Some(response) = self.receive(receive_deadline).await? {
                if is_handshake_response(&response, &request_id) {
                    let (_, value) = parse_control(&response).expect("validated control message");
                    return Ok(Some(value));
                }
                if is_announce(&response) {
                    self.send_handshake_request(&text, deadline).await?;
                    next_send = Instant::now() + HANDSHAKE_RETRY;
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            if Instant::now() >= next_send {
                self.send_handshake_request(&text, deadline).await?;
                next_send = Instant::now() + HANDSHAKE_RETRY;
            }
        }
    }

    async fn send_handshake_request(&self, text: &str, deadline: Instant) -> anyhow::Result<()> {
        match self.send_until(text, deadline).await {
            Ok(()) => Ok(()),
            Err(error) if format!("{error:#}").contains("device_not_connected") => Err(error),
            Err(_) => Ok(()),
        }
    }

    async fn ensure_current_session(&self) -> anyhow::Result<()> {
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
        Ok(())
    }

    async fn command(&self, command: ManagerCommand, deadline: Instant) -> anyhow::Result<()> {
        let addr = self.addr.clone();
        let session = self.session.clone();
        timeout_at(
            deadline,
            crate::ecs::with_rt_mut(move |rt| {
                enqueue_manager_command(rt, &addr, &session, deadline, command)
            }),
        )
        .await
        .context("Manager command timed out")?
    }

    async fn launch(&self) -> anyhow::Result<()> {
        self.command(
            ManagerCommand::Launch(String::new()),
            Instant::now() + HANDSHAKE_TIMEOUT,
        )
        .await
    }

    async fn launch_for_update_check(&self, launch_token: &str) -> anyhow::Result<()> {
        self.command(
            ManagerCommand::Launch(update_check_launch_uri(launch_token)),
            Instant::now() + HANDSHAKE_TIMEOUT,
        )
        .await
    }

    async fn send_until(&self, text: &str, deadline: Instant) -> anyhow::Result<()> {
        ensure!(
            text.chars().count() <= self.max_chars,
            "interconnect message exceeds maxTextChars"
        );
        self.command(ManagerCommand::Send(text.as_bytes().to_vec()), deadline)
            .await
    }

    async fn send(&self, text: &str) -> anyhow::Result<()> {
        self.send_until(text, Instant::now() + ACK_TIMEOUT).await
    }

    async fn receive_packet(&mut self, deadline: Instant) -> anyhow::Result<Option<ManagerPacket>> {
        loop {
            let event = match timeout_at(deadline, self.events.recv()).await {
                Err(_) => return Ok(None),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(result) => result.context("interconnect event channel closed")?,
            };
            if let CoreEvent::DeviceStateChanged(change) = &event {
                if change.device_addr == self.addr {
                    self.ensure_current_session().await?;
                }
                continue;
            }
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
            self.ensure_current_session().await?;
            return Ok(Some(decode_manager_packet(&payload, self.max_chars)?));
        }
    }

    async fn receive(&mut self, deadline: Instant) -> anyhow::Result<Option<String>> {
        while let Some(packet) = self.receive_packet(deadline).await? {
            let ManagerPacket::Text(text) = packet else {
                continue;
            };
            if let Some((kind, value)) = parse_control(&text) {
                // Q belongs only to quit(), which reads packets directly. A
                // delayed cleanup rejection must not abort a later upload.
                if kind == b'Q' {
                    continue;
                }
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
        Ok(None)
    }

    async fn list_installed(&mut self) -> anyhow::Result<Vec<InstalledResourcePack>> {
        for _ in 0..LIST_ATTEMPTS {
            self.ensure_current_session().await?;
            // A retry reads a new Manager snapshot and must not mix old pages.
            let request_id = new_request_id();
            let mut collector = ListCollector::new(request_id.clone());
            let deadline = Instant::now() + ACK_TIMEOUT;
            self.send_until(&format!("L{}", json!({"requestId":request_id})), deadline)
                .await?;
            while let Some(packet) = self.receive_packet(deadline).await? {
                if let ManagerPacket::List(value) = packet {
                    if let Some(items) = collector.push(value)? {
                        return Ok(items);
                    }
                }
            }
        }
        bail!("resource pack list timed out")
    }

    async fn quit(&mut self, launch_token: &str) -> anyhow::Result<()> {
        let deadline = Instant::now() + QUIT_TIMEOUT;
        timeout_at(deadline, async {
            let request_id = new_request_id();
            self.send_until(
                &format!(
                    "Q{}",
                    json!({"requestId":request_id,"launchToken":launch_token})
                ),
                deadline,
            )
            .await?;
            while let Some(packet) = self.receive_packet(deadline).await? {
                let ManagerPacket::Text(text) = packet else {
                    continue;
                };
                if let Some((b'Q', value)) = parse_control(&text) {
                    if value["replyTo"] == request_id {
                        ensure!(
                            value["status"] == "ready",
                            "Manager refused Q: {}",
                            value["errorCode"]
                        );
                        return Ok(());
                    }
                }
            }
            bail!("Manager Q acknowledgement timed out")
        })
        .await
        .context("Manager Q timed out")?
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
    fn manager_message_envelope_unwraps_the_existing_text_packet() {
        let packet = r#"H{"version":2,"type":"response","replyTo":"request_1"}"#;
        let payload = serde_json::to_vec(&json!({ "msg": packet })).unwrap();
        assert_eq!(decode_manager_envelope(&payload).unwrap(), packet);
    }

    #[test]
    fn manager_message_envelope_requires_a_string_msg() {
        assert!(decode_manager_envelope(br#"{}"#).is_err());
        assert!(decode_manager_envelope(br#"{"msg":42}"#).is_err());
    }

    #[test]
    fn structured_list_keeps_fields_and_uses_whole_object_length() {
        let value = json!({"msg":"L","replyTo":"list_42","pageIndex":0,
            "done":true,"total":0,"items":[]});
        let payload = serde_json::to_vec(&value).unwrap();
        match decode_manager_packet(&payload, MAX_TEXT_CHARS).unwrap() {
            ManagerPacket::List(list) => assert_eq!(list, value),
            ManagerPacket::Text(_) => panic!("L must not become a bare text packet"),
        }
        assert!(decode_manager_packet(&payload, 1).is_err());
        assert!(decode_manager_envelope(&payload).is_err());
    }

    #[test]
    fn quit_is_a_text_control_packet_with_reply_correlation() {
        let value = json!({"replyTo":"quit_42","status":"ready"});
        let text = format!("Q{value}");
        let payload = serde_json::to_vec(&json!({"msg":text})).unwrap();
        assert_eq!(decode_manager_envelope(&payload).unwrap(), text);
        let (kind, decoded) = parse_control(&text).unwrap();
        assert_eq!(kind, b'Q');
        assert_eq!(decoded, value);
    }

    #[test]
    fn cleanup_requires_the_exact_cold_launch_token() {
        let token = "check_42";
        assert!(confirms_launch_ownership(
            &json!({"launchToken":token}),
            token
        ));
        for hello in [
            json!({}),
            json!({"launchToken":null}),
            json!({"launchToken":42}),
            json!({"launchToken":"other_check"}),
        ] {
            assert!(!confirms_launch_ownership(&hello, token));
        }
    }

    #[test]
    fn update_check_launch_uses_explicit_context_not_the_normal_launch_uri() {
        let token = new_request_id();
        assert_eq!(
            update_check_launch_uri(&token),
            format!("hap://app/ng.lst.corona/pages/index?astroboxCheckToken={token}")
        );
        // Tokens need no escaping and cannot inject a second query parameter.
        assert!(!token.contains(['?', '&', '/', '=']));
    }

    #[test]
    fn handshake_accepts_only_matching_v2_response_not_announce() {
        let id = "request_123-abc";
        assert!(is_announce(
            r#"H{"version":2,"type":"announce","maxTextChars":18000,"maxWindow":4}"#
        ));
        assert!(!is_handshake_response(
            r#"H{"version":2,"type":"announce","requestId":"request_123-abc"}"#,
            id
        ));
        assert!(!is_handshake_response(
            r#"H{"version":2,"type":"response","replyTo":"older"}"#,
            id
        ));
        assert!(!is_handshake_response(
            r#"H{"version":1,"type":"response","replyTo":"request_123-abc"}"#,
            id
        ));
        let response = r#"H{"version":2,"type":"response","replyTo":"request_123-abc","maxTextChars":18000,"maxWindow":4}"#;
        assert!(is_handshake_response(response, id));
        let (_, value) = parse_control(response).unwrap();
        assert_eq!(parse_handshake_limits(&value).unwrap(), (18_000, 4));
    }

    #[test]
    fn handshake_rejects_invalid_negotiated_limits() {
        let invalid = [
            json!({"maxTextChars":18000}),
            json!({"maxTextChars":"18000","maxWindow":4}),
            json!({"maxTextChars":255,"maxWindow":4}),
            json!({"maxTextChars":18001,"maxWindow":4}),
            json!({"maxTextChars":18000,"maxWindow":0}),
            json!({"maxTextChars":18000,"maxWindow":5}),
            json!({"maxTextChars":18000,"maxWindow":"4"}),
        ];
        for value in invalid {
            assert!(parse_handshake_limits(&value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn request_ids_are_unique_and_wire_safe() {
        let first = new_request_id();
        let second = new_request_id();
        let valid = |id: &str| {
            (1..=64).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        assert!(valid(&first));
        assert!(valid(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn stale_sessions_cannot_enqueue_launch_or_quit_on_a_reconnected_device() {
        let mut rt = crate::ecs::runtime::Runtime::new();
        let original = ResourcePackComponent::default();
        let session = original.session.clone();
        rt.spawn_device("device".into(), (original,));
        rt.spawn_device("device".into(), (ResourcePackComponent::default(),));
        for command in [
            ManagerCommand::Launch(String::new()),
            ManagerCommand::Send(b"Q{}".to_vec()),
        ] {
            let error = enqueue_manager_command(
                &mut rt,
                "device",
                &session,
                Instant::now() + ACK_TIMEOUT,
                command,
            )
            .unwrap_err();
            assert!(error.to_string().contains("connection changed"));
        }
    }

    #[test]
    fn cancelled_command_jobs_cannot_perform_late_side_effects() {
        let mut rt = crate::ecs::runtime::Runtime::new();
        let component = ResourcePackComponent::default();
        let session = component.session.clone();
        rt.spawn_device("device".into(), (component,));
        for command in [
            ManagerCommand::Launch(String::new()),
            ManagerCommand::Send(b"Q{}".to_vec()),
        ] {
            let error = enqueue_manager_command(
                &mut rt,
                "device",
                &session,
                Instant::now() - Duration::from_secs(1),
                command,
            )
            .unwrap_err();
            assert!(error.to_string().contains("deadline expired"));
        }
    }

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
        zip.start_file("corona.json", options).unwrap();
        zip.write_all(MANIFEST).unwrap();
        zip.start_file("app/test.bin", options).unwrap();
        zip.write_all(b"binary_payload").unwrap();
        zip.finish().unwrap();

        let result = load_pack(&zip_path);
        let _ = std::fs::remove_file(&zip_path);
        let pack = result.unwrap();
        assert_eq!(pack.theme, "dark");
        assert_eq!(pack.files.len(), 2);
        assert_eq!(pack.files[0].path, "corona.json");
        assert_eq!(pack.files[1].path, "app/test.bin");
        assert_eq!(pack.files[1].data, b"binary_payload");
        assert_eq!(pack.total, MANIFEST.len() + b"binary_payload".len());
    }
}
