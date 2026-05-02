use std::{io::Cursor, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use pb::xiaomi::protocol::{self, WearPacket};
use prost::{
    Message,
    encoding::{DecodeContext, WireType, decode_key, decode_length_delimiter, skip_field},
};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::{
    anyhow_site, bail_site,
    device::xiaomi::{
        components::{
            mass::{
                SendMassCallbackData, send_file_for_owner,
                send_file_for_owner_with_known_slice_length,
            },
            shared::{HasOwnerId, RequestSlot, SystemRequestExt},
        },
        packet::{
            mass::MassDataType,
            v2::layer2::{L2Channel, L2OpCode},
        },
        system::{XiaomiSystemExt, register_xiaomi_system_ext_on_l2packet},
    },
    ecs::{Component, access::with_device_component_mut},
};

#[cfg(target_arch = "wasm32")]
pub type MediaUploadFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<MediaUploadResult>>>>;

#[cfg(not(target_arch = "wasm32"))]
pub type MediaUploadFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<MediaUploadResult>> + Send>>;

#[derive(Debug, Clone)]
pub struct MediaUploadResult {
    pub song: protocol::Song,
    pub duplicated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaFileDescriptor {
    pub identifier: Option<protocol::media_file::Identifier>,
    pub name: String,
    pub size: Option<u64>,
    pub duration_secs: Option<u32>,
    pub created_at_ms: Option<u64>,
    pub media_type: Option<protocol::media_file::Type>,
}

impl MediaFileDescriptor {
    pub fn stable_key(&self) -> String {
        self.identifier
            .as_ref()
            .map(|value| value.id.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| self.name.clone())
    }
}

#[derive(Component)]
pub struct MediaSystem {
    owner_id: String,
    song_summary_wait: RequestSlot<protocol::SongSummary>,
    media_file_summary_wait: RequestSlot<protocol::media_file::Summary>,
    media_file_list_wait: Option<oneshot::Sender<Result<Vec<MediaFileDescriptor>>>>,
    song_page_wait: Option<oneshot::Sender<Result<protocol::song::GetResponse>>>,
    songlist_wait: Option<oneshot::Sender<Result<protocol::songlist::Response>>>,
    song_remove_wait: Option<oneshot::Sender<Result<protocol::song::RemoveResponse>>>,
    song_add_wait: Option<oneshot::Sender<Result<protocol::song::AddResponse>>>,
    song_report_wait: Option<oneshot::Sender<Result<protocol::song::ReportResult>>>,
}

impl Default for MediaSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl MediaSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            owner_id,
            song_summary_wait: RequestSlot::new(),
            media_file_summary_wait: RequestSlot::new(),
            media_file_list_wait: None,
            song_page_wait: None,
            songlist_wait: None,
            song_remove_wait: None,
            song_add_wait: None,
            song_report_wait: None,
        }
    }

    pub fn request_song_summary(&mut self) -> oneshot::Receiver<Result<protocol::SongSummary>> {
        let (rx, should_enqueue) = self.song_summary_wait.prepare();
        if should_enqueue {
            self.enqueue_request(build_media_packet(
                protocol::media::MediaId::GetSongSummary,
                None,
            ));
        }
        rx
    }

    pub fn request_media_file_summary(
        &mut self,
    ) -> oneshot::Receiver<Result<protocol::media_file::Summary>> {
        let (rx, should_enqueue) = self.media_file_summary_wait.prepare();
        if should_enqueue {
            self.enqueue_request(build_media_packet(
                protocol::media::MediaId::GetMediaFileSummary,
                None,
            ));
        }
        rx
    }

    pub fn request_media_file_list(
        &mut self,
    ) -> Result<oneshot::Receiver<Result<Vec<MediaFileDescriptor>>>> {
        let rx = prepare_single_waiter(&mut self.media_file_list_wait, "media file list request")?;
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::SyncMediaFileList,
            None,
        ));
        Ok(rx)
    }

    pub fn request_media_file_list_compat(
        &mut self,
    ) -> Result<oneshot::Receiver<Result<Vec<MediaFileDescriptor>>>> {
        let rx = prepare_single_waiter(
            &mut self.media_file_list_wait,
            "media file list compatibility request",
        )?;
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::SyncMediaFileList,
            Some(protocol::media::Payload::MediaFileList(
                protocol::media_file::List { list: Vec::new() },
            )),
        ));
        Ok(rx)
    }

    pub fn request_media_file(&mut self, identifier: protocol::media_file::Identifier) {
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::RequestMediaFile,
            Some(protocol::media::Payload::MediaFileIdentifier(identifier)),
        ));
    }

    pub fn request_media_files(&mut self, identifiers: Vec<protocol::media_file::Identifier>) {
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::RequestMediaFileList,
            Some(protocol::media::Payload::MediaFileIdentifiers(
                protocol::media_file::identifier::List { list: identifiers },
            )),
        ));
    }

    pub fn confirm_media_file(&mut self, identifier: protocol::media_file::Identifier) {
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::ConfirmMediaFile,
            Some(protocol::media::Payload::MediaFileIdentifier(identifier)),
        ));
    }

    pub fn request_song_page(
        &mut self,
        index: u32,
    ) -> Result<oneshot::Receiver<Result<protocol::song::GetResponse>>> {
        let rx = prepare_single_waiter(&mut self.song_page_wait, "song page request")?;
        let request = protocol::song::GetRequest { index };
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::GetSong,
            Some(protocol::media::Payload::SongGetRequest(request)),
        ));
        Ok(rx)
    }

    pub fn request_songlist_operation(
        &mut self,
        request: protocol::songlist::Request,
        media_id: protocol::media::MediaId,
    ) -> Result<oneshot::Receiver<Result<protocol::songlist::Response>>> {
        let rx = prepare_single_waiter(&mut self.songlist_wait, "songlist request")?;
        self.enqueue_request(build_media_packet(
            media_id,
            Some(protocol::media::Payload::SonglistRequest(request)),
        ));
        Ok(rx)
    }

    pub fn request_remove_song(
        &mut self,
        song_id: Vec<u8>,
    ) -> Result<oneshot::Receiver<Result<protocol::song::RemoveResponse>>> {
        let rx = prepare_single_waiter(&mut self.song_remove_wait, "song remove request")?;
        let request = protocol::song::RemoveRequest { id: song_id };
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::RemoveSong,
            Some(protocol::media::Payload::SongRemoveRequest(request)),
        ));
        Ok(rx)
    }

    pub fn upload_song_with_progress(
        &mut self,
        song: protocol::Song,
        file_data: Vec<u8>,
        progress_cb: Arc<dyn Fn(SendMassCallbackData) + Send + Sync>,
    ) -> Result<MediaUploadFuture> {
        let owner = self.owner_id.clone();
        let add_rx = prepare_single_waiter(&mut self.song_add_wait, "song add request")?;
        self.enqueue_request(build_media_packet(
            protocol::media::MediaId::AddSong,
            Some(protocol::media::Payload::SongAddRequest(
                protocol::song::AddRequest { song: song.clone() },
            )),
        ));

        Ok(Box::pin(async move {
            let add_resp = match tokio::time::timeout(Duration::from_secs(15), add_rx).await {
                Ok(Ok(Ok(resp))) => resp,
                Ok(Ok(Err(err))) => {
                    clear_song_add_waiter_for_owner(owner.clone()).await;
                    return Err(err).context("song add request failed");
                }
                Ok(Err(err)) => {
                    clear_song_add_waiter_for_owner(owner.clone()).await;
                    return Err(anyhow_site!("song add response channel closed: {err:?}"));
                }
                Err(_) => {
                    clear_song_add_waiter_for_owner(owner.clone()).await;
                    return Err(anyhow_site!("timed out waiting for song add response"));
                }
            };

            let status =
                protocol::PrepareStatus::try_from(add_resp.prepare_status).map_err(|_| {
                    anyhow_site!("unknown song prepare status: {}", add_resp.prepare_status)
                })?;

            match status {
                protocol::PrepareStatus::Ready => {}
                protocol::PrepareStatus::Duplicated => {
                    return Ok(MediaUploadResult {
                        song,
                        duplicated: true,
                    });
                }
                protocol::PrepareStatus::LowStorage => {
                    bail_site!("device reported low storage while preparing music upload");
                }
                other => {
                    bail_site!("music upload prepare failed: {:?}", other);
                }
            }

            let report_rx = prepare_song_report_waiter_for_owner(owner.clone()).await?;
            let expected_slice_length = add_resp.expected_slice_length() as usize;

            if expected_slice_length == 0 {
                log::warn!(
                    "music upload add response returned slice length 0, falling back to MASS prepare"
                );
                send_file_for_owner(
                    owner.clone(),
                    file_data,
                    MassDataType::Music,
                    move |progress| (progress_cb)(progress),
                )
                .await
                .context("failed to send music MASS payload with prepare fallback")?;
            } else {
                send_file_for_owner_with_known_slice_length(
                    owner.clone(),
                    file_data,
                    MassDataType::Music,
                    expected_slice_length,
                    move |progress| (progress_cb)(progress),
                )
                .await
                .context("failed to send music MASS payload")?;
            }

            let report = match tokio::time::timeout(Duration::from_secs(90), report_rx).await {
                Ok(Ok(Ok(report))) => report,
                Ok(Ok(Err(err))) => {
                    clear_song_report_waiter_for_owner(owner.clone()).await;
                    return Err(err).context("music upload result failed");
                }
                Ok(Err(err)) => {
                    clear_song_report_waiter_for_owner(owner.clone()).await;
                    return Err(anyhow_site!(
                        "music upload result channel closed unexpectedly: {err:?}"
                    ));
                }
                Err(_) => {
                    clear_song_report_waiter_for_owner(owner.clone()).await;
                    return Err(anyhow_site!("timed out waiting for music upload result"));
                }
            };

            let code = protocol::song::report_result::Code::try_from(report.code)
                .map_err(|_| anyhow_site!("unknown music upload report code: {}", report.code))?;
            if code != protocol::song::report_result::Code::Success {
                bail_site!("music upload failed with device report: {:?}", code);
            }
            if let Some(report_id) = report.id.filter(|id| !id.is_empty()) {
                if report_id != song.id {
                    bail_site!("music upload result id does not match uploaded song");
                }
            }

            Ok(MediaUploadResult {
                song,
                duplicated: false,
            })
        }))
    }

    fn enqueue_request(&mut self, request: protocol::WearPacket) {
        self.enqueue_pb_request(request, "MediaSystem::enqueue_request");
    }

    fn handle_pb_packet(&mut self, payload: WearPacket) {
        if let Some(protocol::wear_packet::Payload::Media(media)) = payload.payload {
            match media.payload {
                Some(protocol::media::Payload::SongSummary(summary)) => {
                    let comp_summary = summary.clone();
                    let update_res = with_device_component_mut::<MediaComponent, _, _>(
                        self.owner_id.clone(),
                        move |comp| {
                            comp.summary = Some(comp_summary);
                        },
                    );

                    match update_res {
                        Ok(_) => {
                            emit_media_state_changed(self.owner_id.clone());
                            self.song_summary_wait.fulfill(summary);
                        }
                        Err(err) => {
                            let anyhow_err = anyhow_site!(
                                "failed to update media component with song summary: {err:?}"
                            );
                            log::error!("{anyhow_err:?}");
                            self.song_summary_wait.fail(anyhow_err);
                        }
                    }
                }
                Some(protocol::media::Payload::MediaFileSummary(summary)) => {
                    let comp_summary = summary.clone();
                    let update_res = with_device_component_mut::<MediaComponent, _, _>(
                        self.owner_id.clone(),
                        move |comp| {
                            comp.media_file_summary = Some(comp_summary);
                        },
                    );
                    match update_res {
                        Ok(_) => {
                            emit_media_state_changed(self.owner_id.clone());
                            self.media_file_summary_wait.fulfill(summary);
                        }
                        Err(err) => {
                            let anyhow_err = anyhow_site!(
                                "failed to update media component with media file summary: {err:?}"
                            );
                            log::error!("{anyhow_err:?}");
                            self.media_file_summary_wait.fail(anyhow_err);
                        }
                    }
                }
                Some(protocol::media::Payload::SongGetResponse(resp)) => {
                    fulfill_single_waiter(&mut self.song_page_wait, resp);
                }
                Some(protocol::media::Payload::SonglistResponse(resp)) => {
                    fulfill_single_waiter(&mut self.songlist_wait, resp);
                }
                Some(protocol::media::Payload::SongAddResponse(resp)) => {
                    fulfill_single_waiter(&mut self.song_add_wait, resp);
                }
                Some(protocol::media::Payload::SongRemoveResponse(resp)) => {
                    fulfill_single_waiter(&mut self.song_remove_wait, resp);
                }
                Some(protocol::media::Payload::SongReportResult(resp)) => {
                    fulfill_single_waiter(&mut self.song_report_wait, resp);
                }
                Some(protocol::media::Payload::RecordResponse(_))
                | Some(protocol::media::Payload::RecordStatus(_))
                | Some(protocol::media::Payload::RecordRequest(_))
                | Some(protocol::media::Payload::MediaFileList(_))
                | Some(protocol::media::Payload::MediaFileIdentifier(_))
                | Some(protocol::media::Payload::MediaFileIdentifiers(_))
                | Some(protocol::media::Payload::PlayerInfo(_))
                | Some(protocol::media::Payload::PlayerControl(_))
                | Some(protocol::media::Payload::SonglistRequest(_))
                | Some(protocol::media::Payload::SongGetRequest(_))
                | Some(protocol::media::Payload::SongAddRequest(_))
                | Some(protocol::media::Payload::SongRemoveRequest(_))
                | None => {}
            }
        }
    }

    fn handle_media_file_list_payload(&mut self, payload: &[u8]) {
        let decoded = decode_media_file_list(payload)
            .with_context(|| "decode media file list payload")
            .map_err(|err| anyhow_site!("{err:#}"));

        match decoded {
            Ok(list) => {
                let list_for_component = list.clone();
                let update_res = with_device_component_mut::<MediaComponent, _, _>(
                    self.owner_id.clone(),
                    move |comp| {
                        comp.media_files = list_for_component;
                    },
                );
                match update_res {
                    Ok(_) => {
                        emit_media_state_changed(self.owner_id.clone());
                        fulfill_single_waiter(&mut self.media_file_list_wait, list);
                    }
                    Err(err) => {
                        let anyhow_err = anyhow_site!(
                            "failed to update media component with media file list: {err:?}"
                        );
                        log::error!("{anyhow_err:?}");
                        fail_single_waiter(&mut self.media_file_list_wait, anyhow_err);
                    }
                }
            }
            Err(err) => {
                log::warn!(
                    "[MediaSystem] failed to decode media file list payload for {}: {err:#}",
                    self.owner_id
                );
                fail_single_waiter(&mut self.media_file_list_wait, err);
            }
        }
    }

    pub fn clear_media_file_list_wait(&mut self) {
        self.media_file_list_wait = None;
    }

    pub fn clear_song_page_wait(&mut self) {
        self.song_page_wait = None;
    }

    pub fn clear_songlist_wait(&mut self) {
        self.songlist_wait = None;
    }

    pub fn clear_song_add_wait(&mut self) {
        self.song_add_wait = None;
    }

    pub fn clear_song_report_wait(&mut self) {
        self.song_report_wait = None;
    }
}

