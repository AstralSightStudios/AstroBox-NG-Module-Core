use pb::xiaomi::protocol::{self, WearPacket, wear_packet};

use crate::{
    device::xiaomi::{
        components::shared::{HasOwnerId, SystemRequestExt},
        system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    },
    ecs::Component,
    models::sync::TimeSyncProps,
};

#[derive(Component)]
pub struct SyncSystem {
    owner_id: String,
}

impl Default for SyncSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl SyncSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self { owner_id }
    }

    pub fn sync_time(&mut self, props: TimeSyncProps) {
        log::info!(
            "Syncing time with props: {}",
            serde_json::to_string(&props).unwrap_or_default()
        );
        self.enqueue_pb_request(build_time_sync_packet(props), "SyncSystem::SyncTime");
    }

    pub fn set_language(&mut self, locale: String) {
        self.enqueue_pb_request(build_set_language_packet(locale), "SyncSystem::SetLanguage");
    }
}

impl L2PbExt for SyncSystem {
    fn on_pb_packet(&mut self, _payload: WearPacket) {}
}

impl HasOwnerId for SyncSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

#[derive(Component, serde::Serialize)]
pub struct SyncComponent {
}

impl SyncComponent {
    pub fn new() -> Self {
        Self {}
    }
}

fn build_set_language_packet(lang: String) -> WearPacket {
    let payload = protocol::Language { locale: lang };

    let pkt_payload = protocol::System {
        payload: Some(protocol::system::Payload::Language(payload)),
    };

    let pkt = WearPacket {
        r#type: wear_packet::Type::System as i32,
        id: protocol::system::SystemId::SetLanguage as u32,
        payload: Some(wear_packet::Payload::System(pkt_payload)),
    };

    pkt
}

fn build_time_sync_packet(props: TimeSyncProps) -> WearPacket {
    let payload = protocol::SystemTime {
        date: protocol::Date {
            year: props.date.year,
            month: props.date.month,
            day: props.date.day,
        },
        time: protocol::Time {
            hour: props.time.hour,
            minuter: props.time.minute,
            second: Some(props.time.second),
            millisecond: Some(props.time.millisecond),
        },
        time_zone: Some(protocol::Timezone {
            offset: props.timezone.offset,
            dst_saving: Some(props.timezone.dst_offset),
            id: props.timezone.id,
            id_spec: "".to_string(),
        }),
        is_12_hours: Some(props.is_12_hour_format),
    };

    let pkt_payload = protocol::System {
        payload: Some(protocol::system::Payload::SystemTime(payload)),
    };

    let pkt = WearPacket {
        r#type: wear_packet::Type::System as i32,
        id: protocol::system::SystemId::SetSystemTime as u32,
        payload: Some(wear_packet::Payload::System(pkt_payload)),
    };

    pkt
}
