use std::collections::HashMap;

use pb::xiaomi::protocol::{self, WearPacket};

use crate::{
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::{
        fastlane::FastLane,
        logic_component::LogicCompMeta,
        system::{SysMeta, System},
    },
    impl_has_sys_meta, impl_logic_component,
};

use super::shared::SystemRequestExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    pub package_name: String,
    pub fingerprint: Vec<u8>,
}

pub struct ThirdpartyAppSystem {
    meta: SysMeta,
    handshake_requested: bool,
}

impl Default for ThirdpartyAppSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
            handshake_requested: false,
        }
    }
}

impl ThirdpartyAppSystem {
    pub fn request_phone_app_status(&mut self) {
        if self.handshake_requested {
            return;
        }
        self.handshake_requested = true;
        self.enqueue_request(build_thirdparty_app_request_phone_status());
    }

    fn enqueue_request(&mut self, packet: protocol::WearPacket) {
        self.enqueue_pb_request(packet, "ThirdpartyAppSystem::enqueue_request");
    }

    fn handle_basic_info(&mut self, basic_info: protocol::BasicInfo) {
        let pk_name = basic_info.package_name.clone();
        let fingerprint = basic_info.fingerprint.clone();
        let info_for_store = AppInfo {
            package_name: pk_name.clone(),
            fingerprint,
        };
        let info_for_sync = basic_info.clone();

        let this: &mut dyn System = self;
        let _ = FastLane::with_component_mut::<ThirdpartyAppComponent, (), _>(
            this,
            ThirdpartyAppComponent::ID,
            move |comp| {
                comp.apps.insert(pk_name, info_for_store);
            },
        );

        self.enqueue_request(build_thirdparty_app_sync_status(
            info_for_sync,
            protocol::phone_app_status::Status::Connected,
        ));
    }

    fn handle_message_content(&mut self, message: protocol::MessageContent) {
        let text = String::from_utf8_lossy(&message.content).to_string();
        log::debug!(
            "Received third-party app message from {}: {}",
            message.basic_info.package_name,
            text
        );

        // TODO: 集成插件系统后，将消息分发给插件。
    }
}

impl L2PbExt for ThirdpartyAppSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
        if let Some(protocol::wear_packet::Payload::ThirdpartyApp(app)) = payload.payload {
            match app.payload {
                Some(protocol::thirdparty_app::Payload::BasicInfo(basic_info)) => {
                    self.handle_basic_info(basic_info);
                }
                Some(protocol::thirdparty_app::Payload::MessageContent(message)) => {
                    self.handle_message_content(message);
                }
                Some(protocol::thirdparty_app::Payload::AppStatus(status)) => {
                    log::debug!(
                        "Wearable reports app status: {} => {:?}",
                        status.basic_info.package_name,
                        serde_json::to_string(&status).unwrap()
                    );
                }
                _ => {}
            }
        }
    }
}

impl_has_sys_meta!(ThirdpartyAppSystem, meta);

pub struct ThirdpartyAppComponent {
    meta: LogicCompMeta,
    pub apps: HashMap<String, AppInfo>,
}

impl ThirdpartyAppComponent {
    pub const ID: &'static str = "MiWearDeviceThirdpartyLogicComponent";

    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<ThirdpartyAppSystem>(Self::ID),
            apps: HashMap::new(),
        }
    }
}

impl_logic_component!(ThirdpartyAppComponent, meta);

fn build_thirdparty_app_sync_status(
    basic_info: protocol::BasicInfo,
    status: protocol::phone_app_status::Status,
) -> protocol::WearPacket {
    let phone_status = protocol::PhoneAppStatus {
        basic_info,
        status: status as i32,
    };

    let payload = protocol::ThirdpartyApp {
        payload: Some(protocol::thirdparty_app::Payload::AppStatus(phone_status)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::SyncPhoneAppStatus as u32,
        payload: Some(protocol::wear_packet::Payload::ThirdpartyApp(payload)),
    }
}

fn build_thirdparty_app_request_phone_status() -> protocol::WearPacket {
    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::RequestPhoneAppStatus as u32,
        payload: None,
    }
}
