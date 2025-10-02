use anyhow::Context;
use pb::xiaomi::protocol::{self, device_status::Battery, DeviceInfo, DeviceStatus};
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::{
        packet::{self, v2::layer2::L2Packet},
        system::{register_xiaomi_system_ext_on_l2packet, L2PbExt},
        XiaomiDevice,
    },
    ecs::{
        fastlane::FastLane,
        logic_component::LogicCompMeta,
        system::{SysMeta, System},
    },
    impl_has_sys_meta, impl_logic_component,
};

pub struct InfoSystem {
    meta: SysMeta,
    device_info_wait: Option<oneshot::Sender<protocol::DeviceInfo>>,
    device_status_wait: Option<oneshot::Sender<protocol::DeviceStatus>>,
    device_storage_wait: Option<oneshot::Sender<protocol::StorageInfo>>,
}

impl Default for InfoSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
            device_info_wait: None,
            device_status_wait: None,
            device_storage_wait: None,
        }
    }
}

impl InfoSystem {
    pub async fn get_device_info(&mut self) -> anyhow::Result<DeviceInfo> {
        Self::await_response(
            self.request_device_info(),
            "Device info response not received",
        )
        .await
    }

    pub async fn get_device_status(&mut self) -> anyhow::Result<DeviceStatus> {
        Self::await_response(
            self.request_device_status(),
            "Device status response not received",
        )
        .await
    }

    pub async fn get_device_storage_info(&mut self) -> anyhow::Result<protocol::StorageInfo> {
        Self::await_response(
            self.request_device_storage(),
            "Device storage info response not received",
        )
        .await
    }

    pub fn request_device_info(&mut self) -> oneshot::Receiver<DeviceInfo> {
        self.request_receiver(
            |this, tx| this.device_info_wait = Some(tx),
            || Self::build_system_packet(protocol::system::SystemId::GetDeviceInfo),
        )
    }

    pub fn request_device_status(&mut self) -> oneshot::Receiver<DeviceStatus> {
        self.request_receiver(
            |this, tx| this.device_status_wait = Some(tx),
            || Self::build_system_packet(protocol::system::SystemId::GetDeviceStatus),
        )
    }

    pub fn request_device_storage(&mut self) -> oneshot::Receiver<protocol::StorageInfo> {
        self.request_receiver(
            |this, tx| this.device_storage_wait = Some(tx),
            || Self::build_system_packet(protocol::system::SystemId::GetStorageInfo),
        )
    }

    fn enqueue_request(&mut self, request: protocol::WearPacket) {
        let this: &mut dyn System = self;

        FastLane::with_entity_mut::<(), _>(this, move |ent| {
            let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
            let cipher = packet::ensure_l2_cipher_blocking(&dev.addr, dev.sar_version).unwrap();

            dev.sar.enqueue(
                L2Packet::pb_write_enc(request, cipher.as_ref())
                    .unwrap()
                    .to_bytes(),
            );

            Ok(())
        })
        .unwrap();
    }

    fn request_receiver<T, Store, Build>(
        &mut self,
        store_waiter: Store,
        build_packet: Build,
    ) -> oneshot::Receiver<T>
    where
        T: Send + 'static,
        Store: FnOnce(&mut Self, oneshot::Sender<T>),
        Build: FnOnce() -> protocol::WearPacket,
    {
        let (tx, rx) = oneshot::channel();
        store_waiter(self, tx);
        self.enqueue_request(build_packet());
        rx
    }

    async fn await_response<T>(rx: oneshot::Receiver<T>, err_ctx: &'static str) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        let resp = rx.await.context(err_ctx)?;
        Ok(resp)
    }

    fn build_system_packet(id: protocol::system::SystemId) -> protocol::WearPacket {
        protocol::WearPacket {
            r#type: protocol::wear_packet::Type::System as i32,
            id: id as u32,
            payload: None,
        }
    }

    fn fulfill_waiter<T>(waiter: &mut Option<oneshot::Sender<T>>, value: T) {
        if let Some(tx) = waiter.take() {
            let _ = tx.send(value);
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
                        let dev_info_cl = dev_info.clone();
                        FastLane::with_component_mut::<InfoComponent, (), _>(
                            this,
                            InfoComponent::ID,
                            move |comp| {
                                comp.model = dev_info.model;
                                comp.sn = dev_info.serial_number;
                            },
                        )
                        .unwrap();

                        Self::fulfill_waiter(&mut self.device_info_wait, dev_info_cl)
                    }
                    pb::xiaomi::protocol::system::Payload::DeviceStatus(dev_status) => {
                        let dev_status_cl = dev_status.clone();
                        FastLane::with_component_mut::<InfoComponent, (), _>(
                            this,
                            InfoComponent::ID,
                            move |comp| {
                                comp.battery = Some(dev_status.battery);
                            },
                        )
                        .unwrap();

                        Self::fulfill_waiter(&mut self.device_status_wait, dev_status_cl)
                    }
                    pb::xiaomi::protocol::system::Payload::StorageInfo(storage) => {
                        let dev_storage_cl = storage.clone();
                        FastLane::with_component_mut::<InfoComponent, (), _>(
                            this,
                            InfoComponent::ID,
                            move |comp| {
                                comp.storage.free = storage.total - storage.used;
                                comp.storage.total = storage.total;
                            },
                        )
                        .unwrap();

                        Self::fulfill_waiter(&mut self.device_storage_wait, dev_storage_cl)
                    }
                    _ => {}
                }
            }
        }
    }
}

impl_has_sys_meta!(InfoSystem, meta);

pub struct StorageInfo {
    pub total: u64,
    pub free: u64,
}

pub struct InfoComponent {
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
