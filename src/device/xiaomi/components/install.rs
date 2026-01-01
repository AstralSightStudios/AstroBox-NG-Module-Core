use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::{anyhow_site, bail_site};
use anyhow::{Context, Result};
use pb::xiaomi::protocol::{self, WearPacket};
use tokio::sync::oneshot;

use crate::asyncrt::universal_block_on;
use crate::device::xiaomi::components::{
    mass::{SendMassCallbackData, send_file_for_owner},
    resource::ResourceSystem,
};
use crate::device::xiaomi::config::ResConfig;
use crate::device::xiaomi::packet::{self, mass::MassDataType};
use crate::device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet};
use crate::device::xiaomi::{XiaomiDevice, resutils};
use crate::ecs::{Component, access::with_device_component_mut};
use parking_lot::Mutex;

#[cfg(target_arch = "wasm32")]
type InstallFuture = Pin<Box<dyn Future<Output = Result<()>>>>;

#[cfg(not(target_arch = "wasm32"))]
type InstallFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

#[derive(Component)]
pub struct InstallSystem {
    owner_id: String,
}

struct InstallWaiters {
    data_type: MassDataType,
    prepare_tx: Option<oneshot::Sender<i32>>,
    result_tx: Option<oneshot::Sender<InstallResultEvent>>,
}

enum InstallResultEvent {
    ThirdpartyApp(protocol::app_installer::Result),
    Watchface(protocol::InstallResult),
    Firmware(protocol::prepare_ota::Response),
}

impl Default for InstallSystem {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl InstallSystem {
    pub fn new(owner_id: String) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self { owner_id }
    }

    pub fn send_install_request(
        &mut self,
        r#type: MassDataType,
        file_data: Vec<u8>,
        package_name: Option<&str>,
    ) -> Result<InstallFuture> {
        self.send_install_request_with_progress(r#type, file_data, package_name, Arc::new(|_| {}))
    }

    pub fn send_install_request_with_progress(
        &mut self,
        r#type: MassDataType,
        file_data: Vec<u8>,
        package_name: Option<&str>,
        progress_cb: Arc<dyn Fn(SendMassCallbackData) + Send + Sync>,
    ) -> Result<InstallFuture> {
        let owner = self.owner_id.clone();

        let (prepare_tx, prepare_rx) = oneshot::channel::<i32>();
        let (result_tx_opt, result_rx_opt) = match r#type {
            MassDataType::NotificationIcon => (None, None),
            _ => {
                let (tx, rx) = oneshot::channel();
                (Some(tx), Some(rx))
            }
        };

        with_device_component_mut::<InstallComponent, _, _>(
            owner.clone(),
            move |comp| -> Result<()> {
                let mut waiters = comp.waiters.lock();
                if waiters.is_some() {
                    bail_site!("install request is already in progress");
                }
                *waiters = Some(InstallWaiters {
                    data_type: r#type,
                    prepare_tx: Some(prepare_tx),
                    result_tx: result_tx_opt,
                });
                Ok(())
            },
        )
        .map_err(|err| anyhow_site!("failed to access install component: {:?}", err))??;

        let req_result: Result<WearPacket> = (|| {
            Ok(match r#type {
                MassDataType::Watchface => {
                    let res_config =
                        with_device_component_mut::<XiaomiDevice, ResConfig, _>(
                            owner.clone(),
                            |dev| dev.config.res.clone(),
                        )
                        .map_err(|err| anyhow_site!("failed to access resource config: {:?}", err))?;
                    let id = resutils::get_watchface_id(&file_data, &res_config)
                        .context("invalid watchface id")?;
                    build_watchface_install_request(&id, file_data.len())
                }
                MassDataType::Firmare => build_firmware_install_request(
                    "99.99.99".to_string(),
                    &crate::tools::calc_md5(&file_data),
                    "AstroBox Firmware Update".to_string(),
                ),
                MassDataType::NotificationIcon => {
                    let pkg =
                        package_name.context("package_name is required for notification icon")?;
                    build_notification_icon_request(pkg)
                }
                MassDataType::ThirdPartyApp => {
                    let pkg =
                        package_name.context("package_name is required for third-party app")?;
                    build_thirdparty_app_install_request(pkg, 114514, file_data.len())
                }
            })
        })();