impl HasOwnerId for MediaSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

impl XiaomiSystemExt for MediaSystem {
    fn on_layer2_packet(&mut self, channel: L2Channel, _opcode: L2OpCode, payload: &[u8]) {
        if channel != L2Channel::Pb {
            return;
        }

        let packet_id = extract_varint_field(payload, 2).unwrap_or(None);
        if packet_id == Some(protocol::media::MediaId::ReportMediaFileList as u64) {
            match extract_length_delimited_field(payload, 20).and_then(|media_bytes| {
                match media_bytes {
                    Some(bytes) => extract_length_delimited_field(&bytes, 14),
                    None => Ok(None),
                }
            }) {
                Ok(Some(list_payload)) => self.handle_media_file_list_payload(&list_payload),
                Ok(None) => {
                    log::warn!(
                        "[MediaSystem] media file list report on {} did not contain payload",
                        self.owner_id
                    );
                }
                Err(err) => {
                    let err = anyhow_site!("decode media file list wrapper failed: {err:#}");
                    log::warn!("[MediaSystem] {err:#}");
                    fail_single_waiter(&mut self.media_file_list_wait, err);
                }
            }
        }

        match WearPacket::decode(Cursor::new(payload)) {
            Ok(packet) => self.handle_pb_packet(packet),
            Err(err) => {
                log::warn!(
                    "failed to decode Xiaomi PB payload for MediaSystem ({} bytes): {}",
                    payload.len(),
                    err
                );
            }
        }
    }
}

