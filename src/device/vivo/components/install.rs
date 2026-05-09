use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::{generated::typed::BleAppInstallV2Req, response_cid},
    msgpack::MsgpackReader,
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

const BID_APP_V2: u8 = 41;
const CID_INSTALL: u8 = 3;
const CID_STOP_INSTALL: u8 = 4;
const CID_CANCEL_INSTALL: u8 = 6;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VivoQuickAppInstallRequest {
    pub app_id: String,
    pub app_name: String,
    pub app_file_size: i32,
    pub app_url: String,
    #[serde(default)]
    pub file_md5: String,
    #[serde(default)]
    pub app_icon: String,
}

impl VivoQuickAppInstallRequest {
    fn validate(&self) -> anyhow::Result<()> {
        if self.app_id.trim().is_empty() {
            bail_site!("vivo quick-app appId is empty");
        }
        if self.app_name.trim().is_empty() {
            bail_site!("vivo quick-app appName is empty");
        }
        if self.app_url.trim().is_empty() {
            bail_site!("vivo quick-app appUrl is empty");
        }
        if self.app_file_size < 0 {
            bail_site!("vivo quick-app appFileSize is negative");
        }
        Ok(())
    }

    fn into_ble_req(self) -> BleAppInstallV2Req {
        BleAppInstallV2Req {
            app_id: self.app_id,
            app_name: self.app_name,
            app_file_size: self.app_file_size,
            app_url: self.app_url,
            file_md5: self.file_md5,
            app_icon: self.app_icon,
        }
    }
}

#[derive(Component)]
pub struct InstallSystem {
    owner_id: String,
    tk_handle: Handle,
    install_wait: RequestSlot<()>,
    stop_wait: RequestSlot<()>,
    cancel_wait: RequestSlot<()>,
}

impl InstallSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            install_wait: RequestSlot::new(),
            stop_wait: RequestSlot::new(),
            cancel_wait: RequestSlot::new(),
        }
    }

    pub fn install_quick_app_by_url(
        &mut self,
        req: VivoQuickAppInstallRequest,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        self.send_quick_app_install_control(
            req,
            CID_INSTALL,
            "installing",
            "VivoInstallSystem::install_quick_app_by_url",
        )
    }

    pub fn stop_quick_app_install(
        &mut self,
        req: VivoQuickAppInstallRequest,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        self.send_quick_app_install_control(
            req,
            CID_STOP_INSTALL,
            "stopping install for",
            "VivoInstallSystem::stop_quick_app_install",
        )
    }

    pub fn cancel_quick_app_install(
        &mut self,
        req: VivoQuickAppInstallRequest,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        self.send_quick_app_install_control(
            req,
            CID_CANCEL_INSTALL,
            "cancelling install for",
            "VivoInstallSystem::cancel_quick_app_install",
        )
    }

    fn send_quick_app_install_control(
        &mut self,
        req: VivoQuickAppInstallRequest,
        cid: u8,
        verb: &'static str,
        log_ctx: &'static str,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        req.validate()?;
        let app_id = req.app_id.clone();
        let app_name = req.app_name.clone();
        if req.file_md5.trim().is_empty() {
            log::warn!(
                "[VivoDevice.Install] quick-app {} has empty fileMd5; official app normally provides it",
                app_id
            );
        }
        if req.app_file_size == 0 {
            log::warn!(
                "[VivoDevice.Install] quick-app {} has zero appFileSize; watch may reject the request",
                app_id
            );
        }

        let payload = req.into_ble_req().payload().map_err(|err| {
            anyhow_site!("failed to encode vivo quick-app install request: {err}")
        })?;

        let (rx, should_enqueue) = self.slot_for_cid(cid)?.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.Install] {} quick-app app_id={} name={}",
                verb,
                app_id,
                app_name
            );
            let send_result =
                self.send_vivo_message(VscpMessage::new(BID_APP_V2, cid, payload), log_ctx);
            if let Err(err) = send_result {
                self.slot_for_cid(cid)?.fail(err);
            }
        }
        Ok(rx)
    }

    fn slot_for_cid(&mut self, cid: u8) -> anyhow::Result<&mut RequestSlot<()>> {
        match cid {
            CID_INSTALL => Ok(&mut self.install_wait),
            CID_STOP_INSTALL => Ok(&mut self.stop_wait),
            CID_CANCEL_INSTALL => Ok(&mut self.cancel_wait),
            _ => bail_site!("unsupported vivo quick-app install control cid={cid}"),
        }
    }

    fn handle_common_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_common_response_code(&message.payload)?;
        if code != 0 {
            bail_site!(
                "vivo quick-app install control rejected by watch: cid={} code={code}",
                message.cid
            );
        }

        log::info!(
            "[VivoDevice.Install] quick-app install control succeeded cid={}",
            message.cid
        );
        match message.cid {
            cid if cid == response_cid(CID_INSTALL) => self.install_wait.fulfill(()),
            cid if cid == response_cid(CID_STOP_INSTALL) => self.stop_wait.fulfill(()),
            cid if cid == response_cid(CID_CANCEL_INSTALL) => self.cancel_wait.fulfill(()),
            _ => {}
        }
        Ok(())
    }
}

impl HasVivoRequestContext for InstallSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for InstallSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_APP_V2 {
            return;
        }

        let result = match message.cid {
            cid if cid == response_cid(CID_INSTALL)
                || cid == response_cid(CID_STOP_INSTALL)
                || cid == response_cid(CID_CANCEL_INSTALL) =>
            {
                self.handle_common_response(message)
            }
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice.Install] response handling failed: {err:?}");
            match message.cid {
                cid if cid == response_cid(CID_INSTALL) => {
                    self.install_wait.fail(anyhow_site!("{err:#}"));
                }
                cid if cid == response_cid(CID_STOP_INSTALL) => {
                    self.stop_wait.fail(anyhow_site!("{err:#}"));
                }
                cid if cid == response_cid(CID_CANCEL_INSTALL) => {
                    self.cancel_wait.fail(anyhow_site!("{err:#}"));
                }
                _ => {}
            }
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct InstallComponent {
    pub last_installing_app_id: Option<String>,
}

impl InstallComponent {
    pub fn new() -> Self {
        Self {
            last_installing_app_id: None,
        }
    }
}

fn decode_common_response_code(payload: &[u8]) -> anyhow::Result<i32> {
    let mut reader = MsgpackReader::new(payload);
    reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo install response code: {err}"))
}
