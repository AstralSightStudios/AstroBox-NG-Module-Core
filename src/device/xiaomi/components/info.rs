use anyhow::anyhow;
use pb::xiaomi::protocol::{self, DeviceInfo, DeviceStatus, device_status::Battery};
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::{
        fastlane::FastLane,
        logic_component::LogicCompMeta,
        system::{SysMeta, System},
    },
    impl_has_sys_meta, impl_logic_component,
};

use super::shared::{RequestSlot, SystemRequestExt, await_response};

pub struct InfoSystem {
    meta: SysMeta,
    device_info_wait: RequestSlot<protocol::DeviceInfo>,
    device_status_wait: RequestSlot<protocol::DeviceStatus>,
    device_storage_wait: RequestSlot<protocol::StorageInfo>,
}

impl Default for InfoSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
            device_info_wait: RequestSlot::new(),
            device_status_wait: RequestSlot::new(),
            device_storage_wait: RequestSlot::new(),
        }
    }
}

impl InfoSystem {
    pub async fn get_device_info(&mut self) -> anyhow::Result<DeviceInfo> {
        await_response(
            self.request_device_info(),
            "Device info response not received",
        )
        .await
    }

    pub async fn get_device_status(&mut self) -> anyhow::Result<DeviceStatus> {
        await_response(
            self.request_device_status(),
            "Device status response not received",
        )
        .await
    }

    pub async fn get_device_storage_info(&mut self) -> anyhow::Result<protocol::StorageInfo> {
        await_response(
            self.request_device_storage(),
            "Device storage info response not received",
        )
        .await
    }

    pub fn request_device_info(&mut self) -> oneshot::Receiver<anyhow::Result<DeviceInfo>> {
        let rx = self.device_info_wait.prepare();
        self.enqueue_request(Self::build_system_packet(
            protocol::system::SystemId::GetDeviceInfo,
        ));
        rx
    }

    pub fn request_device_status(&mut self) -> oneshot::Receiver<anyhow::Result<DeviceStatus>> {
        let rx = self.device_status_wait.prepare();
        self.enqueue_request(Self::build_system_packet(
            protocol::system::SystemId::GetDeviceStatus,
        ));
        rx
    }

    pub fn request_device_storage(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<protocol::StorageInfo>> {
        let rx = self.device_storage_wait.prepare();
        self.enqueue_request(Self::build_system_packet(
            protocol::system::SystemId::GetStorageInfo,
        ));
        rx
    }

    fn enqueue_request(&mut self, request: protocol::WearPacket) {
        self.enqueue_pb_request(request, "InfoSystem::enqueue_request");
    }

    fn build_system_packet(id: protocol::system::SystemId) -> protocol::WearPacket {
        protocol::WearPacket {
            r#type: protocol::wear_packet::Type::System as i32,
            id: id as u32,
            payload: None,
        }
    }
}

impl L2PbExt for InfoSystem {
    fn on_pb_packet(&mut self, payload: pb::xiaomi::protocol::WearPacket) {
        let this: &mut dyn System = self;

        if let Some(pb::xiaomi::protocol::wear_packet::Payload::System(sys)) = payload.payload {
            if let Some(sys_payload) = sys.payload {
                match sys_payload {
                    pb::xiaomi::protocol::system::Payload::DeviceInfo(dev_info) => {
                        let dev_info_for_slot = dev_info.clone();
                        let model = dev_info.model.clone();
                        let serial_number = dev_info.serial_number.clone();
                        let update_res = FastLane::with_component_mut::<InfoComponent, (), _>(
                            this,
                            InfoComponent::ID,
                            move |comp| {
                                comp.model = model;
                                comp.sn = serial_number;
                            },
                        );

                        match update_res {
                            Ok(_) => self.device_info_wait.fulfill(dev_info_for_slot),
                            Err(err) => {
                                let anyhow_err = anyhow!(
                                    "failed to update info component with device info: {err:?}"
                                );
                                log::error!("{anyhow_err:?}");
                                self.device_info_wait.fail(anyhow_err);
                            }
                        }
                    }
                    pb::xiaomi::protocol::system::Payload::DeviceStatus(dev_status) => {
                        let dev_status_for_slot = dev_status.clone();
                        let battery = dev_status.battery;
                        let update_res = FastLane::with_component_mut::<InfoComponent, (), _>(
                            this,
                            InfoComponent::ID,
                            move |comp| {
                                comp.battery = Some(battery);
                            },
                        );

                        match update_res {
                            Ok(_) => self.device_status_wait.fulfill(dev_status_for_slot),
                            Err(err) => {
                                let anyhow_err = anyhow!(
                                    "failed to update info component with device status: {err:?}"
                                );
                                log::error!("{anyhow_err:?}");
                                self.device_status_wait.fail(anyhow_err);
                            }
                        }
                    }
                    pb::xiaomi::protocol::system::Payload::StorageInfo(storage) => {
                        let storage_for_slot = storage.clone();
                        let total = storage.total;
                        let used = storage.used;
                        let update_res = FastLane::with_component_mut::<InfoComponent, (), _>(
                            this,
                            InfoComponent::ID,
                            move |comp| {
                                comp.storage.free = total - used;
                                comp.storage.total = total;
                            },
                        );

                        match update_res {
                            Ok(_) => self.device_storage_wait.fulfill(storage_for_slot),
                            Err(err) => {
                                let anyhow_err = anyhow!(
                                    "failed to update info component with storage info: {err:?}"
                                );
                                log::error!("{anyhow_err:?}");
                                self.device_storage_wait.fail(anyhow_err);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl_has_sys_meta!(InfoSystem, meta);

#[derive(serde::Serialize)]
pub struct StorageInfo {
    pub total: u64,
    pub free: u64,
}

#[derive(serde::Serialize)]
pub struct InfoComponent {
    #[serde(skip_serializing)]
    meta: LogicCompMeta,
    //codename: String,
    model: String,
    sn: String,
    battery: Option<Battery>,
    storage: StorageInfo,
}

impl InfoComponent {
    pub const ID: &'static str = "MiWearDeviceInfoLogicComponent";
    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<InfoSystem>(Self::ID),
            //codename: "".to_string(),
            model: "".to_string(),
            sn: "".to_string(),
            battery: None,
            storage: StorageInfo { total: 0, free: 0 },
        }
    }
}

impl_logic_component!(InfoComponent, meta);
