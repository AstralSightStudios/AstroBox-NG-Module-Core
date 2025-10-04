use std::{future::Future, pin::Pin, sync::Arc};

use crate::{
    asyncrt::universal_block_on,
    device::xiaomi::{
        components::{
            auth::AuthComponent, info::InfoComponent, install::InstallComponent,
            mass::MassComponent, resource::ResourceComponent, thirdparty::ThirdpartyAppComponent,
        },
        config::XiaomiDeviceConfig,
        r#type::ConnectType,
    },
    ecs::entity::{Entity, EntityMeta},
    impl_has_entity_meta,
};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

pub mod components;
pub mod config;
pub mod packet;
pub mod resutils;
pub mod sar;
pub mod system;
pub mod r#type;

#[derive(Debug)]
pub enum SendError {
    Disconnected,
    Io(String),
}

type SendFuture = Pin<Box<dyn Future<Output = Result<(), SendError>> + Send>>;
type SendFn = Arc<dyn Fn(Vec<u8>) -> SendFuture + Send + Sync>;

pub struct XiaomiDevice {
    meta: EntityMeta,
    pub name: String,
    pub addr: String,
    pub sar_version: u32,
    pub connect_type: ConnectType,
    pub force_android: bool,
    sender: SendFn,
    pub sar: sar::SarController,
    pub config: XiaomiDeviceConfig,
}

impl XiaomiDevice {
    pub fn new<F, Fut>(
        tk_handle: Handle,
        name: String,
        addr: String,
        authkey: String,
        sar_version: u32,
        connect_type: ConnectType,
        force_android: bool,
        config: XiaomiDeviceConfig,
        sender: F,
    ) -> Self
    where
        F: Fn(Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), SendError>> + Send + 'static,
    {
        let raw_sender: SendFn = Arc::new(move |data: Vec<u8>| Box::pin(sender(data)));
        let send_lock = Arc::new(Mutex::new(()));
        let transport_config = config.transport.clone();
        let sender: SendFn = {
            let raw_sender = raw_sender.clone();
            let send_lock = send_lock.clone();
            Arc::new(move |data: Vec<u8>| {
                let raw_sender = raw_sender.clone();
                let send_lock = send_lock.clone();
                let chunk_size_ble = transport_config.chunk_size_ble;
                let chunk_size_spp = transport_config.chunk_size_spp;
                Box::pin(async move {
                    let _guard = send_lock.lock().await;

                    let mut chunk_size_max = chunk_size_ble;
                    if connect_type == ConnectType::SPP {
                        chunk_size_max = chunk_size_spp;
                    }

                    if data.len() <= chunk_size_max {
                        raw_sender(data).await
                    } else {
                        for chunk in data.chunks(chunk_size_max) {
                            raw_sender(chunk.to_vec()).await?;
                        }
                        Ok(())
                    }
                })
            })
        };

        let auth_comp = AuthComponent::new(authkey.clone());
        let install_comp = InstallComponent::new();
        let mass_comp = MassComponent::new();
        let info_comp = InfoComponent::new();
        let thirdparty_comp = ThirdpartyAppComponent::new();
        let resource_comp = ResourceComponent::new();

        // 创建 SAR 控制器，并传入设备名以便定时任务访问
        if connect_type == ConnectType::SPP {
            universal_block_on(|| async {
                sender(crate::tools::hex_stream_to_bytes("badcfe00c00300000100ef").unwrap())
                    .await
                    .unwrap();
            });
        }
        let sar =
            sar::SarController::new(tk_handle, sender.clone(), addr.clone(), config.sar.clone());

        let mut dev = Self {
            meta: EntityMeta {
                id: addr.clone(),
                components: vec![],
                comp_index: std::collections::HashMap::new(),
            },
            name,
            addr,
            sar_version,
            connect_type,
            force_android,
            sender,
            sar,
            config,
        };

        dev.add_component(Box::new(auth_comp));
        dev.add_component(Box::new(install_comp));
        dev.add_component(Box::new(mass_comp));
        dev.add_component(Box::new(info_comp));
        dev.add_component(Box::new(thirdparty_comp));
        dev.add_component(Box::new(resource_comp));
        dev
    }

    pub async fn send_data(&self, data: Vec<u8>) -> Result<(), SendError> {
        (self.sender)(data).await
    }
}

impl_has_entity_meta!(XiaomiDevice, meta);
