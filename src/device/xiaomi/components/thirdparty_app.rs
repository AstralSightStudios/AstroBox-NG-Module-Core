use pb::xiaomi::protocol::{self, WearPacket};

use crate::{
    device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet},
    ecs::{logic_component::LogicCompMeta, system::SysMeta},
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
}

impl Default for ThirdpartyAppSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
        }
    }
}

impl ThirdpartyAppSystem {
    pub fn send_phone_message(&mut self, app: &AppInfo, payload: Vec<u8>) {
        let packet = build_thirdparty_app_msg_content(app, payload);
        self.enqueue_request(packet);
    }

    pub fn launch_app(&mut self, app: &AppInfo, page: &str) {
        let packet = build_thirdparty_app_launch(app, page);
        self.enqueue_request(packet);
    }

    pub fn uninstall_app(&mut self, app: &AppInfo) {
        let packet = build_thirdparty_app_uninstall(app);
        self.enqueue_request(packet);
    }

    pub fn sync_status(&mut self, app: &AppInfo, status: protocol::phone_app_status::Status) {
        let packet = build_thirdparty_app_sync_status(to_basic_info(app), status);
        self.enqueue_request(packet);
    }

    fn enqueue_request(&mut self, packet: protocol::WearPacket) {
        self.enqueue_pb_request(packet, "ThirdpartyAppSystem::enqueue_request");
    }

    fn handle_basic_info(&mut self, basic_info: protocol::BasicInfo) {
        let info_for_sync = basic_info.clone();

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
}

impl ThirdpartyAppComponent {
    pub const ID: &'static str = "MiWearDeviceThirdpartyAppLogicComponent";

    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<ThirdpartyAppSystem>(Self::ID),
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

fn build_thirdparty_app_msg_content(app: &AppInfo, data: Vec<u8>) -> protocol::WearPacket {
    let message_content = protocol::MessageContent {
        basic_info: to_basic_info(app),
        content: data,
    };

    let payload = protocol::ThirdpartyApp {
        payload: Some(protocol::thirdparty_app::Payload::MessageContent(
            message_content,
        )),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::SendPhoneMessage as u32,
        payload: Some(protocol::wear_packet::Payload::ThirdpartyApp(payload)),
    }
}

fn build_thirdparty_app_launch(app: &AppInfo, page: &str) -> protocol::WearPacket {
    let launch_info = protocol::LaunchInfo {
        basic_info: to_basic_info(app),
        uri: page.to_string(),
    };

    let payload = protocol::ThirdpartyApp {
        payload: Some(protocol::thirdparty_app::Payload::LaunchInfo(launch_info)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::LaunchApp as u32,
        payload: Some(protocol::wear_packet::Payload::ThirdpartyApp(payload)),
    }
}

fn build_thirdparty_app_uninstall(app: &AppInfo) -> protocol::WearPacket {
    let payload = protocol::ThirdpartyApp {
        payload: Some(protocol::thirdparty_app::Payload::BasicInfo(to_basic_info(
            app,
        ))),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::RemoveApp as u32,
        payload: Some(protocol::wear_packet::Payload::ThirdpartyApp(payload)),
    }
}

fn to_basic_info(app: &AppInfo) -> protocol::BasicInfo {
    protocol::BasicInfo {
        package_name: app.package_name.clone(),
        fingerprint: app.fingerprint.clone(),
    }
}
