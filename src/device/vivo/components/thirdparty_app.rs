use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::response_cid,
    msgpack::{MsgpackReader, write_i32, write_str},
};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::Component,
};

use super::shared::{HasVivoRequestContext, RequestSlot, VivoRequestExt};

const BID_APP_V1: u8 = 40;
const CID_APP_INSTALL_V1: u8 = 1;
const CID_APP_UNINSTALL: u8 = 3;

#[derive(Component)]
pub struct ThirdpartyAppSystem {
    owner_id: String,
    tk_handle: Handle,
    uninstall_wait: RequestSlot<()>,
    install_wait: RequestSlot<()>,
}

impl ThirdpartyAppSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            uninstall_wait: RequestSlot::new(),
            install_wait: RequestSlot::new(),
        }
    }

    /// 发 BID 40 / CID 1 `BleAppInstallReq`，告诉手表「我已经把这个 .rpk 推完了，
    /// 装上吧」。手表会回 `CommonResponse`（仅 retCode）。
    /// `file_id` 必须与 file_v2 SetUpRequestV2.fileId 相同（CRC32 hex）。
    pub fn send_v1_install(
        &mut self,
        app_id: String,
        file_name: String,
        file_id: String,
        version_code: i32,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        if app_id.trim().is_empty() {
            bail_site!("vivo quick-app v1 install: app_id is empty");
        }
        let payload = build_v1_install_payload(&app_id, &file_name, &file_id, version_code)?;
        let (rx, should_enqueue) = self.install_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.ThirdpartyApp] sending BleAppInstallReq app_id={} file_name={} file_id={} version_code={}",
                app_id,
                file_name,
                file_id,
                version_code
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_APP_V1, CID_APP_INSTALL_V1, payload),
                "VivoThirdpartyAppSystem::send_v1_install",
            ) {
                self.install_wait.fail(err);
            }
        }
        Ok(rx)
    }

    fn handle_install_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_common_response_code(&message.payload)?;
        if code != 0 {
            bail_site!("vivo quick-app v1 install rejected by watch: code={code}");
        }
        log::info!("[VivoDevice.ThirdpartyApp] quick-app v1 install accepted");
        self.install_wait.fulfill(());
        Ok(())
    }

    pub fn uninstall_app(
        &mut self,
        package_name: &str,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        let package_name = package_name.trim();
        if package_name.is_empty() {
            bail_site!("vivo app id is empty");
        }

        let mut payload = Vec::new();
        write_str(&mut payload, package_name)
            .map_err(|err| anyhow_site!("failed to encode vivo app uninstall request: {err}"))?;

        let (rx, should_enqueue) = self.uninstall_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.ThirdpartyApp] uninstalling quick-app app_id={}",
                package_name
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_APP_V1, CID_APP_UNINSTALL, payload),
                "VivoThirdpartyAppSystem::uninstall_app",
            ) {
                self.uninstall_wait.fail(err);
            }
        }
        Ok(rx)
    }

    fn handle_uninstall_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_common_response_code(&message.payload)?;
        if code != 0 {
            bail_site!("vivo app uninstall rejected by watch: code={code}");
        }

        log::info!("[VivoDevice.ThirdpartyApp] quick-app uninstall succeeded");
        self.uninstall_wait.fulfill(());
        Ok(())
    }
}

impl HasVivoRequestContext for ThirdpartyAppSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for ThirdpartyAppSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_APP_V1 {
            return;
        }
        let result = match message.cid {
            cid if cid == response_cid(CID_APP_UNINSTALL) => {
                self.handle_uninstall_response(message)
            }
            cid if cid == response_cid(CID_APP_INSTALL_V1) => self.handle_install_response(message),
            _ => Ok(()),
        };
        if let Err(err) = result {
            log::warn!(
                "[VivoDevice.ThirdpartyApp] response failed cid={}: {err:?}",
                message.cid
            );
            match message.cid {
                cid if cid == response_cid(CID_APP_UNINSTALL) => {
                    self.uninstall_wait.fail(anyhow_site!("{err:#}"));
                }
                cid if cid == response_cid(CID_APP_INSTALL_V1) => {
                    self.install_wait.fail(anyhow_site!("{err:#}"));
                }
                _ => {}
            }
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct ThirdpartyAppComponent {}

impl ThirdpartyAppComponent {
    pub fn new() -> Self {
        Self {}
    }
}

fn decode_common_response_code(payload: &[u8]) -> anyhow::Result<i32> {
    let mut reader = MsgpackReader::new(payload);
    reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo common response code: {err}"))
}

fn build_v1_install_payload(
    app_id: &str,
    file_name: &str,
    file_id: &str,
    version_code: i32,
) -> anyhow::Result<Vec<u8>> {
    // jadx WAppBleComReq + BleAppInstallReq: packString(appId), packString(fileName),
    // packString(fileId), packInt(appVerCode).
    let mut out = Vec::with_capacity(app_id.len() + file_name.len() + file_id.len() + 8);
    write_str(&mut out, app_id)
        .map_err(|err| anyhow_site!("failed to encode app install appId: {err}"))?;
    write_str(&mut out, file_name)
        .map_err(|err| anyhow_site!("failed to encode app install fileName: {err}"))?;
    write_str(&mut out, file_id)
        .map_err(|err| anyhow_site!("failed to encode app install fileId: {err}"))?;
    write_i32(&mut out, version_code);
    Ok(out)
}
