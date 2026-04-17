use pb::xiaomi::protocol;
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::Component,
};

use super::shared::{HasOwnerId, RequestSlot, SystemRequestExt};

#[derive(Component)]
pub struct ReportSystem {
    owner_id: String,
    device_log_wait: RequestSlot<protocol::report_data::Result>,
}

impl Default for ReportSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl ReportSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            owner_id,
            device_log_wait: RequestSlot::new(),
        }
    }

    pub fn request_device_log_export(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<protocol::report_data::Result>> {
        let (rx, should_enqueue) = self.device_log_wait.prepare();
        if should_enqueue {
            self.enqueue_request(build_report_data_packet(
                protocol::report_data::Type::DeviceLog,
            ));
        }
        rx
    }

    pub fn clear_device_log_wait(&mut self) {
        self.device_log_wait.clear();
    }

    fn enqueue_request(&mut self, request: protocol::WearPacket) {
        self.enqueue_pb_request(request, "ReportSystem::enqueue_request");
    }
}

impl HasOwnerId for ReportSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

impl L2PbExt for ReportSystem {
    fn on_pb_packet(&mut self, payload: protocol::WearPacket) {
        let Some(protocol::wear_packet::Payload::System(system)) = payload.payload else {
            return;
        };
        let Some(protocol::system::Payload::ReportDataResult(result)) = system.payload else {
            return;
        };
        if result.r#type == protocol::report_data::Type::DeviceLog as i32 {
            self.device_log_wait.fulfill(result);
        }
    }
}

fn build_report_data_packet(report_type: protocol::report_data::Type) -> protocol::WearPacket {
    let report_data = protocol::ReportData {
        r#type: report_type as i32,
        id: None,
    };

    let packet_payload = protocol::System {
        payload: Some(protocol::system::Payload::ReportData(report_data)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::System as i32,
        id: protocol::system::SystemId::ReportData as u32,
        payload: Some(protocol::wear_packet::Payload::System(packet_payload)),
    }
}