#[derive(Component, Default, serde::Serialize)]
pub struct MediaComponent {
    pub summary: Option<protocol::SongSummary>,
    pub media_file_summary: Option<protocol::media_file::Summary>,
    #[serde(skip_serializing)]
    pub media_files: Vec<MediaFileDescriptor>,
}

fn build_media_packet(
    id: protocol::media::MediaId,
    payload: Option<protocol::media::Payload>,
) -> protocol::WearPacket {
    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::Media as i32,
        id: id as u32,
        payload: Some(protocol::wear_packet::Payload::Media(protocol::Media {
            payload,
        })),
    }
}

fn prepare_single_waiter<T>(
    slot: &mut Option<oneshot::Sender<Result<T>>>,
    context: &'static str,
) -> Result<oneshot::Receiver<Result<T>>> {
    if slot.is_some() {
        bail_site!("{} already in progress", context);
    }
    let (tx, rx) = oneshot::channel();
    *slot = Some(tx);
    Ok(rx)
}

fn fulfill_single_waiter<T>(slot: &mut Option<oneshot::Sender<Result<T>>>, value: T) {
    if let Some(tx) = slot.take() {
        if tx.send(Ok(value)).is_err() {
            log::debug!("media waiter receiver dropped before fulfillment");
        }
    }
}

