use pb::xiaomi::protocol::{self, WearPacket, wear_packet};

use crate::{
    device::xiaomi::{
        components::shared::SystemRequestExt,
        system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    },
    ecs::{logic_component::LogicCompMeta, system::SysMeta},
    impl_has_sys_meta, impl_logic_component,
    models::sync::TimeSyncProps,
};

pub struct SyncSystem {
    meta: SysMeta,
}

impl Default for SyncSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
        }
    }
}

impl SyncSystem {
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

impl_has_sys_meta!(SyncSystem, meta);

#[derive(serde::Serialize)]
pub struct SyncComponent {
    #[serde(skip_serializing)]
    meta: LogicCompMeta,
}

impl SyncComponent {
    pub const ID: &'static str = "MiWearDeviceSyncLogicComponent";

    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<SyncSystem>(Self::ID),
        }
    }
}

impl_logic_component!(SyncComponent, meta);

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
