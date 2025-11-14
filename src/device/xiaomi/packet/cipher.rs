use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};

use pb::xiaomi::protocol;

use crate::{
    device::xiaomi::{
        XiaomiDevice,
        components::auth::AuthComponent,
        packet::v2::layer2::{L2Cipher, L2Packet},
    },
    ecs::entity::EntityExt,
};

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
    match ensure_l2_cipher_blocking(dev.addr(), dev.sar_version) {
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