fn fail_single_waiter<T>(slot: &mut Option<oneshot::Sender<Result<T>>>, err: anyhow::Error) {
    if let Some(tx) = slot.take() {
        if tx.send(Err(err)).is_err() {
            log::debug!("media waiter receiver dropped before failure");
        }
    }
}

fn emit_media_state_changed(device_addr: String) {
    crate::events::emit(crate::events::CoreEvent::DeviceStateChanged(
        crate::events::DeviceStateChanged { device_addr },
    ));
}

fn extract_varint_field(raw: &[u8], target_tag: u32) -> Result<Option<u64>> {
    let mut buf = raw;
    while !buf.is_empty() {
        let (tag, wire_type) = decode_key(&mut buf)
            .map_err(|err| anyhow_site!("decode protobuf key failed: {err}"))?;
        match wire_type {
            WireType::Varint if tag == target_tag => {
                return prost::encoding::decode_varint(&mut buf)
                    .map(Some)
                    .map_err(|err| anyhow_site!("decode protobuf varint failed: {err}"));
            }
            other => {
                skip_field(other, tag, &mut buf, DecodeContext::default())
                    .map_err(|err| anyhow_site!("skip protobuf field failed: {err}"))?;
            }
        }
    }
    Ok(None)
}

fn extract_length_delimited_field(raw: &[u8], target_tag: u32) -> Result<Option<Vec<u8>>> {
    let mut buf = raw;
    let mut last = None;

    while !buf.is_empty() {
        let (tag, wire_type) = decode_key(&mut buf)
            .map_err(|err| anyhow_site!("decode protobuf key failed: {err}"))?;
        match wire_type {
            WireType::LengthDelimited => {
                let len = decode_length_delimiter(&mut buf)
                    .map_err(|err| anyhow_site!("decode protobuf length failed: {err}"))?;
                if buf.len() < len {
                    bail_site!(
                        "protobuf length-delimited field exceeds remaining buffer (len={}, remaining={})",
                        len,
                        buf.len()
                    );
                }
                let (field, rest) = buf.split_at(len);
                if tag == target_tag {
                    last = Some(field.to_vec());
                }
                buf = rest;
            }
            other => {
                skip_field(other, tag, &mut buf, DecodeContext::default())
                    .map_err(|err| anyhow_site!("skip protobuf field failed: {err}"))?;
            }
        }
    }

    Ok(last)
}

