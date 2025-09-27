use pb::pb::protocol::{self, WearPacket};

use crate::device::xiaomi::config::ResConfig;
use crate::device::xiaomi::packet::{self, mass::MassDataType, v2::layer2::L2Packet};
use crate::device::xiaomi::{resutils, XiaomiDevice};
use crate::ecs::fastlane::FastLane;
use crate::ecs::logic_component::LogicCompMeta;
use crate::ecs::system::{SysMeta, System};
use crate::impl_has_sys_meta;
use crate::impl_logic_component;

use crate::device::xiaomi::components::mass::{send_file_for_owner, SendMassCallbackData};
use crate::device::xiaomi::system::{register_xiaomi_system_ext_on_l2packet, L2PbExt};
use std::sync::Arc;

pub struct InstallSystem {
    meta: SysMeta,
}

impl Default for InstallSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
        }
    }
}

impl InstallSystem {
    pub fn send_install_request(
        &mut self,
        r#type: MassDataType,
        file_data: Vec<u8>,
        package_name: Option<&str>,
    ) {
        self.send_install_request_with_progress(r#type, file_data, package_name, Arc::new(|_| {}));
    }

    pub fn send_install_request_with_progress(
        &mut self,
        r#type: MassDataType,
        file_data: Vec<u8>,
        package_name: Option<&str>,
        progress_cb: Arc<dyn Fn(SendMassCallbackData) + Send + Sync>,
    ) {
        let this: &mut dyn System = self;

        let file_data_cl = file_data.clone();
        let cb_arc = progress_cb.clone();
        FastLane::with_component_mut::<InstallComponent, (), _>(
            this,
            InstallComponent::ID,
            move |comp| {
                comp.install_data = Some(file_data_cl);
                comp.progress_cb = Some(cb_arc);
            },
        )
        .unwrap();

        let res_config = FastLane::with_entity_mut::<ResConfig, _>(this, |ent| {
            let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
            Ok(dev.config.res.clone())
        })
        .unwrap();

        let req: WearPacket = match r#type {
            MassDataType::WATCHFACE => {
                let id = resutils::get_watchface_id(&file_data, &res_config)
                    .expect("invalid watchface id");
                build_watchface_install_request(&id, file_data.len())
            }
            MassDataType::FIRMWARE => build_firmware_install_request(
                "99.99.99".to_string(),
                &crate::tools::calc_md5(&file_data),
                "AstroBox Firmware Update".to_string(),
            ),
            MassDataType::NotificationIcon => {
                build_notification_icon_request(package_name.expect("package_name is required"))
            }
            MassDataType::ThirdpartyApp => build_thirdparty_app_install_request(
                package_name.expect("package_name is required"),
                114514,
                file_data.len(),
            ),
        };

        FastLane::with_entity_mut::<(), _>(this, move |ent| {
            let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
            let bytes = match packet::ensure_l2_cipher_blocking(&dev.name, dev.sar_version) {
                Some(cipher) => match L2Packet::pb_write_enc(req.clone(), cipher.as_ref()) {
                    Ok(pkt) => pkt.to_bytes(),
                    Err(err) => {
                        log::error!("pb_write_enc failed, fallback to plain write: {:?}", err);
                        L2Packet::pb_write(req).to_bytes()
                    }
                },
                None => L2Packet::pb_write(req).to_bytes(),
            };
            dev.sar.enqueue(bytes);
            Ok(())
        })
        .unwrap();
    }

    #[inline]
    fn on_prepare_ready(&mut self, r#type: MassDataType) {
        let this: &mut dyn System = self;
        let owner = this.owner().unwrap_or("").to_string();

        let (file_data_opt, cb_opt) = FastLane::with_component_mut::<
            InstallComponent,
            (
                Option<Vec<u8>>,
                Option<Arc<dyn Fn(SendMassCallbackData) + Send + Sync>>,
            ),
            _,
        >(this, InstallComponent::ID, move |comp| {
            (comp.install_data.take(), comp.progress_cb.take())
        })
        .unwrap();

        if let Some(file_data) = file_data_opt {
            let owner_id = owner.clone();
            let progress_cb = cb_opt.unwrap_or_else(|| Arc::new(|_| {}));
            let runtime_handle =
                FastLane::with_entity_mut::<tokio::runtime::Handle, _>(this, move |ent| {
                    let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
                    Ok(dev.sar.runtime_handle())
                })
                .ok();

            if let Some(handle) = runtime_handle {
                crate::asyncrt::spawn_with_handle(
                    {
                        let cb_arc = progress_cb.clone();
                        let owner_for_send = owner_id.clone();
                        async move {
                            log::info!("Starting install resource...");
                            if let Err(err) =
                                send_file_for_owner(owner_for_send, file_data, r#type, move |d| {
                                    (cb_arc)(d)
                                })
                                .await
                            {
                                log::error!("Failed to send MASS payload: {:?}", err);
                            }
                        }
                    },
                    handle,
                );
            } else {
                log::error!(
                    "InstallSystem.on_prepare_ready: missing runtime handle for owner {}",
                    owner
                );
            }
        } else {
            log::warn!("InstallSystem.on_prepare_ready called but no install_data was set");
        }
    }
}

