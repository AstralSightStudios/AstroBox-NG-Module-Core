// Vivo OTA (BID 23) install command pipeline.
//
// 配合 file_v2 transfer 使用：先把固件 .pkg 推到手表（业务名 TYPE_OTA，BID 64/62），
// 再通过 BID 23 / CID 2 `OTAInstallRequest` 通知手表「文件已就绪，请安装」。
// 手表回 BID 23 / CID 0x82 `OTAInstallResponse`，仅含一个 retCode。
//
// 这块未真机调试，固件版本字段（versionName）是关键 — 手表会把它和 `OTA.unzip()`
// 出的 metadata 做对比，对不上会拒绝。需要 caller 提供真实版本号。

use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::response_cid,
    msgpack::{MsgpackReader, write_bool, write_i32, write_i64, write_str},
};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        components::shared::{HasVivoRequestContext, RequestSlot, VivoRequestExt},
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::Component,
};

const BID_OTA: u8 = 23;
const CID_OTA_FILE_STATUS: u8 = 1;
const CID_OTA_INSTALL: u8 = 2;

#[derive(Component)]
pub struct OtaSystem {
    owner_id: String,
    tk_handle: Handle,
    install_wait: RequestSlot<()>,
    file_status_wait: RequestSlot<i32>,
}

impl OtaSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            install_wait: RequestSlot::new(),
            file_status_wait: RequestSlot::new(),
        }
    }

    /// BID 23 / CID 1：查询某个固件文件在手表上的 OTA 状态，
    /// 用于在推完 .pkg 之前先确认要不要传。
    pub fn query_file_status(
        &mut self,
        version_name: String,
        file_size: u64,
        smart_upgrade: bool,
    ) -> oneshot::Receiver<anyhow::Result<i32>> {
        let payload = match build_file_status_payload(&version_name, file_size, smart_upgrade) {
            Ok(p) => p,
            Err(err) => {
                let (rx, _) = self.file_status_wait.prepare();
                self.file_status_wait.fail(err);
                return rx;
            }
        };
        let (rx, should_enqueue) = self.file_status_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.Ota] querying file status version={} size={} smart={}",
                version_name,
                file_size,
                smart_upgrade
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_OTA, CID_OTA_FILE_STATUS, payload),
                "VivoOtaSystem::query_file_status",
            ) {
                self.file_status_wait.fail(err);
            }
        }
        rx
    }

    /// BID 23 / CID 2：通知手表「固件已上传完成，请装上」。
    /// `install_now=true` 对应 jadx 里 `OTAInstallRequest(versionName, installNow=true)`，
    /// 字段 `f57708s = !installNow`。
    pub fn send_install(
        &mut self,
        version_name: String,
        install_now: bool,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let payload = match build_install_payload(&version_name, install_now) {
            Ok(p) => p,
            Err(err) => {
                let (rx, _) = self.install_wait.prepare();
                self.install_wait.fail(err);
                return rx;
            }
        };
        let (rx, should_enqueue) = self.install_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.Ota] sending OTAInstallRequest version={} install_now={}",
                version_name,
                install_now
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_OTA, CID_OTA_INSTALL, payload),
                "VivoOtaSystem::send_install",
            ) {
                self.install_wait.fail(err);
            }
        }
        rx
    }

    fn handle_install_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_code(&message.payload)?;
        if code != 0 {
            bail_site!("vivo OTA install rejected by watch: code={code}");
        }
        log::info!("[VivoDevice.Ota] OTAInstallResponse accepted");
        self.install_wait.fulfill(());
        Ok(())
    }

    fn handle_file_status_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let mut reader = MsgpackReader::new(&message.payload);
        let code = reader
            .read_i32()
            .map_err(|err| anyhow_site!("failed to decode OTA file status code: {err}"))?;
        // 后续字段 (status, fileExist, retryCount...) 解析略，业务上目前只看 code。
        log::info!("[VivoDevice.Ota] OTAFileStatusResponse code={}", code);
        self.file_status_wait.fulfill(code);
        Ok(())
    }
}

impl HasVivoRequestContext for OtaSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for OtaSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_OTA {
            return;
        }
        let result = match message.cid {
            cid if cid == response_cid(CID_OTA_INSTALL) => self.handle_install_response(message),
            cid if cid == response_cid(CID_OTA_FILE_STATUS) => {
                self.handle_file_status_response(message)
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            log::warn!("[VivoDevice.Ota] message handling failed: {err:?}");
            match message.cid {
                cid if cid == response_cid(CID_OTA_INSTALL) => {
                    self.install_wait.fail(anyhow_site!("{err:#}"));
                }
                cid if cid == response_cid(CID_OTA_FILE_STATUS) => {
                    self.file_status_wait.fail(anyhow_site!("{err:#}"));
                }
                _ => {}
            }
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct OtaComponent {
    pub last_version: Option<String>,
}

impl OtaComponent {
    pub fn new() -> Self {
        Self { last_version: None }
    }
}

fn build_install_payload(version_name: &str, install_now: bool) -> anyhow::Result<Vec<u8>> {
    // jadx OTAInstallRequest.toPayload: packString(versionName), packBoolean(!installNow)
    let mut out = Vec::with_capacity(version_name.len() + 8);
    write_str(&mut out, version_name)
        .map_err(|err| anyhow_site!("failed to encode OTA install version: {err}"))?;
    write_bool(&mut out, !install_now);
    Ok(out)
}

fn build_file_status_payload(
    version_name: &str,
    file_size: u64,
    smart_upgrade: bool,
) -> anyhow::Result<Vec<u8>> {
    // jadx OTAFileStatusRequest.toPayload:
    //   packString(versionName), packInt(installLater), packInt(smartUpgrade), packLong(fileSize)
    let mut out = Vec::with_capacity(version_name.len() + 16);
    write_str(&mut out, version_name)
        .map_err(|err| anyhow_site!("failed to encode OTA file-status version: {err}"))?;
    write_i32(&mut out, 0); // installLater = 0 means we want to install
    write_i32(&mut out, if smart_upgrade { 1 } else { 0 });
    write_i64(&mut out, file_size as i64);
    Ok(out)
}

fn decode_code(payload: &[u8]) -> anyhow::Result<i32> {
    let mut reader = MsgpackReader::new(payload);
    reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode OTA response code: {err}"))
}
