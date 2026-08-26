use pb::xiaomi::protocol::{self, WearPacket};
use tokio::sync::oneshot;

use crate::{
    anyhow_site,
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::Component,
};

use super::shared::{HasOwnerId, RequestSlot, SystemRequestExt};

#[derive(Component)]
pub struct WatchfaceSystem {
    owner_id: String,
    set_current_wait: RequestSlot<()>,
    uninstall_wait: RequestSlot<()>,
    edit_wait: RequestSlot<protocol::EditResponse>,
    bg_image_wait: RequestSlot<protocol::BgImageResult>,
    font_wait: RequestSlot<protocol::FontResult>,
    support_data_wait: RequestSlot<Vec<i32>>,
}

impl Default for WatchfaceSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl WatchfaceSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            owner_id,
            set_current_wait: RequestSlot::new(),
            uninstall_wait: RequestSlot::new(),
            edit_wait: RequestSlot::new(),
            bg_image_wait: RequestSlot::new(),
            font_wait: RequestSlot::new(),
            support_data_wait: RequestSlot::new(),
        }
    }

    /// Fire-and-forget variant retained for callers that do not need the
    /// device acknowledgement.  Xiaomi's WatchFace protocol does expose a
    /// `Success` response; use `request_set_watchface` when completion matters.
    pub fn set_watchface(&mut self, watchface_id: &str) -> anyhow::Result<()> {
        self.enqueue_request(build_watchface_set(watchface_id))
    }

    /// Fire-and-forget variant retained for callers that do not need the
    /// device acknowledgement.  Use `request_uninstall_watchface` when
    /// completion matters.
    pub fn uninstall_watchface(&mut self, watchface_id: &str) -> anyhow::Result<()> {
        self.enqueue_request(build_watchface_uninstall(watchface_id))
    }

    pub fn request_set_watchface(
        &mut self,
        watchface_id: &str,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let (rx, should_enqueue) = self.set_current_wait.prepare();
        if should_enqueue {
            if let Err(err) = self.enqueue_request(build_watchface_set(watchface_id)) {
                self.set_current_wait.fail(err);
            }
        }
        rx
    }

    pub fn request_uninstall_watchface(
        &mut self,
        watchface_id: &str,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let (rx, should_enqueue) = self.uninstall_wait.prepare();
        if should_enqueue {
            if let Err(err) = self.enqueue_request(build_watchface_uninstall(watchface_id)) {
                self.uninstall_wait.fail(err);
            }
        }
        rx
    }

    pub fn request_edit(
        &mut self,
        request: protocol::EditRequest,
    ) -> oneshot::Receiver<anyhow::Result<protocol::EditResponse>> {
        let (rx, _should_enqueue) = self.edit_wait.prepare();
        if let Err(err) = self.enqueue_request(build_watchface_edit(request)) {
            self.edit_wait.fail(err);
        }
        rx
    }

    pub fn prepare_bg_image_wait(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<protocol::BgImageResult>> {
        let (rx, _should_enqueue) = self.bg_image_wait.prepare();
        rx
    }

    pub fn prepare_font_wait(&mut self) -> oneshot::Receiver<anyhow::Result<protocol::FontResult>> {
        let (rx, _should_enqueue) = self.font_wait.prepare();
        rx
    }

    pub fn request_support_data(&mut self) -> oneshot::Receiver<anyhow::Result<Vec<i32>>> {
        let (rx, should_enqueue) = self.support_data_wait.prepare();
        if should_enqueue {
            if let Err(err) = self.enqueue_request(build_watchface_get_support_data()) {
                self.support_data_wait.fail(err);
            }
        }
        rx
    }

    fn enqueue_request(&mut self, packet: protocol::WearPacket) -> anyhow::Result<()> {
        self.try_enqueue_pb_request(packet, "WatchfaceSystem::enqueue_request")
    }

    fn handle_set_response(&mut self, success: bool) {
        if success {
            self.set_current_wait.fulfill(());
        } else {
            self.set_current_wait.fail(anyhow_site!(
                "Xiaomi watch rejected set-current watchface request"
            ));
        }
    }

    fn handle_uninstall_response(&mut self, success: bool) {
        if success {
            self.uninstall_wait.fulfill(());
        } else {
            self.uninstall_wait.fail(anyhow_site!(
                "Xiaomi watch rejected uninstall watchface request"
            ));
        }
    }
}

