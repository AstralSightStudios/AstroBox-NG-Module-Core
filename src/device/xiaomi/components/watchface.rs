use pb::xiaomi::protocol::{self, WearPacket};

use crate::{
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::{logic_component::LogicCompMeta, system::SysMeta},
    impl_has_sys_meta, impl_logic_component,
};

use super::shared::SystemRequestExt;

pub struct WatchfaceSystem {
    meta: SysMeta,
}

impl Default for WatchfaceSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
        }
    }
}

impl WatchfaceSystem {
    pub fn set_watchface(&mut self, watchface_id: &str) {
        let packet = build_watchface_set(watchface_id);
        self.enqueue_request(packet);
    }

    pub fn uninstall_watchface(&mut self, watchface_id: &str) {
        let packet = build_watchface_uninstall(watchface_id);
        self.enqueue_request(packet);
    }

    fn enqueue_request(&mut self, packet: protocol::WearPacket) {
        self.enqueue_pb_request(packet, "WatchfaceSystem::enqueue_request");
    }
}

impl L2PbExt for WatchfaceSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
        if let Some(protocol::wear_packet::Payload::WatchFace(msg)) = payload.payload {
            match msg.payload {
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

impl_has_sys_meta!(WatchfaceSystem, meta);

#[derive(serde::Serialize)]
pub struct WatchfaceComponent {
    #[serde(skip_serializing)]
    meta: LogicCompMeta,
}

impl WatchfaceComponent {
    pub const ID: &'static str = "MiWearDeviceWatchfaceLogicComponent";

    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<WatchfaceSystem>(Self::ID),
        }
    }
}

impl_logic_component!(WatchfaceComponent, meta);

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
