use crate::{
    device::xiaomi::{
        components::auth::AuthComponent,
        packet::v2::layer2::{L2Cipher, L2Packet},
        XiaomiDevice,
    },
    ecs::entity::{Entity, EntityExt},
};
use pb::xiaomi::protocol;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};
use tokio::runtime::Handle;

pub mod mass;
pub mod v2;

pub type SharedL2Cipher = Arc<dyn L2Cipher + Send + Sync>;

static GLOBAL_L2_CIPHERS: OnceLock<RwLock<HashMap<String, SharedL2Cipher>>> = OnceLock::new();

fn cipher_registry() -> &'static RwLock<HashMap<String, SharedL2Cipher>> {
    GLOBAL_L2_CIPHERS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn register_l2_cipher(device_id: String, cipher: SharedL2Cipher) {
    match cipher_registry().write() {
        Ok(mut guard) => {
            guard.insert(device_id, cipher);
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.insert(device_id, cipher);
        }
    }
}

pub fn get_l2_cipher(device_id: &str) -> Option<SharedL2Cipher> {
    match cipher_registry().read() {
        Ok(guard) => guard.get(device_id).cloned(),
        Err(poisoned) => poisoned.into_inner().get(device_id).cloned(),
    }
}

pub async fn ensure_l2_cipher(device_id: &str, sar_version: u32) -> Option<SharedL2Cipher> {
    if let Some(existing) = get_l2_cipher(device_id) {
        return Some(existing);
    }

    match sar_version {
        2 => V2L2Cipher::new(device_id.to_string()).await.map(|raw| {
            let cipher: SharedL2Cipher = Arc::new(raw);
            register_l2_cipher(device_id.to_string(), cipher.clone());
            cipher
        }),
        _ => None,
    }
}

pub fn ensure_l2_cipher_blocking(device_id: &str, sar_version: u32) -> Option<SharedL2Cipher> {
    if let Some(existing) = get_l2_cipher(device_id) {
        return Some(existing);
    }

    let device_id_owned = device_id.to_string();
    crate::asyncrt::universal_block_on(|| async {
        ensure_l2_cipher(&device_id_owned, sar_version).await
    })
}

pub fn encode_pb_packet(
    dev: &XiaomiDevice,
    packet: protocol::WearPacket,
    log_ctx: &str,
) -> Vec<u8> {
    match ensure_l2_cipher_blocking(&dev.addr, dev.sar_version) {
        Some(cipher) => match L2Packet::pb_write_enc(packet.clone(), cipher.as_ref()) {
            Ok(pkt) => pkt.to_bytes(),
            Err(err) => {
                log::error!(
                    "[{}] pb_write_enc failed, fallback to plain write: {}",
                    log_ctx,
                    err
                );
                L2Packet::pb_write(packet).to_bytes()
            }
        },
        None => L2Packet::pb_write(packet).to_bytes(),
    }
}

pub fn enqueue_pb_packet(dev: &mut XiaomiDevice, packet: protocol::WearPacket, log_ctx: &str) {
    let bytes = encode_pb_packet(dev, packet, log_ctx);
    dev.sar.enqueue(bytes);
}

