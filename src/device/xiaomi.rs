use std::{future::Future, pin::Pin, sync::Arc};

#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::NetworkComponent;
use crate::{
    asyncrt::universal_block_on,
    device::{
        Device,
        xiaomi::{
            components::{
                auth::AuthComponent, info::InfoComponent, install::InstallComponent, mass::MassComponent, resource::ResourceComponent, sync::SyncComponent, thirdparty_app::ThirdpartyAppComponent, watchface::WatchfaceComponent
            },
            config::XiaomiDeviceConfig,
            r#type::ConnectType,
        },
    },
    ecs::entity::{Entity, EntityExt, EntityMeta, HasEntityMeta},
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

#[derive(serde::Serialize)]
pub struct XiaomiDevice {
    #[serde(flatten)]
    device: Device,
    pub sar_version: u32,          // SAR 协议版本，对应SPP v?
    pub connect_type: ConnectType, // 连接类型，SPP or BLE
    pub force_android: bool, // 安卓人安卓代码安卓生态安卓手表安卓设备安卓pb 在连接设备时强制使用ANDROID作为设备类型
    #[serde(skip_serializing)]
    sender: SendFn,
    #[serde(skip_serializing)]
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
        // 包装线程安全Sender
        let raw_sender: SendFn = Arc::new(move |data: Vec<u8>| Box::pin(sender(data)));
        // 上锁防止串串包
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

        // 初始化Components
        let auth_comp = AuthComponent::new(authkey.clone());
        let install_comp = InstallComponent::new();
        let mass_comp = MassComponent::new();
        let info_comp = InfoComponent::new();
        let thirdparty_comp = ThirdpartyAppComponent::new();
        let resource_comp = ResourceComponent::new();
        let watchface_comp = WatchfaceComponent::new();
        #[cfg(not(target_arch = "wasm32"))]
        let network_comp = NetworkComponent::new(config.network.clone());
        let sync_comp = SyncComponent::new();

        // 不知道为什么傻逼小米针对SPP连接要发这么一个神秘Hello
        if connect_type == ConnectType::SPP {
            universal_block_on(|| async {
                sender(crate::tools::hex_stream_to_bytes("badcfe00c00300000100ef").unwrap())
                    .await
                    .unwrap();
            });
        }

        let base = Device::new(name, addr);
        // 创建 SAR 控制器，并传入设备名以便定时任务访问
        let sar = sar::SarController::new(
            tk_handle.clone(),
            sender.clone(),
            base.addr().to_string(),
            config.sar.clone(),
        );

        let mut dev = Self {
            device: base,
            sar_version,
            connect_type,
            force_android,
            sender,
            sar,
            config,
        };

        // 挂载Components
        dev.add_component(Box::new(auth_comp));
        dev.add_component(Box::new(install_comp));
        dev.add_component(Box::new(mass_comp));
        dev.add_component(Box::new(info_comp));
        dev.add_component(Box::new(thirdparty_comp));
        dev.add_component(Box::new(resource_comp));
        dev.add_component(Box::new(watchface_comp));
        #[cfg(not(target_arch = "wasm32"))]
        dev.add_component(Box::new(network_comp));
        dev.add_component(Box::new(sync_comp));

        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Ok(comp) = dev.get_component_as_mut::<NetworkComponent>(NetworkComponent::ID) {
                if let Err(err) = comp.ensure_stack(tk_handle.clone()) {
                    log::warn!("[XiaomiDevice] failed to start network stack: {err:?}");
                }
            }
        }
        dev
    }

    pub async fn send_data(&self, data: Vec<u8>) -> Result<(), SendError> {
        (self.sender)(data).await
    }

    pub fn base(&self) -> &Device {
        &self.device
    }

    pub fn base_mut(&mut self) -> &mut Device {
        &mut self.device
    }

    pub fn name(&self) -> &str {
        self.device.name()
    }

    pub fn addr(&self) -> &str {
        self.device.addr()
    }
}

impl HasEntityMeta for XiaomiDevice {
    fn meta(&self) -> &EntityMeta {
        self.device.meta()
    }

    fn meta_mut(&mut self) -> &mut EntityMeta {
        self.device.meta_mut()
    }
}
