use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use pb::xiaomi::protocol::{self, WearPacket};
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
        packet::mass::MassDataType,
        system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    },
    ecs::{Component, access::with_device_component_mut},
};

pub type MediaUploadFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<MediaUploadResult>> + Send>,
>;

#[derive(Debug, Clone)]
pub struct MediaUploadResult {
    pub song: protocol::Song,
    pub duplicated: bool,
}

#[derive(Component)]
pub struct MediaSystem {
    owner_id: String,
    summary_wait: RequestSlot<protocol::SongSummary>,
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
            summary_wait: RequestSlot::new(),
            song_page_wait: None,
            songlist_wait: None,
            song_remove_wait: None,
            song_add_wait: None,
            song_report_wait: None,
        }
    }

    pub fn request_song_summary(
        &mut self,
    ) -> oneshot::Receiver<Result<protocol::SongSummary>> {
        let (rx, should_enqueue) = self.summary_wait.prepare();
        if should_enqueue {
            self.enqueue_request(build_media_packet(
                protocol::media::MediaId::GetSongSummary,
                None,
            ));
        }
        rx
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
                protocol::song::AddRequest {
                    song: song.clone(),
                },
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

            let status = protocol::PrepareStatus::try_from(add_resp.prepare_status)
                .map_err(|_| anyhow_site!("unknown song prepare status: {}", add_resp.prepare_status))?;

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
                send_file_for_owner(owner.clone(), file_data, MassDataType::Music, move |progress| {
                    (progress_cb)(progress)
                })
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

impl L2PbExt for MediaSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
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
                            crate::events::emit(crate::events::CoreEvent::DeviceStateChanged(
                                crate::events::DeviceStateChanged {
                                    device_addr: self.owner_id.clone(),
                                },
                            ));
                            self.summary_wait.fulfill(summary);
                        }
                        Err(err) => {
                            let anyhow_err = anyhow_site!(
                                "failed to update media component with song summary: {err:?}"
                            );
                            log::error!("{anyhow_err:?}");
                            self.summary_wait.fail(anyhow_err);
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
                _ => {}
            }
        }
    }
}

#[derive(Component, Default, serde::Serialize)]
pub struct MediaComponent {
    pub summary: Option<protocol::SongSummary>,
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