fn extract_repeated_length_delimited_fields(raw: &[u8], target_tag: u32) -> Result<Vec<Vec<u8>>> {
    let mut buf = raw;
    let mut fields = Vec::new();

    while !buf.is_empty() {
        let (tag, wire_type) = decode_key(&mut buf)
            .map_err(|err| anyhow_site!("decode protobuf key failed: {err}"))?;
        match wire_type {
            WireType::LengthDelimited => {
                let len = decode_length_delimiter(&mut buf)
                    .map_err(|err| anyhow_site!("decode protobuf length failed: {err}"))?;
                if buf.len() < len {
                    bail_site!(
                        "protobuf repeated field exceeds remaining buffer (len={}, remaining={})",
                        len,
                        buf.len()
                    );
                }
                let (field, rest) = buf.split_at(len);
                if tag == target_tag {
                    fields.push(field.to_vec());
                }
                buf = rest;
            }
            other => {
                skip_field(other, tag, &mut buf, DecodeContext::default())
                    .map_err(|err| anyhow_site!("skip protobuf field failed: {err}"))?;
            }
        }
    }

    Ok(fields)
}

fn decode_media_file_list(payload: &[u8]) -> Result<Vec<MediaFileDescriptor>> {
    extract_repeated_length_delimited_fields(payload, 1)?
        .into_iter()
        .map(|item| decode_media_file_descriptor(&item))
        .collect()
}

