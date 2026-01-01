use pb::xiaomi::protocol::{self, WearPacket};
use tokio::sync::oneshot;

use crate::{
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::{Component, access::with_device_component_mut},
};

use super::shared::{HasOwnerId, RequestSlot, SystemRequestExt, await_response};
use crate::anyhow_site;

#[derive(Component)]
pub struct ResourceSystem {
    owner_id: String,
    watchface_wait: RequestSlot<Vec<protocol::WatchFaceItem>>,
    quick_app_wait: RequestSlot<Vec<protocol::AppItem>>,
}

impl Default for ResourceSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl ResourceSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            owner_id,
            watchface_wait: RequestSlot::new(),
            quick_app_wait: RequestSlot::new(),
        }
    }

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

    pub fn request_watchface_list(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<Vec<protocol::WatchFaceItem>>> {
        let rx = self.watchface_wait.prepare();
        self.enqueue_request(build_watchface_get_installed());
        rx
    }

    pub fn request_quick_app_list(
        &mut self,
    ) -> oneshot::Receiver<anyhow::Result<Vec<protocol::AppItem>>> {
        let rx = self.quick_app_wait.prepare();
        self.enqueue_request(build_thirdparty_app_get_installed());
        rx
    }

    fn enqueue_request(&mut self, request: protocol::WearPacket) {
        self.enqueue_pb_request(request, "ResourceSystem::enqueue_request");
    }
}

impl HasOwnerId for ResourceSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
}

impl L2PbExt for ResourceSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
        match payload.payload {
            Some(protocol::wear_packet::Payload::WatchFace(watch_face)) => {
                if payload.id != protocol::watch_face::WatchFaceId::GetInstalledList as u32 {
                    return;
                }

                match watch_face.payload {
                    Some(protocol::watch_face::Payload::WatchFaceList(list)) => {
                        let items = list.list.clone();
                        let comp_items = items.clone();
                        let update_res =
                            with_device_component_mut::<ResourceComponent, _, _>(
                                self.owner_id.clone(),
                                move |comp| {
                                    comp.watchfaces = comp_items;
                                },
                            );

                        match update_res {
                            Ok(_) => self.watchface_wait.fulfill(items),
                            Err(err) => {
                                let anyhow_err = anyhow_site!(
                                    "failed to update watchface list in component: {err:?}"
                                );
                                log::error!("{anyhow_err:?}");
                                self.watchface_wait.fail(anyhow_err);
                            }
                        }
                    }
                    unexpected => {
                        let anyhow_err = anyhow_site!(
                            "unexpected watchface payload for installed list: {:?}",
                            unexpected
                        );
                        log::warn!("{anyhow_err:?}");
                        self.watchface_wait.fail(anyhow_err);
                    }
                }
            }
            Some(protocol::wear_packet::Payload::ThirdpartyApp(thirdparty_app)) => {
                if payload.id != protocol::thirdparty_app::ThirdpartyAppId::GetInstalledList as u32
                {
                    return;
                }

                match thirdparty_app.payload {
                    Some(protocol::thirdparty_app::Payload::AppItemList(list)) => {
                        let items = list.list.clone();
                        let comp_items = items.clone();
                        let update_res =
                            with_device_component_mut::<ResourceComponent, _, _>(
                                self.owner_id.clone(),
                                move |comp| {
                                    comp.quick_apps = comp_items;
                                },
                            );

                        match update_res {
                            Ok(_) => self.quick_app_wait.fulfill(items),
                            Err(err) => {
                                let anyhow_err = anyhow_site!(
                                    "failed to update quick app list in component: {err:?}"
                                );
                                log::error!("{anyhow_err:?}");
                                self.quick_app_wait.fail(anyhow_err);
                            }
                        }
                    }
                    unexpected => {
                        let anyhow_err = anyhow_site!(
                            "unexpected third-party app payload for installed list: {:?}",
                            unexpected
                        );
                        log::warn!("{anyhow_err:?}");
                        self.quick_app_wait.fail(anyhow_err);
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct ResourceComponent {
    pub watchfaces: Vec<protocol::WatchFaceItem>,
    pub quick_apps: Vec<protocol::AppItem>,
}

impl ResourceComponent {
    pub fn new() -> Self {
        Self {
            watchfaces: vec![],
            quick_apps: vec![],
        }
    }
}

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
