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
pub mod proto;
pub mod session;
pub use list::InstalledResourcePack;
use list::ListCollector;
pub use proto::CORONA_MODULE_ID;
use session::{
    CoronaTransport, LiveCoronaTransport, SessionConfig, prepare_archive_streaming,
    run_install_session, run_list_session,
};

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

/// Query a complete installed snapshot over CanopusConnect (`corona` module).
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
    let config = SessionConfig::default();
    let mut transport = LiveCoronaTransport::new(addr, session, config.host_max_rpc as usize)?;
    let result = run_list_session(&mut transport, config.control_timeout).await;
    transport.close().await;
    result
}

/// Legacy Interconnect list query kept for older quick-app compatibility switches.
pub async fn list_installed_legacy_interconnect(
    addr: String,
) -> anyhow::Result<Vec<InstalledResourcePack>> {
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
    if let Some(hello) = transport.handshake_for(MANAGER_PROBE_TIMEOUT).await? {
        transport.max_chars = parse_handshake_limits(&hello)?.0;
        return transport.list_installed().await;
    }

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
    if owned {
        if let Err(error) = transport.quit(&launch_token).await {
            log::debug!("[ResourcePack] Manager cleanup failed: {error:#}");
        }
    }
    result
}

/// Install a CRPack over CanopusConnect (`corona` binary protocol) by streaming directly
/// from `path` without copying the full archive into RAM.
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
    let inspect_path = path.clone();
    let prepared = tokio::task::spawn_blocking(move || prepare_archive_streaming(&inspect_path))
        .await
        .context("CRPack preflight worker panicked")??;
    if let Some(token) = &resume {
        ensure!(
            token.digest == prepared.digest_hex,
            "res_pack changed; remove the old task and start a replacement"
        );
    }
    let config = SessionConfig::default();
    let mut transport = LiveCoronaTransport::new(addr, lock, config.host_max_rpc as usize)?;
    let result =
        run_install_session(&mut transport, &path, &prepared, resume, config, progress).await;
    transport.close().await;
    result
}