        let req = match req_result {
            Ok(req) => req,
            Err(err) => {
                let owner_for_cleanup = owner.clone();
                universal_block_on(|| clear_install_waiters(owner_for_cleanup.clone()));
                return Err(err);
            }
        };

        if let Err(err) = with_device_component_mut::<XiaomiDevice, (), _>(
            owner.clone(),
            move |dev| {
                packet::cipher::enqueue_pb_packet(
                    dev,
                    req,
                    "InstallSystem::send_install_request_with_progress",
                );
            },
        ) {
            let owner_for_cleanup = owner.clone();
            universal_block_on(|| clear_install_waiters(owner_for_cleanup.clone()));
            return Err(anyhow_site!("failed to enqueue install request: {:?}", err));
        }

        let owner_for_future = owner.clone();
        let progress_cb_future = progress_cb.clone();

        let fut = async move {
            let result = async {
                let prepare_status = prepare_rx
                    .await
                    .map_err(|_| anyhow_site!("prepare response channel closed unexpectedly"))?;

                let prepare_enum = protocol::PrepareStatus::try_from(prepare_status)
                    .map_err(|_| anyhow_site!("unknown prepare status: {prepare_status}"))?;

                if prepare_enum != protocol::PrepareStatus::Ready {
                    bail_site!("install prepare failed with status: {:?}", prepare_enum);
                }

                send_file_for_owner(owner_for_future.clone(), file_data, r#type, move |d| {
                    (progress_cb_future)(d)
                })
                .await
                .context("failed to send MASS payload")?;

                if let Some(result_rx) = result_rx_opt {
                    let event = result_rx
                        .await
                        .map_err(|_| anyhow_site!("install result message missing"))?;
                    handle_install_result(r#type, event)?;
                    if r#type == MassDataType::ThirdPartyApp {
                        refresh_quick_app_list(owner_for_future.clone()).await;
                    }
                }

                Ok(())
            }
            .await;

            clear_install_waiters(owner_for_future).await;
            result
        };

        Ok(Box::pin(fut))
    }
}

