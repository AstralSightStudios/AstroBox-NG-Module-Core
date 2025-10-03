use pb::xiaomi::protocol::{self, WearPacket};
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

pub struct ResourceSystem {
    meta: SysMeta,
    watchface_wait: RequestSlot<Vec<protocol::WatchFaceItem>>,
    quick_app_wait: RequestSlot<Vec<protocol::AppItem>>,
}

impl Default for ResourceSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
            watchface_wait: RequestSlot::new(),
            quick_app_wait: RequestSlot::new(),
        }
    }
}

impl ResourceSystem {
    pub async fn get_installed_watchfaces(
        &mut self,
    ) -> anyhow::Result<Vec<protocol::WatchFaceItem>> {
        await_response(
            self.request_watchface_list(),
            "Watchface list response not received",
        )
        .await
    }

    pub async fn get_installed_quick_apps(&mut self) -> anyhow::Result<Vec<protocol::AppItem>> {
        await_response(
            self.request_quick_app_list(),
            "Quick app list response not received",
        )
        .await
    }

    pub fn request_watchface_list(&mut self) -> oneshot::Receiver<Vec<protocol::WatchFaceItem>> {
        let rx = self.watchface_wait.prepare();
        self.enqueue_request(build_watchface_get_installed());
        rx
    }

    pub fn request_quick_app_list(&mut self) -> oneshot::Receiver<Vec<protocol::AppItem>> {
        let rx = self.quick_app_wait.prepare();
        self.enqueue_request(build_thirdparty_app_get_installed());
        rx
    }

    fn enqueue_request(&mut self, request: protocol::WearPacket) {
        self.enqueue_pb_request(request, "ResourceSystem::enqueue_request");
    }
}

impl L2PbExt for ResourceSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
        match payload.payload {
            Some(protocol::wear_packet::Payload::WatchFace(watch_face)) => {
                if payload.id != protocol::watch_face::WatchFaceId::GetInstalledList as u32 {
                    return;
                }

                if let Some(protocol::watch_face::Payload::WatchFaceList(list)) = watch_face.payload
                {
                    let items = list.list.clone();
                    let comp_items = items.clone();
                    let this: &mut dyn System = self;
                    FastLane::with_component_mut::<ResourceComponent, (), _>(
                        this,
                        ResourceComponent::ID,
                        move |comp| {
                            comp.watchfaces = comp_items;
                        },
                    )
                    .unwrap();

                    self.watchface_wait.fulfill(items);
                }
            }
            Some(protocol::wear_packet::Payload::ThirdpartyApp(thirdparty_app)) => {
                if payload.id != protocol::thirdparty_app::ThirdpartyAppId::GetInstalledList as u32
                {
                    return;
                }

                if let Some(protocol::thirdparty_app::Payload::AppItemList(list)) =
                    thirdparty_app.payload
                {
                    let items = list.list.clone();
                    let comp_items = items.clone();
                    let this: &mut dyn System = self;
                    FastLane::with_component_mut::<ResourceComponent, (), _>(
                        this,
                        ResourceComponent::ID,
                        move |comp| {
                            comp.quick_apps = comp_items;
                        },
                    )
                    .unwrap();

                    self.quick_app_wait.fulfill(items);
                }
            }
            _ => {}
        }
    }
}

impl_has_sys_meta!(ResourceSystem, meta);

pub struct ResourceComponent {
    meta: LogicCompMeta,
    pub watchfaces: Vec<protocol::WatchFaceItem>,
    pub quick_apps: Vec<protocol::AppItem>,
}

impl ResourceComponent {
    pub const ID: &'static str = "MiWearDeviceResourceLogicComponent";

    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<ResourceSystem>(Self::ID),
            watchfaces: vec![],
            quick_apps: vec![],
        }
    }
}

impl_logic_component!(ResourceComponent, meta);

fn build_watchface_get_installed() -> protocol::WearPacket {
    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::WatchFace as i32,
        id: protocol::watch_face::WatchFaceId::GetInstalledList as u32,
        payload: None,
    }
}

fn build_thirdparty_app_get_installed() -> protocol::WearPacket {
    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::GetInstalledList as u32,
        payload: None,
    }
}