/// Legacy Interconnect CRPack installer kept only for old-device compatibility switches.
pub async fn install_legacy_interconnect(
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
    use std::{
        collections::VecDeque,
        io::Write,
        sync::Mutex as StdMutex,
    };

    use crate::device::res_pack::proto::{
        CoronaCode, CoronaOp, CoronaPhase, FrameHeader, HelloCaps, LIST_END_INDEX,
        ProgressState, decode_frame, encode_frame,
    };
    use crate::device::res_pack::session::{
        CoronaTransport, RpcFuture, SessionConfig, WindowTracker, prepare_archive_streaming,
        run_install_session, run_list_session,
    };
    use sha2::{Digest, Sha256};
    use tokio::sync::mpsc;

    #[derive(Debug, Default)]
    struct MockDeviceState {
        connected: bool,
        listen_count: usize,
        hello_count: usize,
        open_count: usize,
        query_count: usize,
        finish_count: usize,
        cancel_count: usize,
        token_high_water: u32,
        active_token: u32,
        last_open_nonce: Option<[u8; 8]>,
        last_open_sha: Option<[u8; 32]>,
        last_open_size: u32,
        archive_bytes: u32,
        received_offset: u32,
        written_offset: u32,
        durable_offset: u32,
        buffer_used: u32,
        frames_since_flush: u8,
        phase: CoronaPhase,
        detail: CoronaCode,
        finish_polls_remaining: usize,
        disk: Vec<u8>,
        enqueued_data_offsets: Vec<u32>,
        drop_acks_count: usize,
        disconnect_at_offset: Option<u32>,
        reverse_rpc_pairs: bool,
        pending_rpc_replies: VecDeque<(u32, tokio::sync::oneshot::Sender<anyhow::Result<()>>)>,
        caps: HelloCaps,
    }

    impl MockDeviceState {
        fn new() -> Self {
            Self {
                connected: false,
                listen_count: 0,
                hello_count: 0,
                open_count: 0,
                query_count: 0,
                finish_count: 0,
                cancel_count: 0,
                token_high_water: 0x100,
                active_token: 0,
                last_open_nonce: None,
                last_open_sha: None,
                last_open_size: 0,
                archive_bytes: 0,
                received_offset: 0,
                written_offset: 0,
                durable_offset: 0,
                buffer_used: 0,
                frames_since_flush: 0,
                phase: CoronaPhase::Idle,
                detail: CoronaCode::Ok,
                finish_polls_remaining: 2,
                disk: Vec::new(),
                enqueued_data_offsets: Vec::new(),
                drop_acks_count: 0,
                disconnect_at_offset: None,
                reverse_rpc_pairs: false,
                pending_rpc_replies: VecDeque::new(),
                caps: HelloCaps::default(),
            }
        }

        fn flush_buffer(&mut self) {
            self.written_offset = self.received_offset;
            self.buffer_used = 0;
            self.frames_since_flush = 0;
            if self.written_offset - self.durable_offset >= self.caps.checkpoint_bytes
                || self.written_offset == self.archive_bytes
            {
                self.durable_offset = self.written_offset;
            }
        }

        fn progress(&self) -> ProgressState {
            ProgressState {
                token: self.active_token,
                written_offset: self.written_offset,
                durable_offset: self.durable_offset,
                window_frames: self.caps.window_frames,
                ack_every: self.caps.ack_every,
                phase: self.phase,
                detail: self.detail,
            }
        }
    }

    struct MockCoronaTransport {
        state: Arc<StdMutex<MockDeviceState>>,
        events_tx: mpsc::UnboundedSender<Vec<u8>>,
        events_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        max_rpc: usize,
        inflight_sem: Arc<tokio::sync::Semaphore>,
    }

    impl MockCoronaTransport {
        fn new(state: Arc<StdMutex<MockDeviceState>>) -> Self {
            let (events_tx, events_rx) = mpsc::unbounded_channel();
            Self {
                state,
                events_tx,
                events_rx,
                max_rpc: 2,
                inflight_sem: Arc::new(tokio::sync::Semaphore::new(2)),
            }
        }
    }

    impl CoronaTransport for MockCoronaTransport {
        fn connect_and_listen<'a>(
            &'a mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                let mut st = self.state.lock().unwrap();
                st.connected = true;
                st.listen_count += 1;
                Ok(())
            })
        }

        fn set_max_rpc(&mut self, max_rpc: usize) {
            self.max_rpc = max_rpc;
            self.inflight_sem = Arc::new(tokio::sync::Semaphore::new(max_rpc));
        }

        fn send_control<'a>(
            &'a mut self,
            frame: Vec<u8>,
            _timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                let (hdr, payload) = decode_frame(&frame)?;
                let op = CoronaOp::from_request_byte(hdr.op)?;
                let mut st = self.state.lock().unwrap();
                ensure!(st.connected, "device_not_connected");

                match op {
                    CoronaOp::Hello => {
                        st.hello_count += 1;
                        let resp = encode_frame(
                            FrameHeader {
                                op: CoronaOp::Hello.response_byte(),
                                code: CoronaCode::Ok as u8,
                                seq: hdr.seq,
                                token: 0,
                                arg: 0,
                            },
                            &st.caps.encode_payload(),
                        )?;
                        let _ = self.events_tx.send(resp);
                    }
                    CoronaOp::Open => {
                        st.open_count += 1;
                        let mut sha = [0u8; 32];
                        let mut nonce = [0u8; 8];
                        sha.copy_from_slice(&payload[0..32]);
                        nonce.copy_from_slice(&payload[32..40]);

                        let same_content = st.last_open_sha == Some(sha) && st.last_open_size == hdr.arg;
                        if st.last_open_nonce == Some(nonce) && same_content && st.active_token != 0 {
                            // Idempotent retry of same OPEN
                        } else {
                            st.token_high_water += 1;
                            st.active_token = st.token_high_water;
                            st.last_open_nonce = Some(nonce);
                            if !same_content {
                                st.last_open_sha = Some(sha);
                                st.last_open_size = hdr.arg;
                                st.archive_bytes = hdr.arg;
                                st.received_offset = 0;
                                st.written_offset = 0;
                                st.durable_offset = 0;
                                st.buffer_used = 0;
                                st.frames_since_flush = 0;
                                st.disk = vec![0u8; hdr.arg as usize];
                                st.phase = CoronaPhase::Receiving;
                                st.detail = CoronaCode::Ok;
                            } else {
                                // Resume from written_offset (discarding uncommitted RAM tail)
                                st.received_offset = st.written_offset;
                                st.buffer_used = 0;
                                st.frames_since_flush = 0;
                                if matches!(st.detail, CoronaCode::Offset | CoronaCode::Busy) {
                                    st.detail = CoronaCode::Ok;
                                }
                                if st.phase == CoronaPhase::Error && st.detail == CoronaCode::Ok {
                                    st.phase = CoronaPhase::Receiving;
                                }
                            }
                        }
                        let prog = st.progress();
                        let resp = encode_frame(
                            FrameHeader {
                                op: CoronaOp::Open.response_byte(),
                                code: CoronaCode::Ok as u8,
                                seq: hdr.seq,
                                token: st.active_token,
                                arg: prog.written_offset,
                            },
                            &prog.encode_payload(),
                        )?;
                        let _ = self.events_tx.send(resp);
                    }
                    CoronaOp::Query => {
                        st.query_count += 1;
                        if hdr.token != st.active_token {
                            let resp = encode_frame(
                                FrameHeader {
                                    op: CoronaOp::Query.response_byte(),
                                    code: CoronaCode::Stale as u8,
                                    seq: hdr.seq,
                                    token: hdr.token,
                                    arg: 0,
                                },
                                &[],
                            )?;
                            let _ = self.events_tx.send(resp);
                            return Ok(());
                        }
                        if st.phase == CoronaPhase::Receiving {
                            st.flush_buffer();
                        } else if matches!(st.phase, CoronaPhase::Verifying | CoronaPhase::Installing) {
                            if st.finish_polls_remaining > 1 {
                                st.finish_polls_remaining -= 1;
                                st.phase = CoronaPhase::Installing;
                            } else {
                                st.finish_polls_remaining = 0;
                                st.phase = CoronaPhase::Installed;
                            }
                        }
                        let prog = st.progress();
                        let code = if st.phase == CoronaPhase::Error {
                            st.detail
                        } else {
                            CoronaCode::Ok
                        };
                        let resp = encode_frame(
                            FrameHeader {
                                op: CoronaOp::Query.response_byte(),
                                code: code as u8,
                                seq: hdr.seq,
                                token: st.active_token,
                                arg: prog.written_offset,
                            },
                            &prog.encode_payload(),
                        )?;
                        let _ = self.events_tx.send(resp);
                    }
                    CoronaOp::Finish => {
                        st.finish_count += 1;
                        st.flush_buffer();
                        let mut hasher = Sha256::new();
                        hasher.update(&st.disk);
                        let disk_sha: [u8; 32] = hasher.finalize().into();
                        if Some(disk_sha) != st.last_open_sha {
                            st.phase = CoronaPhase::Error;
                            st.detail = CoronaCode::Hash;
                            let prog = st.progress();
                            let resp = encode_frame(
                                FrameHeader {
                                    op: CoronaOp::Finish.response_byte(),
                                    code: CoronaCode::Hash as u8,
                                    seq: hdr.seq,
                                    token: st.active_token,
                                    arg: prog.written_offset,
                                },
                                &prog.encode_payload(),
                            )?;
                            let _ = self.events_tx.send(resp);
                            return Ok(());
                        }
                        st.phase = CoronaPhase::Verifying;
                        let prog = st.progress();
                        let resp = encode_frame(
                            FrameHeader {
                                op: CoronaOp::Finish.response_byte(),
                                code: CoronaCode::Ok as u8,
                                seq: hdr.seq,
                                token: st.active_token,
                                arg: prog.written_offset,
                            },
                            &prog.encode_payload(),
                        )?;
                        let _ = self.events_tx.send(resp);
                    }
                    CoronaOp::Cancel => {
                        st.cancel_count += 1;
                        st.phase = CoronaPhase::Canceled;
                        st.detail = CoronaCode::Ok;
                        let prog = st.progress();
                        let resp = encode_frame(
                            FrameHeader {
                                op: CoronaOp::Cancel.response_byte(),
                                code: CoronaCode::Ok as u8,
                                seq: hdr.seq,
                                token: st.active_token,
                                arg: prog.written_offset,
                            },
                            &prog.encode_payload(),
                        )?;
                        let _ = self.events_tx.send(resp);
                    }
                    CoronaOp::List => {
                        let rev = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                        let code = if rev != 0 && rev != 1 {
                            CoronaCode::Changed
                        } else {
                            CoronaCode::Ok
                        };
                        let mut out_payload = Vec::new();
                        if code == CoronaCode::Ok {
                            out_payload.extend_from_slice(&1u32.to_le_bytes()); // revision = 1
                            out_payload.extend_from_slice(&1u32.to_le_bytes()); // total = 1
                            out_payload.extend_from_slice(&LIST_END_INDEX.to_le_bytes());
                            out_payload.push(0b011); // version + versionCode
                            out_payload.push(4); // "dark"
                            out_payload.push(4); // "Dark"
                            out_payload.push(5); // "1.0.0"
                            out_payload.push(0); // no author
                            out_payload.push(0); // metadata ok
                            out_payload.extend_from_slice(&7u64.to_le_bytes());
                            out_payload.extend_from_slice(b"darkDark1.0.0");
                        }
                        let resp = encode_frame(
                            FrameHeader {
                                op: CoronaOp::List.response_byte(),
                                code: code as u8,
                                seq: hdr.seq,
                                token: 0,
                                arg: hdr.arg,
                            },
                            &out_payload,
                        )?;
                        let _ = self.events_tx.send(resp);
                    }
                    CoronaOp::Data => unreachable!(),
                }
                Ok(())
            })
        }

        fn submit_data<'a>(
            &'a mut self,
            offset: u32,
            frame: Vec<u8>,
            _timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<RpcFuture>> + Send + 'a>> {
            Box::pin(async move {
                let permit = Arc::clone(&self.inflight_sem).acquire_owned().await?;
                let (hdr, payload) = decode_frame(&frame)?;
                let mut st = self.state.lock().unwrap();
                ensure!(st.connected, "device_not_connected");

                if st.disconnect_at_offset == Some(offset) {
                    st.disconnect_at_offset = None;
                    st.connected = false;
                    // Discard un-flushed RAM buffer on disconnect/session drop, keeping written_offset.
                    st.received_offset = st.written_offset;
                    st.buffer_used = 0;
                    st.frames_since_flush = 0;
                    while let Some((_, tx)) = st.pending_rpc_replies.pop_front() {
                        let _ = tx.send(Err(anyhow::anyhow!("device_not_connected")));
                    }
                    bail!("device_not_connected: simulated mid-stream disconnect");
                }

                st.enqueued_data_offsets.push(offset);
                if hdr.token == st.active_token && st.phase == CoronaPhase::Receiving {
                    let len = payload.len() as u32;
                    if offset < st.received_offset {
                        // Complete duplicate within received prefix is ignored.
                        if offset + len > st.received_offset {
                            st.phase = CoronaPhase::Error;
                            st.detail = CoronaCode::Offset;
                        }
                    } else if offset == st.received_offset && offset + len <= st.archive_bytes {
                        let start = offset as usize;
                        let end = start + payload.len();
                        st.disk[start..end].copy_from_slice(payload);
                        st.received_offset += len;
                        st.buffer_used += len;
                        st.frames_since_flush += 1;
                        // Single 4 KiB buffer invariant on device
                        assert!(st.buffer_used <= 4096);
                        if st.frames_since_flush >= st.caps.ack_every
                            || st.received_offset == st.archive_bytes
                        {
                            st.flush_buffer();
                            if st.drop_acks_count > 0 {
                                st.drop_acks_count -= 1;
                            } else {
                                let prog = st.progress();
                                let ack = encode_frame(
                                    FrameHeader {
                                        op: CoronaOp::Data.response_byte(),
                                        code: 0,
                                        seq: 0,
                                        token: st.active_token,
                                        arg: prog.written_offset,
                                    },
                                    &prog.encode_payload(),
                                )?;
                                let _ = self.events_tx.send(ack);
                            }
                        }
                    } else {
                        st.phase = CoronaPhase::Error;
                        st.detail = CoronaCode::Offset;
                    }
                }

                let (tx, rx) = tokio::sync::oneshot::channel();
                if st.reverse_rpc_pairs {
                    st.pending_rpc_replies.push_back((offset, tx));
                    if st.pending_rpc_replies.len() >= 2
                        || st.received_offset == st.archive_bytes
                        || st.frames_since_flush == 0
                    {
                        // Deliver CPC2 responses in reverse order to verify request correlation!
                        while let Some((_, reply_tx)) = st.pending_rpc_replies.pop_back() {
                            let _ = reply_tx.send(Ok(()));
                        }
                    }
                } else {
                    let _ = tx.send(Ok(()));
                }

                Ok(Box::pin(async move {
                    let _permit = permit;
                    rx.await.context("mock RPC channel closed")?
                }) as RpcFuture)
            })
        }

        fn recv_frame<'a>(
            &'a mut self,
            deadline: Instant,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send + 'a>,
        > {
            Box::pin(async move {
                match timeout_at(deadline, self.events_rx.recv()).await {
                    Err(_) => Ok(None),
                    Ok(Some(frame)) => Ok(Some(frame)),
                    Ok(None) => bail!("mock event channel closed"),
                }
            })
        }
    }

    #[tokio::test]
    async fn end_to_end_1mib_crpack_streaming_window_out_of_order_ack_loss_and_reconnect() {
        let zip_path = std::env::temp_dir().join(format!(
            "corona_e2e_1mib_{}.crpack",
            std::process::id()
        ));
        let expected_zip_bytes = {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let stored = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("corona.json", stored).unwrap();
            zip.write_all(MANIFEST).unwrap();
            // Add a ~1 MiB asset so archive is > 1 MiB.
            let mut payload = vec![0u8; 1024 * 1024];
            for (i, b) in payload.iter_mut().enumerate() {
                *b = ((i * 31 + 17) & 0xff) as u8;
            }
            zip.start_file("assets/large_wallpaper.bin", stored).unwrap();
            zip.write_all(&payload).unwrap();
            zip.finish().unwrap();
            std::fs::read(&zip_path).unwrap()
        };
        assert!(expected_zip_bytes.len() > 1024 * 1024);

        let prepared = prepare_archive_streaming(&zip_path).unwrap();
        assert_eq!(prepared.theme_id, "dark");
        assert_eq!(prepared.archive_bytes as usize, expected_zip_bytes.len());

        let state = Arc::new(StdMutex::new(MockDeviceState::new()));
        {
            let mut st = state.lock().unwrap();
            st.caps.max_rpc = 3;
            st.drop_acks_count = 2; // Drop 2 consecutive ACKs (8 frames = full window) to force QUERY barrier recovery
            st.disconnect_at_offset = Some(468 * 25); // Disconnect mid-stream at frame 25
            st.reverse_rpc_pairs = true; // Return CPC2 responses out-of-order
        }

        let mut transport = MockCoronaTransport::new(state.clone());
        let config = SessionConfig {
            host_max_rpc: 3,
            control_timeout: Duration::from_millis(200),
            data_rpc_timeout: Duration::from_millis(200),
            ack_wait: Duration::from_millis(25),
            finish_poll_interval: Duration::from_millis(5),
            max_recovery_rounds: 6,
            max_finish_polls: 10,
        };

        let phases = Arc::new(StdMutex::new(Vec::<String>::new()));
        let phases_cb = phases.clone();
        run_install_session(
            &mut transport,
            &zip_path,
            &prepared,
            None,
            config,
            move |prog| {
                let mut list = phases_cb.lock().unwrap();
                if list.last() != Some(&prog.status) {
                    list.push(prog.status);
                }
            },
        )
        .await
        .unwrap();

        let _ = std::fs::remove_file(&zip_path);

        let st = state.lock().unwrap();
        assert_eq!(st.phase, CoronaPhase::Installed);
        assert_eq!(st.written_offset, prepared.archive_bytes);
        assert_eq!(st.disk, expected_zip_bytes);
        // Reconnected after simulated disconnect -> at least 2 HELLO and 2 OPEN calls with new token!
        assert!(st.hello_count >= 2);
        assert!(st.open_count >= 2);
        // Lost ACK triggered QUERY barrier recovery + FINISH polling!
        assert!(st.query_count >= 3);
        assert_eq!(st.finish_count, 1);

        let seen_phases = phases.lock().unwrap().clone();
        assert!(seen_phases.contains(&"uploading".to_string()));
        assert!(seen_phases.contains(&"verifying".to_string()));
        assert!(seen_phases.contains(&"installing".to_string()));
        assert_eq!(seen_phases.last().map(String::as_str), Some("installed"));
    }

    #[tokio::test]
    async fn corona_binary_list_paginates_and_validates_revision() {
        let state = Arc::new(StdMutex::new(MockDeviceState::new()));
        let mut transport = MockCoronaTransport::new(state);
        let items = run_list_session(&mut transport, Duration::from_millis(200))
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].theme_id, "dark");
        assert_eq!(items[0].name, "Dark");
        assert_eq!(items[0].version.as_deref(), Some("1.0.0"));
        assert_eq!(items[0].version_code, Some(7));
        assert_eq!(items[0].metadata_status, "ok");
    }

    #[test]
    fn window_tracker_enforces_byte_and_rpc_caps_and_ignores_stale_or_duplicate_acks() {
        let caps = HelloCaps {
            max_rpc: 2,
            window_frames: 8,
            ack_every: 4,
            chunk_bytes: 468,
            ..HelloCaps::default()
        };
        let init = ProgressState {
            token: 10,
            written_offset: 0,
            durable_offset: 0,
            window_frames: 8,
            ack_every: 4,
            phase: CoronaPhase::Receiving,
            detail: CoronaCode::Ok,
        };
        let mut tracker = WindowTracker::new(&caps, &init, 5000, 2);
        assert_eq!(tracker.window_limit(), 3744);
        assert_eq!(tracker.next_chunk_range(), Some((0, 468)));
        tracker.record_sent(468);
        assert_eq!(tracker.next_chunk_range(), Some((468, 468)));
        tracker.record_sent(468);
        // 2 RPCs in flight -> blocked by max_rpc even though byte window has room
        assert_eq!(tracker.next_chunk_range(), None);
        tracker.record_rpc_completed();
        assert_eq!(tracker.next_chunk_range(), Some((936, 468)));

        // Duplicate ACK at 0 does not move window; ACK at 468 advances window to 468 + 3744 = 4212
        assert!(!tracker.apply_progress(&init).unwrap());
        let ack1 = ProgressState {
            written_offset: 468,
            ..init
        };
        assert!(tracker.apply_progress(&ack1).unwrap());
        assert_eq!(tracker.acked_offset, 468);
        assert_eq!(tracker.window_limit(), 4212);
        // Smaller ACK is ignored without rewinding acked_offset
        assert!(!tracker.apply_progress(&init).unwrap());
        assert_eq!(tracker.acked_offset, 468);
    }

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
