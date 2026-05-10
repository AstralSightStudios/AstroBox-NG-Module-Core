use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::messages::{
    generated::typed::{
        CloudPhoneMsgRequest, CloudSupportRequest, CloudSupportResponse, CloudWatchMsgRequest,
        CloudWatchSwitchRequest, CloudWatchSwitchResp,
    },
    response_cid,
};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::{Component, access::with_device_component_mut},
};

use super::shared::{HasVivoRequestContext, RequestSlot, VivoRequestExt};

const BID_CLOUD: u8 = 47;
const CID_CLOUD_MSG: u8 = 1;
const CID_PHONE_MSG: u8 = 2;
const CID_SUPPORT: u8 = 3;
const CID_SWITCH: u8 = 5;

#[derive(Component)]
pub struct CloudBridgeSystem {
    owner_id: String,
    tk_handle: Handle,
    message_wait: RequestSlot<()>,
    support_wait: RequestSlot<CloudBridgeSupport>,
    switch_wait: RequestSlot<()>,
}

impl CloudBridgeSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            message_wait: RequestSlot::new(),
            support_wait: RequestSlot::new(),
            switch_wait: RequestSlot::new(),
        }
    }

    pub fn query_support(
        &mut self,
        phone_support: bool,
    ) -> oneshot::Receiver<anyhow::Result<CloudBridgeSupport>> {
        let (rx, should_enqueue) = self.support_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.CloudBridge] querying support phone_support={}",
                phone_support
            );
            let payload = CloudSupportRequest {
                is_support: if phone_support { 1 } else { 0 },
            }
            .payload();
            match payload {
                Ok(payload) => {
                    if let Err(err) = self.send_vivo_message(
                        VscpMessage::new(BID_CLOUD, CID_SUPPORT, payload),
                        "VivoCloudBridgeSystem::query_support",
                    ) {
                        self.support_wait.fail(err);
                    }
                }
                Err(err) => {
                    self.support_wait.fail(anyhow_site!(
                        "failed to encode vivo cloud support request: {err}"
                    ));
                }
            }
        }
        rx
    }

    pub fn send_cloud_message(
        &mut self,
        msg: String,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        if msg.trim().is_empty() {
            bail_site!("vivo cloud message is empty");
        }
        let payload = CloudPhoneMsgRequest { msg: msg.clone() }
            .payload()
            .map_err(|err| anyhow_site!("failed to encode vivo cloud message: {err}"))?;
        let (rx, should_enqueue) = self.message_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.CloudBridge] sending cloud message bytes={}",
                msg.len()
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_CLOUD, CID_PHONE_MSG, payload),
                "VivoCloudBridgeSystem::send_cloud_message",
            ) {
                self.message_wait.fail(err);
            } else {
                self.message_wait.fulfill(());
            }
        }
        Ok(rx)
    }

    pub fn set_switch(
        &mut self,
        switch_on: bool,
    ) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        let payload = CloudWatchSwitchRequest {
            switch_on: if switch_on { 1 } else { 0 },
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo cloud switch request: {err}"))?;
        let (rx, should_enqueue) = self.switch_wait.prepare();
        if should_enqueue {
            log::info!(
                "[VivoDevice.CloudBridge] setting cloud switch={}",
                switch_on
            );
            if let Err(err) = self.send_vivo_message(
                VscpMessage::new(BID_CLOUD, CID_SWITCH, payload),
                "VivoCloudBridgeSystem::set_switch",
            ) {
                self.switch_wait.fail(err);
            }
        }
        Ok(rx)
    }

    fn handle_support_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = CloudSupportResponse::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo cloud support response: {err}"))?;
        if resp.code != 0 {
            bail_site!(
                "vivo cloud support query rejected by watch: code={}",
                resp.code
            );
        }

        let support = CloudBridgeSupport {
            is_support: resp.is_support == 1,
            is_open: resp.is_open == 1,
        };
        update_cloud_component(&self.owner_id, {
            let support = support.clone();
            move |comp| {
                comp.support = Some(support.clone());
            }
        })?;
        log::info!(
            "[VivoDevice.CloudBridge] support response support={} open={}",
            support.is_support,
            support.is_open
        );
        self.support_wait.fulfill(support);
        Ok(())
    }

    fn handle_switch_request(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let req = CloudWatchSwitchRequest::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo cloud switch request: {err}"))?;
        let is_open = req.switch_on == 1;
        update_cloud_component(&self.owner_id, move |comp| {
            comp.watch_switch_open = Some(is_open);
        })?;
        log::info!(
            "[VivoDevice.CloudBridge] watch cloud switch changed open={}",
            is_open
        );
        let payload = CloudWatchSwitchResp { code: 0 }
            .payload()
            .map_err(|err| anyhow_site!("failed to encode vivo cloud switch ack: {err}"))?;
        self.send_vivo_message(
            VscpMessage::new(BID_CLOUD, response_cid(CID_SWITCH), payload),
            "VivoCloudBridgeSystem::ack_switch",
        )
    }

    fn handle_switch_response(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = CloudWatchSwitchResp::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo cloud switch response: {err}"))?;
        if resp.code != 0 {
            bail_site!("vivo cloud switch rejected by watch: code={}", resp.code);
        }
        self.switch_wait.fulfill(());
        Ok(())
    }

    fn handle_cloud_message(&mut self, message: &VscpMessage) -> anyhow::Result<()> {
        let req = CloudWatchMsgRequest::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo cloud message: {err}"))?;
        log::debug!(
            "[VivoDevice.CloudBridge] received cloud message bytes={}",
            req.msg.len()
        );
        let msg = req.msg;
        update_cloud_component(&self.owner_id, {
            let msg = msg.clone();
            move |comp| {
                comp.last_message = Some(msg);
            }
        })?;
        crate::events::emit(crate::events::CoreEvent::InterconnectMessage(
            crate::events::InterconnectMessage {
                device_addr: self.owner_id.clone(),
                pkg_name: "vivo.cloud".to_string(),
                payload: msg.into_bytes(),
            },
        ));
        Ok(())
    }
}

impl HasVivoRequestContext for CloudBridgeSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for CloudBridgeSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_CLOUD {
            return;
        }

        let result = match message.cid {
            CID_CLOUD_MSG => self.handle_cloud_message(message),
            CID_SWITCH => self.handle_switch_request(message),
            cid if cid == response_cid(CID_SUPPORT) => self.handle_support_response(message),
            cid if cid == response_cid(CID_SWITCH) => self.handle_switch_response(message),
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice.CloudBridge] message handling failed: {err:?}");
            match message.cid {
                cid if cid == response_cid(CID_SUPPORT) => {
                    self.support_wait.fail(anyhow_site!("{err:#}"));
                }
                cid if cid == response_cid(CID_SWITCH) => {
                    self.switch_wait.fail(anyhow_site!("{err:#}"));
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudBridgeSupport {
    pub is_support: bool,
    pub is_open: bool,
}

#[derive(Component, serde::Serialize)]
pub struct CloudBridgeComponent {
    pub support: Option<CloudBridgeSupport>,
    pub watch_switch_open: Option<bool>,
    pub last_message: Option<String>,
}

impl CloudBridgeComponent {
    pub fn new() -> Self {
        Self {
            support: None,
            watch_switch_open: None,
            last_message: None,
        }
    }
}

fn update_cloud_component<F>(owner_id: &str, f: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut CloudBridgeComponent) + Send + 'static,
{
    with_device_component_mut::<CloudBridgeComponent, _, _>(owner_id.to_string(), f)
        .map_err(|err| anyhow_site!("failed to update vivo cloud component: {err:?}"))
}