impl L2PbExt for InstallSystem {
    fn on_pb_packet(&mut self, payload: protocol::WearPacket) {
        let owner = self.owner_id.clone();
        let _ = with_device_component_mut::<InstallComponent, _, _>(owner, move |comp| {
            let mut waiters_guard = comp.waiters.lock();
            if let Some(waiters) = waiters_guard.as_mut() {
                    match payload.payload {
                        Some(protocol::wear_packet::Payload::WatchFace(wf)) => {
                            if let MassDataType::Watchface = waiters.data_type {
                                match wf.payload {
                                    Some(protocol::watch_face::Payload::PrepareStatus(status)) => {
                                        if let Some(tx) = waiters.prepare_tx.take() {
                                            let _ = tx.send(status);
                                        }
                                    }
                                    Some(protocol::watch_face::Payload::InstallResult(result)) => {
                                        if let Some(tx) = waiters.result_tx.take() {
                                            let _ = tx.send(InstallResultEvent::Watchface(result));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(protocol::wear_packet::Payload::ThirdpartyApp(ta)) => {
                            if let MassDataType::ThirdPartyApp = waiters.data_type {
                                match ta.payload {
                                    Some(protocol::thirdparty_app::Payload::InstallResponse(
                                        resp,
                                    )) => {
                                        if let Some(tx) = waiters.prepare_tx.take() {
                                            let _ = tx.send(resp.prepare_status);
                                        }
                                    }
                                    Some(protocol::thirdparty_app::Payload::InstallResult(
                                        result,
                                    )) => {
                                        if let Some(tx) = waiters.result_tx.take() {
                                            let _ =
                                                tx.send(InstallResultEvent::ThirdpartyApp(result));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some(protocol::wear_packet::Payload::System(sys)) => {
                            if let MassDataType::Firmare = waiters.data_type {
                                if let Some(protocol::system::Payload::PrepareOtaResponse(resp)) =
                                    sys.payload
                                {
                                    if let Some(tx) = waiters.prepare_tx.take() {
                                        let _ = tx.send(resp.prepare_status);
                                    } else if let Some(tx) = waiters.result_tx.take() {
                                        let _ = tx.send(InstallResultEvent::Firmware(resp));
                                    }
                                }
                            }
                        }
                        Some(protocol::wear_packet::Payload::Notification(nc)) => {
                            if let MassDataType::NotificationIcon = waiters.data_type {
                                if let Some(protocol::notification::Payload::AppIconResponse(
                                    resp,
                                )) = nc.payload
                                {
                                    if let Some(tx) = waiters.prepare_tx.take() {
                                        let _ = tx.send(resp.prepare_status);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
            }
        });
    }
}

async fn clear_install_waiters(owner: String) {
    let _ = crate::ecs::with_rt_mut({
        let owner = owner.clone();
        move |rt| {
            let _ = rt.with_device_mut(&owner, |world, entity| {
                if let Some(comp) = world.get_mut::<InstallComponent>(entity) {
                    *comp.waiters.lock() = None;
                }
            });
        }
    })
    .await;
}

async fn refresh_quick_app_list(owner: String) {
    let _ = crate::ecs::with_rt_mut({
        move |rt| {
            let _ = rt.with_device_mut(&owner, |world, entity| {
                if let Some(mut system) = world.get_mut::<ResourceSystem>(entity) {
                    let _ = system.request_quick_app_list();
                }
            });
        }
    })
    .await;
}

fn handle_install_result(r#type: MassDataType, event: InstallResultEvent) -> Result<()> {
    match (r#type, event) {
        (MassDataType::ThirdPartyApp, InstallResultEvent::ThirdpartyApp(result)) => {
            use protocol::app_installer::result::Code;
            let code = Code::try_from(result.code)
                .map_err(|_| anyhow_site!("unknown third-party install code: {}", result.code))?;
            match code {
                Code::InstallSuccess => Ok(()),
                Code::InstallFailed | Code::VerifyFailed => {
                    bail_site!("third-party app install failed: {:?}", code)
                }
            }
        }
        (MassDataType::Watchface, InstallResultEvent::Watchface(result)) => {
            use protocol::install_result::Code;
            let code = Code::try_from(result.code)
                .map_err(|_| anyhow_site!("unknown watchface install code: {}", result.code))?;
            match code {
                Code::InstallSuccess | Code::InstallUsed => Ok(()),
                Code::InstallFailed | Code::VerifyFailed => {
                    bail_site!("watchface install failed: {:?}", code)
                }
            }
        }
        (MassDataType::Firmare, InstallResultEvent::Firmware(resp)) => {
            let status = protocol::PrepareStatus::try_from(resp.prepare_status).map_err(|_| {
                anyhow_site!("unknown firmware prepare status: {}", resp.prepare_status)
            })?;
            if status != protocol::PrepareStatus::Ready {
                bail_site!("firmware install reported status: {:?}", status);
            }
            Ok(())
        }
        (unexpected_type, unexpected_event) => {
            bail_site!(
                "mismatched install result (type={} event={})",
                u8::from(unexpected_type),
                match unexpected_event {
                    InstallResultEvent::ThirdpartyApp(_) => "thirdparty",
                    InstallResultEvent::Watchface(_) => "watchface",
                    InstallResultEvent::Firmware(_) => "firmware",
                }
            )
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct InstallComponent {
    #[serde(skip_serializing)]
    waiters: Mutex<Option<InstallWaiters>>,
}

impl InstallComponent {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(None),
        }
    }
}

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
