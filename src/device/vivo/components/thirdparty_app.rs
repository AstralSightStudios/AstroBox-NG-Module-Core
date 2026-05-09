use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::response_cid,
    msgpack::{MsgpackReader, write_str},
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
const CID_APP_UNINSTALL: u8 = 3;

#[derive(Component)]
pub struct ThirdpartyAppSystem {
    owner_id: String,
    tk_handle: Handle,
    uninstall_wait: RequestSlot<()>,
}

impl ThirdpartyAppSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            uninstall_wait: RequestSlot::new(),
        }
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
        if message.bid != BID_APP_V1 || message.cid != response_cid(CID_APP_UNINSTALL) {
            return;
        }

        if let Err(err) = self.handle_uninstall_response(message) {
            log::warn!("[VivoDevice.ThirdpartyApp] uninstall response failed: {err:?}");
            self.uninstall_wait.fail(err);
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