pub fn on_packet(tk_handle: Handle, device_id: String, data: Vec<u8>) {
    crate::asyncrt::spawn_with_handle(
        async move {
            let sar_version = crate::ecs::with_rt_mut({
                let device_id_clone = device_id.clone();
                move |rt| {
                    rt.find_entity_by_id_mut::<XiaomiDevice>(&device_id_clone)
                        .map(|dev| dev.sar_version)
                }
            })
            .await;
            let shared_cipher = match sar_version {
                Some(version) => ensure_l2_cipher(&device_id, version).await,
                None => None,
            };

            let mut i = 0usize;
            while i + 8 <= data.len() {
                if !(data[i] == 0xa5 && data[i + 1] == 0xa5) {
                    if let Some(pos) = data[i + 1..].windows(2).position(|w| w == [0xa5, 0xa5]) {
                        i += 1 + pos;
                        continue;
                    } else {
                        break;
                    }
                }

                let declared_len = u16::from_le_bytes([data[i + 4], data[i + 5]]) as usize;
                let total = 8 + declared_len;
                if i + total > data.len() {
                    break;
                }

                let l1_bytes = &data[i..i + total];
                let l1 =
                    match crate::device::xiaomi::packet::v2::layer1::L1Packet::from_bytes(l1_bytes)
                    {
                        Ok(p) => p,
                        Err(err) => {
                            log::warn!("Decode L1 Packet Err: {}", err.to_string());
                            i += 2;
                            continue;
                        }
                    };

                let deliver_up = crate::ecs::with_rt_mut({
                    let device_id_lookup = device_id.clone();
                    let l1_clone = l1.clone();
                    move |rt| {
                        if let Some(dev) =
                            rt.find_entity_by_id_mut::<XiaomiDevice>(&device_id_lookup)
                        {
                            if dev.sar_version == 2 {
                                return dev.sar.on_l1_packet(&l1_clone);
                            }
                        }
                        false
                    }
                })
                .await;

                if deliver_up {
                    let cipher_ref = shared_cipher.as_ref().map(|c| c.as_ref() as &dyn L2Cipher);
                    if let Ok(l2p) = crate::device::xiaomi::packet::v2::layer2::L2Packet::from_l1(
                        &l1, cipher_ref,
                    ) {
                        let ch = l2p.channel;
                        let op = l2p.opcode;
                        let payload = l2p.payload;

                        crate::ecs::with_rt_mut({
                            let device_id_dispatch = device_id.clone();
                            move |rt| {
                                if let Some(dev) =
                                    rt.find_entity_by_id_mut::<XiaomiDevice>(&device_id_dispatch)
                                {
                                    if dev.sar_version == 2 {
                                        for comp in dev.components() {
                                            if let Some(logic_comp) = comp.as_logic_component_mut() {
                                                let sys = logic_comp.system_mut();
                                                super::system::try_invoke_xiaomi_system_ext_on_l2packet(
                                                    sys, ch, op, &payload,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        })
                        .await;
                    }
                }

                i += total;
            }
        },
        tk_handle,
    );
}

pub struct V2L2Cipher {
    enc_key: Vec<u8>,
    dec_key: Vec<u8>,
}

impl V2L2Cipher {
    pub async fn new(device_id: String) -> Option<Self> {
        let device_id_clone = device_id.clone();
        let keys = crate::ecs::with_rt_mut(move |rt| {
            if let Some(device) = rt.find_entity_by_id_mut::<XiaomiDevice>(&device_id_clone) {
                if let Ok(auth_comp) =
                    device.get_component_as_mut::<AuthComponent>(AuthComponent::ID)
                {
                    return (auth_comp.enc_key.clone(), auth_comp.dec_key.clone());
                }
            }
            (vec![], vec![])
        })
        .await;
        let enc_key = keys.0;
        let dec_key = keys.1;
        if enc_key.len() == 16 && dec_key.len() == 16 {
            Some(Self { enc_key, dec_key })
        } else {
            log::debug!(
                "ensure_l2_cipher: device {} missing auth keys (enc={}, dec={})",
                device_id,
                enc_key.len(),
                dec_key.len()
            );
            None
        }
    }
}

impl L2Cipher for V2L2Cipher {
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, ()> {
        Ok(crate::crypto::aesctr::aes128_ctr_crypt(
            &crate::tools::vec_to_array_16_opt(&self.enc_key).unwrap(),
            &crate::tools::vec_to_array_16_opt(&self.enc_key).unwrap(),
            plaintext,
        ))
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, ()> {
        Ok(crate::crypto::aesctr::aes128_ctr_crypt(
            &crate::tools::vec_to_array_16_opt(&self.dec_key).unwrap(),
            &crate::tools::vec_to_array_16_opt(&self.dec_key).unwrap(),
            ciphertext,
        ))
    }
}