impl L2PbExt for WatchfaceSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
        let packet_id = payload.id;
        if let Some(protocol::wear_packet::Payload::WatchFace(msg)) = payload.payload {
            match msg.payload {
                Some(protocol::watch_face::Payload::Success(success))
                    if packet_id == protocol::watch_face::WatchFaceId::SetWatchFace as u32 =>
                {
                    self.handle_set_response(success);
                }
                Some(protocol::watch_face::Payload::Success(success))
                    if packet_id == protocol::watch_face::WatchFaceId::RemoveWatchFace as u32 =>
                {
                    self.handle_uninstall_response(success);
                }
                Some(protocol::watch_face::Payload::EditResponse(resp)) => {
                    log::debug!(
                        "[Watchface] edit response: {:?}",
                        serde_json::to_string(&resp).unwrap_or_default()
                    );
                    self.edit_wait.fulfill(resp);
                }
                Some(protocol::watch_face::Payload::BgImageResult(result)) => {
                    log::debug!(
                        "[Watchface] bg image result: {:?}",
                        serde_json::to_string(&result).unwrap_or_default()
                    );
                    self.bg_image_wait.fulfill(result);
                }
                Some(protocol::watch_face::Payload::SupportDataList(list)) => {
                    self.support_data_wait.fulfill(list.list);
                }
                Some(protocol::watch_face::Payload::FontResult(result)) => {
                    log::debug!(
                        "[Watchface] font result: code={} id={}",
                        result.code,
                        result.id
                    );
                    self.font_wait.fulfill(result);
                }
                Some(protocol::watch_face::Payload::InstallResult(result)) => {
                    log::debug!(
                        "Watchface install result: {:?}",
                        serde_json::to_string(&result).unwrap_or_default()
                    );
                }
                Some(protocol::watch_face::Payload::PrepareStatus(status)) => {
                    log::debug!("Watchface prepare status: {}", status);
                }
                _ => {}
            }
        }
    }
}

impl HasOwnerId for WatchfaceSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

#[derive(Component, serde::Serialize)]
pub struct WatchfaceComponent {}

impl WatchfaceComponent {
    pub fn new() -> Self {
        Self {}
    }
}

fn build_watchface_set(watchface_id: &str) -> protocol::WearPacket {
    let payload = protocol::WatchFace {
        payload: Some(protocol::watch_face::Payload::Id(watchface_id.to_string())),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::WatchFace as i32,
        id: protocol::watch_face::WatchFaceId::SetWatchFace as u32,
        payload: Some(protocol::wear_packet::Payload::WatchFace(payload)),
    }
}

fn build_watchface_uninstall(watchface_id: &str) -> protocol::WearPacket {
    let payload = protocol::WatchFace {
        payload: Some(protocol::watch_face::Payload::Id(watchface_id.to_string())),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::WatchFace as i32,
        id: protocol::watch_face::WatchFaceId::RemoveWatchFace as u32,
        payload: Some(protocol::wear_packet::Payload::WatchFace(payload)),
    }
}

fn build_watchface_edit(request: protocol::EditRequest) -> protocol::WearPacket {
    let payload = protocol::WatchFace {
        payload: Some(protocol::watch_face::Payload::EditRequest(request)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::WatchFace as i32,
        id: protocol::watch_face::WatchFaceId::EditWatchFace as u32,
        payload: Some(protocol::wear_packet::Payload::WatchFace(payload)),
    }
}

fn build_watchface_get_support_data() -> protocol::WearPacket {
    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::WatchFace as i32,
        id: protocol::watch_face::WatchFaceId::GetSupportData as u32,
        payload: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_ack_resolves_set_waiter() {
        let mut system = WatchfaceSystem::new(String::new());
        let (mut rx, should_enqueue) = system.set_current_wait.prepare();
        assert!(should_enqueue);

        system.on_pb_packet(WearPacket {
            r#type: protocol::wear_packet::Type::WatchFace as i32,
            id: protocol::watch_face::WatchFaceId::SetWatchFace as u32,
            payload: Some(protocol::wear_packet::Payload::WatchFace(
                protocol::WatchFace {
                    payload: Some(protocol::watch_face::Payload::Success(true)),
                },
            )),
        });

        assert!(matches!(rx.try_recv().unwrap(), Ok(())));
    }

    #[test]
    fn false_success_ack_fails_uninstall_waiter() {
        let mut system = WatchfaceSystem::new(String::new());
        let (mut rx, should_enqueue) = system.uninstall_wait.prepare();
        assert!(should_enqueue);

        system.on_pb_packet(WearPacket {
            r#type: protocol::wear_packet::Type::WatchFace as i32,
            id: protocol::watch_face::WatchFaceId::RemoveWatchFace as u32,
            payload: Some(protocol::wear_packet::Payload::WatchFace(
                protocol::WatchFace {
                    payload: Some(protocol::watch_face::Payload::Success(false)),
                },
            )),
        });

        let err = rx.try_recv().unwrap().unwrap_err();
        assert!(err.to_string().contains("rejected uninstall watchface"));
    }
}