fn decode_media_file_descriptor(raw: &[u8]) -> Result<MediaFileDescriptor> {
    let mut buf = raw;
    let mut string_fields: Vec<(u32, String)> = Vec::new();
    let mut varint_fields: Vec<(u32, u64)> = Vec::new();
    let mut identifier: Option<protocol::media_file::Identifier> = None;
    let mut media_type = None;
    let mut size = None;
    let mut created_at_ms = None;
    let mut duration_secs = None;

    while !buf.is_empty() {
        let (tag, wire_type) = decode_key(&mut buf)
            .map_err(|err| anyhow_site!("decode protobuf key failed: {err}"))?;
        match wire_type {
            WireType::Varint => {
                let value = prost::encoding::decode_varint(&mut buf)
                    .map_err(|err| anyhow_site!("decode protobuf varint failed: {err}"))?;
                match tag {
                    2 if value <= i32::MAX as u64 => {
                        media_type = protocol::media_file::Type::try_from(value as i32).ok();
                    }
                    3 => size = Some(value),
                    4 => created_at_ms = normalize_media_file_timestamp(value),
                    5 => duration_secs = normalize_media_file_duration(value),
                    _ => {}
                }
                varint_fields.push((tag, value));
            }
            WireType::LengthDelimited => {
                let len = decode_length_delimiter(&mut buf)
                    .map_err(|err| anyhow_site!("decode protobuf length failed: {err}"))?;
                if buf.len() < len {
                    bail_site!(
                        "media file field exceeds remaining buffer (len={}, remaining={})",
                        len,
                        buf.len()
                    );
                }
                let (field, rest) = buf.split_at(len);
                if tag == 1 && identifier.is_none() {
                    if let Ok(candidate) =
                        protocol::media_file::Identifier::decode(Cursor::new(field))
                    {
                        if !candidate.id.is_empty() {
                            identifier = Some(candidate);
                        }
                    }
                }
                if let Ok(text) = String::from_utf8(field.to_vec()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        string_fields.push((tag, trimmed.to_string()));
                    }
                }
                buf = rest;
            }
            other => {
                skip_field(other, tag, &mut buf, DecodeContext::default())
                    .map_err(|err| anyhow_site!("skip protobuf field failed: {err}"))?;
            }
        }
    }

    let identifier_id = identifier.as_ref().map(|value| value.id.as_str());
    let name = infer_media_file_name(&string_fields, identifier_id);
    let media_type = media_type.or_else(|| infer_media_file_type(&varint_fields));
    let created_at_ms = created_at_ms.or_else(|| infer_media_file_timestamp(&varint_fields));
    let size = size.or_else(|| infer_media_file_size(&varint_fields, created_at_ms));
    let duration_secs =
        duration_secs.or_else(|| infer_media_file_duration(&varint_fields, created_at_ms, size));

    Ok(MediaFileDescriptor {
        identifier,
        name,
        size,
        duration_secs,
        created_at_ms,
        media_type,
    })
}

fn infer_media_file_name(fields: &[(u32, String)], identifier_id: Option<&str>) -> String {
    let id = identifier_id.unwrap_or_default();

    let preferred = fields
        .iter()
        .map(|(_, value)| value)
        .find(|value| value.as_str() != id && looks_like_media_name(value));

    preferred
        .cloned()
        .or_else(|| {
            fields
                .iter()
                .map(|(_, value)| value)
                .find(|value| !value.trim().is_empty())
                .cloned()
        })
        .unwrap_or_else(|| {
            if id.is_empty() {
                "record".to_string()
            } else {
                display_name_from_identifier(id)
            }
        })
}