impl L2PbExt for InstallSystem {
    fn on_pb_packet(&mut self, payload: protocol::WearPacket) {
        if let Some(next_type) = match payload.payload {
            Some(protocol::wear_packet::Payload::WatchFace(wf)) => match wf.payload {
                Some(protocol::watch_face::Payload::PrepareStatus(status))
                    if status == protocol::PrepareStatus::Ready as i32 =>
                {
                    Some(MassDataType::WATCHFACE)
                }
                _ => None,
            },
            Some(protocol::wear_packet::Payload::ThirdpartyApp(ta)) => match ta.payload {
                Some(protocol::thirdparty_app::Payload::InstallResponse(resp))
                    if resp.prepare_status == protocol::PrepareStatus::Ready as i32 =>
                {
                    Some(MassDataType::ThirdpartyApp)
                }
                _ => None,
            },
            Some(protocol::wear_packet::Payload::System(sys)) => match sys.payload {
                Some(protocol::system::Payload::PrepareOtaResponse(resp))
                    if resp.prepare_status == protocol::PrepareStatus::Ready as i32 =>
                {
                    Some(MassDataType::FIRMWARE)
                }
                _ => None,
            },
            Some(protocol::wear_packet::Payload::Notification(nc)) => match nc.payload {
                Some(protocol::notification::Payload::AppIconResponse(resp))
                    if resp.prepare_status == protocol::PrepareStatus::Ready as i32 =>
                {
                    Some(MassDataType::NotificationIcon)
                }
                _ => None,
            },
            _ => None,
        } {
            self.on_prepare_ready(next_type);
        }
    }
}

impl_has_sys_meta!(InstallSystem, meta);

pub struct InstallComponent {
    meta: LogicCompMeta,
    install_data: Option<Vec<u8>>,
    progress_cb: Option<Arc<dyn Fn(SendMassCallbackData) + Send + Sync>>,
}

impl InstallComponent {
    pub const ID: &'static str = "MiWearDeviceInstallLogicComponent";
    pub fn new() -> Self {
        Self {
            meta: LogicCompMeta::new::<InstallSystem>(Self::ID),
            install_data: None,
            progress_cb: None,
        }
    }
}

impl_logic_component!(InstallComponent, meta);

pub fn build_watchface_install_request(id: &str, package_size: usize) -> protocol::WearPacket {
    let prepare_info = protocol::PrepareInfo {
        id: id.to_string(),
        size: package_size as u32,
        version_code: Some(65536),
        support_compress_mode: None,
        verification: None,
    };

    let pkt_payload = protocol::WatchFace {
        payload: Some(protocol::watch_face::Payload::PrepareInfo(prepare_info)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::WatchFace as i32,
        id: protocol::watch_face::WatchFaceId::PrepareInstallWatchFace as u32,
        payload: Some(protocol::wear_packet::Payload::WatchFace(pkt_payload)),
    }
}

pub fn build_thirdparty_app_install_request(
    package_name: &str,
    version_code: u32,
    package_size: usize,
) -> protocol::WearPacket {
    let install_req = protocol::app_installer::Request {
        package_name: package_name.to_string(),
        version_code,
        package_size: package_size as u32,
    };

    let pkt_payload = protocol::ThirdpartyApp {
        payload: Some(protocol::thirdparty_app::Payload::InstallRequest(
            install_req,
        )),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::ThirdpartyApp as i32,
        id: protocol::thirdparty_app::ThirdpartyAppId::PrepareInstallApp as u32,
        payload: Some(protocol::wear_packet::Payload::ThirdpartyApp(pkt_payload)),
    }
}

pub fn build_firmware_install_request(
    firmware_version: String,
    file_md5: &Vec<u8>,
    change_log: String,
) -> protocol::WearPacket {
    let install_req = protocol::prepare_ota::Request {
        force: true,
        r#type: protocol::prepare_ota::Type::All as i32,
        firmware_version,
        file_md5: crate::tools::to_hex_string(file_md5),
        change_log,
        file_url: "".to_owned(),
        file_size: None,
    };

    let pkt_payload = protocol::System {
        payload: Some(protocol::system::Payload::PrepareOtaRequest(install_req)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::System as i32,
        id: protocol::system::SystemId::PrepareOta as u32,
        payload: Some(protocol::wear_packet::Payload::System(pkt_payload)),
    }
}

pub fn build_notification_icon_request(package_name: &str) -> protocol::WearPacket {
    let nc_req = protocol::prepare_app_icon::Request {
        package_name: package_name.to_string(),
        support_compress_mode: None,
    };

    let pkt_payload = protocol::Notification {
        payload: Some(protocol::notification::Payload::AppIconRequest(nc_req)),
    };

    protocol::WearPacket {
        r#type: protocol::wear_packet::Type::Notification as i32,
        id: protocol::notification::NotificationId::PrepareAppIcon as u32,
        payload: Some(protocol::wear_packet::Payload::Notification(pkt_payload)),
    }
}