fn display_name_from_identifier(value: &str) -> String {
    let trimmed = value.trim();
    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn infer_media_file_type(fields: &[(u32, u64)]) -> Option<protocol::media_file::Type> {
    fields.iter().find_map(|(_, value)| {
        if *value > i32::MAX as u64 {
            return None;
        }
        protocol::media_file::Type::try_from(*value as i32).ok()
    })
}

fn infer_media_file_timestamp(fields: &[(u32, u64)]) -> Option<u64> {
    let millis = fields
        .iter()
        .filter_map(|(_, value)| {
            if *value >= 1_000_000_000_000 {
                Some(*value)
            } else {
                None
            }
        })
        .max();
    if millis.is_some() {
        return millis;
    }

    fields
        .iter()
        .filter_map(|(_, value)| {
            if *value >= 946_684_800 && *value <= 4_102_444_800 {
                Some(*value * 1000)
            } else {
                None
            }
        })
        .max()
}

fn infer_media_file_size(fields: &[(u32, u64)], timestamp_ms: Option<u64>) -> Option<u64> {
    fields
        .iter()
        .map(|(_, value)| *value)
        .filter(|value| Some(*value) != timestamp_ms)
        .filter(|value| *value >= 1024)
        .max()
}

fn infer_media_file_duration(
    fields: &[(u32, u64)],
    timestamp_ms: Option<u64>,
    size: Option<u64>,
) -> Option<u32> {
    let mut best: Option<u32> = None;

    for value in fields.iter().map(|(_, value)| *value) {
        if Some(value) == timestamp_ms || Some(value) == size {
            continue;
        }
        let secs = if value <= 86_400 {
            Some(value as u32)
        } else if value % 1000 == 0 && value / 1000 <= 86_400 {
            Some((value / 1000) as u32)
        } else {
            None
        };
        if let Some(candidate) = secs.filter(|candidate| *candidate > 0) {
            best = Some(
                best.map(|current| current.min(candidate))
                    .unwrap_or(candidate),
            );
        }
    }

    best
}

fn normalize_media_file_timestamp(value: u64) -> Option<u64> {
    if value >= 1_000_000_000_000 {
        return Some(value);
    }
    if (946_684_800..=4_102_444_800).contains(&value) {
        return Some(value * 1000);
    }
    None
}

fn normalize_media_file_duration(value: u64) -> Option<u32> {
    if value == 0 {
        return None;
    }
    if value <= 86_400 {
        return Some(value as u32);
    }
    if value % 1000 == 0 && value / 1000 <= 86_400 {
        return Some((value / 1000) as u32);
    }
    None
}

fn looks_like_media_name(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains('.') || trimmed.contains(' ') {
        return true;
    }
    let ascii = trimmed
        .chars()
        .all(|ch| ch.is_ascii_hexdigit() || ch == '-');
    !ascii
}

pub async fn prepare_song_report_waiter_for_owner(
    owner_id: String,
) -> Result<oneshot::Receiver<Result<protocol::song::ReportResult>>> {
    crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&owner_id, |world, entity| {
            let mut system = world
                .get_mut::<MediaSystem>(entity)
                .ok_or_else(|| anyhow_site!("Media system not found"))?;
            prepare_single_waiter(&mut system.song_report_wait, "song report wait")
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await
}

pub async fn clear_media_file_list_waiter_for_owner(owner_id: String) {
    let _ = crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&owner_id, |world, entity| {
            if let Some(mut system) = world.get_mut::<MediaSystem>(entity) {
                system.clear_media_file_list_wait();
            }
            Ok::<_, anyhow::Error>(())
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await;
}

pub async fn clear_song_add_waiter_for_owner(owner_id: String) {
    let _ = crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&owner_id, |world, entity| {
            if let Some(mut system) = world.get_mut::<MediaSystem>(entity) {
                system.clear_song_add_wait();
            }
            Ok::<_, anyhow::Error>(())
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await;
}

pub async fn clear_song_report_waiter_for_owner(owner_id: String) {
    let _ = crate::ecs::with_rt_mut(move |rt| {
        rt.with_device_mut(&owner_id, |world, entity| {
            if let Some(mut system) = world.get_mut::<MediaSystem>(entity) {
                system.clear_song_report_wait();
            }
            Ok::<_, anyhow::Error>(())
        })
        .ok_or_else(|| anyhow_site!("Device not found"))?
    })
    .await;
}
