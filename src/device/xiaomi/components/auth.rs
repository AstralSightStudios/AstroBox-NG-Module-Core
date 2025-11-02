use crate::crypto::aesccm::aes128_ccm_encrypt;
use crate::device::xiaomi::XiaomiDevice;
use crate::device::xiaomi::packet::v2::layer2::L2Packet;
use crate::device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet};
use crate::device::xiaomi::r#type::ConnectType;
use crate::ecs::entity::LookupError;
use crate::ecs::fastlane::FastLane;
use crate::ecs::system::{SysMeta, System};
use crate::impl_has_sys_meta;
use crate::{anyhow_site, bail_site};
use crate::{ecs::logic_component::LogicCompMeta, impl_logic_component};
use anyhow::Context;
use hmac::{Hmac, Mac};
use pb::xiaomi::protocol::WearPacket;
use prost::Message;
use sha2::Sha256;
use tokio::sync::oneshot;

pub struct AuthSystem {
    meta: SysMeta,
    auth_wait: Option<oneshot::Sender<anyhow::Result<()>>>,
}

impl Default for AuthSystem {
    fn default() -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self {
            meta: SysMeta::default(),
            auth_wait: None,
        }
    }
}

impl AuthSystem {
    pub fn prepare_auth(&mut self) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        if self.auth_wait.is_some() {
            bail_site!("auth flow already in progress");
        }

        let nonce = crate::tools::generate_random_bytes(16);

        let this: &mut dyn System = self;
        let nonce_clone = nonce.clone();
        FastLane::with_component_mut::<AuthComponent, (), _>(
            this,
            AuthComponent::ID,
            move |comp| {
                comp.random_bytes = nonce_clone;
            },
        )
        .map_err(|err| anyhow_site!("failed to update auth component nonce: {err:?}"))?;

        FastLane::with_entity_mut::<(), _>(this, move |ent| {
            let entity_id = ent.id().to_string();
            let dev = ent
                .as_any_mut()
                .downcast_mut::<XiaomiDevice>()
                .ok_or_else(|| LookupError::TypeMismatch {
                    id: entity_id,
                    expected: std::any::type_name::<XiaomiDevice>(),
                    actual: std::any::type_name::<dyn crate::ecs::entity::Entity>(),
                })?;
            dev.sar
                .enqueue(L2Packet::pb_write(build_auth_step_1(&nonce)).to_bytes());
            Ok(())
        })
        .map_err(|err| anyhow_site!("failed to send auth step 1 packet: {err:?}"))?;

        let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
        self.auth_wait = Some(tx);

        Ok(rx)
    }

    pub async fn start_auth(&mut self) -> anyhow::Result<()> {
        let rx = self.prepare_auth()?;
        let result = rx.await.context("Auth await response not received")?;
        result
    }
}

impl L2PbExt for AuthSystem {
    fn on_pb_packet(&mut self, payload: WearPacket) {
        log::info!("on_pb_packet: {}", serde_json::to_string(&payload).unwrap());
        if let Some(pkt) = payload.payload {
            match pkt {
                pb::xiaomi::protocol::wear_packet::Payload::Account(acc) => {
                    if let Some(acc_payload) = acc.payload {
                        let this: &mut dyn System = self;

                        match acc_payload {
                            pb::xiaomi::protocol::account::Payload::AuthDeviceVerify(
                                verify_pkt,
                            ) => match build_auth_step_2(this, &verify_pkt) {
                                Ok(verify_ret) => {
                                    if let Err(err) =
                                        FastLane::with_entity_mut::<(), _>(this, move |ent| {
                                            let entity_id = ent.id().to_string();
                                            let dev = ent
                                                .as_any_mut()
                                                .downcast_mut::<XiaomiDevice>()
                                                .ok_or_else(|| LookupError::TypeMismatch {
                                                    id: entity_id,
                                                    expected: std::any::type_name::<XiaomiDevice>(),
                                                    actual: std::any::type_name::<
                                                        dyn crate::ecs::entity::Entity,
                                                    >(
                                                    ),
                                                })?;

                                            dev.sar
                                                .enqueue(L2Packet::pb_write(verify_ret).to_bytes());

                                            Ok(())
                                        })
                                    {
                                        let anyhow_err = anyhow_site!(
                                            "failed to enqueue auth confirm packet: {err:?}"
                                        );
                                        log::error!("{anyhow_err:?}");
                                        if let Some(waiter) = self.auth_wait.take() {
                                            let _ = waiter.send(Err(anyhow_err));
                                        }
                                    }
                                }
                                Err(err) => {
                                    log::warn!("Auth device verify failed: {err:?}");
                                    if let Some(waiter) = self.auth_wait.take() {
                                        let _ = waiter.send(Err(err));
                                    }
                                }
                            },
                            pb::xiaomi::protocol::account::Payload::AuthDeviceConfirm(_dc) => {
                                let update_res = FastLane::with_component_mut::<AuthComponent, (), _>(
                                    this,
                                    AuthComponent::ID,
                                    |comp| {
                                        comp.is_authed = true;
                                    },
                                );

                                match update_res {
                                    Ok(_) => {
                                        if let Some(aw_sender) = self.auth_wait.take() {
                                            if let Err(err) = aw_sender.send(Ok(())) {
                                                log::debug!(
                                                    "Auth completion receiver dropped before delivery: {:?}",
                                                    err
                                                );
                                            }
                                        } else {
                                            log::debug!(
                                                "AuthDeviceConfirm received but no pending waiter present"
                                            );
                                        }
                                    }
                                    Err(err) => {
                                        let anyhow_err = anyhow_site!(
                                            "failed to mark auth component as authed: {err:?}"
                                        );
                                        log::error!("{anyhow_err:?}");
                                        if let Some(waiter) = self.auth_wait.take() {
                                            let _ = waiter.send(Err(anyhow_err));
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl_has_sys_meta!(AuthSystem, meta);

#[derive(serde::Serialize)]
pub struct AuthComponent {
    #[serde(skip_serializing)]
    meta: LogicCompMeta,
    pub authkey: String,
    pub is_authed: bool,
    pub random_bytes: Vec<u8>,
    pub enc_key: Vec<u8>,
    pub dec_key: Vec<u8>,
    pub enc_nonce: Vec<u8>,
    pub dec_nonce: Vec<u8>,
}

impl AuthComponent {
    pub const ID: &'static str = "MiWearDeviceAuthLogicComponent";
    pub fn new(authkey: String) -> Self {
        Self {
            meta: LogicCompMeta::new::<AuthSystem>(Self::ID),
            authkey,
            is_authed: false,
            random_bytes: vec![],
            enc_key: vec![],
            dec_key: vec![],
            enc_nonce: vec![],
            dec_nonce: vec![],
        }
    }
}

impl_logic_component!(AuthComponent, meta);

fn build_auth_step_1(nonce: &[u8]) -> pb::xiaomi::protocol::WearPacket {
    let account_payload = pb::xiaomi::protocol::auth::AppVerify {
        app_random: nonce.to_vec(),
        app_device_id: None,
        check_dynamic_code: None,
    };

    let pkt_payload = pb::xiaomi::protocol::Account {
        payload: Some(pb::xiaomi::protocol::account::Payload::AuthAppVerify(
            account_payload,
        )),
    };

    let pkt = pb::xiaomi::protocol::WearPacket {
        r#type: pb::xiaomi::protocol::wear_packet::Type::Account as i32,
        id: pb::xiaomi::protocol::account::AccountId::AuthVerify as u32,
        payload: Some(pb::xiaomi::protocol::wear_packet::Payload::Account(
            pkt_payload,
        )),
    };

    pkt
}

fn build_auth_step_2(
    this: &mut (dyn System + 'static),
    device_verify: &pb::xiaomi::protocol::auth::DeviceVerify,
) -> anyhow::Result<pb::xiaomi::protocol::WearPacket> {
    let w_random = device_verify.device_random.clone();
    let w_sign = device_verify.device_sign.clone();

    if w_random.len() != 16 || w_sign.len() != 32 {
        return Err(anyhow_site!("nonce/hmac length mismatch"));
    }

    let authkey =
        FastLane::with_component_mut::<AuthComponent, String, _>(this, AuthComponent::ID, |comp| {
            comp.authkey.clone()
        })
        .unwrap();

    let (force_android, connect_type) =
        FastLane::with_entity_mut::<(bool, crate::device::xiaomi::ConnectType), _>(this, |ent| {
            let dev = ent.as_any_mut().downcast_mut::<XiaomiDevice>().unwrap();
            Ok((dev.force_android.clone(), dev.connect_type.clone()))
        })
        .unwrap();

    let p_random_vec = FastLane::with_component_mut::<AuthComponent, Vec<u8>, _>(
        this,
        AuthComponent::ID,
        |comp| comp.random_bytes.clone(),
    )
    .unwrap();
    if p_random_vec.len() != 16 {
        return Err(anyhow_site!("phone nonce length mismatch"));
    }

    let block64 = kdf_miwear(
        &string_to_u8_16(&authkey).ok_or_else(|| anyhow_site!("invalid authkey hex len"))?,
        (&p_random_vec[..]).try_into().unwrap(), // &[u8;16]
        (&w_random[..]).try_into().unwrap(),     // &[u8;16]
    );

    let dec_key: Vec<u8> = block64[0..16].to_vec();
    let enc_key: Vec<u8> = block64[16..32].to_vec();
    let dec_nonce: Vec<u8> = block64[32..36].to_vec();
    let enc_nonce: Vec<u8> = block64[36..40].to_vec();

    use hmac::Mac as _;
    let mut mac = Hmac::<Sha256>::new_from_slice(&dec_key).unwrap();
    mac.update(&w_random);
    mac.update(&p_random_vec);
    let expect = mac.finalize().into_bytes();
    if w_sign.as_slice() != &expect[..] {
        return Err(anyhow_site!(
            "Auth HMAC mismatch, This usually means your AuthKey is wrong."
        ));
    }

    // encryptedSigns (HMAC) ---
    let mut mac2 = Hmac::<Sha256>::new_from_slice(&enc_key).unwrap();
    mac2.update(&p_random_vec);
    mac2.update(&w_random);
    let encrypted_signs = mac2.finalize().into_bytes().to_vec(); // 32 B

    // 设备类型
    let mut device_type = pb::xiaomi::protocol::companion_device::DeviceType::Android as i32;
    if connect_type == ConnectType::BLE && !force_android {
        device_type = pb::xiaomi::protocol::companion_device::DeviceType::Ios as i32;
    }

    let proto_companion_device = pb::xiaomi::protocol::CompanionDevice {
        device_type,
        system_version: None,
        device_name: "AstroBox".to_string(),
        app_capability: Some(0xffff_ffff),
        region: None,
        server_prefix: None,
    };

    let companion_device = proto_companion_device.encode_to_vec();

    // 构造 12B nonce：4B enc_nonce + 8B counter(全0)
    let mut pkt_nonce = Vec::with_capacity(12);
    pkt_nonce.extend_from_slice(&enc_nonce);
    pkt_nonce.extend_from_slice(&0u32.to_le_bytes()); // counterHi
    pkt_nonce.extend_from_slice(&0u32.to_le_bytes()); // counterLo

    let enc_key_arr: &[u8; 16] = (&enc_key[..]).try_into().unwrap();
    let nonce_arr: &[u8; 12] = (&pkt_nonce[..]).try_into().unwrap();

    let encrypted_device_info = aes128_ccm_encrypt(enc_key_arr, nonce_arr, &[], &companion_device);

    let account_payload = pb::xiaomi::protocol::auth::AppConfirm {
        app_sign: encrypted_signs,
        encrypt_companion_device: encrypted_device_info,
    };

    let pkt_payload = pb::xiaomi::protocol::Account {
        payload: Some(pb::xiaomi::protocol::account::Payload::AuthAppConfirm(
            account_payload,
        )),
    };

    let pkt = pb::xiaomi::protocol::WearPacket {
        r#type: pb::xiaomi::protocol::wear_packet::Type::Account as i32,
        id: pb::xiaomi::protocol::account::AccountId::AuthConfirm as u32,
        payload: Some(pb::xiaomi::protocol::wear_packet::Payload::Account(
            pkt_payload,
        )),
    };

    let enc_key_to_set = enc_key;
    let dec_key_to_set = dec_key;
    let enc_nonce_to_set = enc_nonce;
    let dec_nonce_to_set = dec_nonce;

    FastLane::with_component_mut::<AuthComponent, (), _>(this, AuthComponent::ID, move |comp| {
        comp.enc_key = enc_key_to_set;
        comp.dec_key = dec_key_to_set;
        comp.enc_nonce = enc_nonce_to_set;
        comp.dec_nonce = dec_nonce_to_set;
    })
    .unwrap();

    Ok(pkt)
}

fn string_to_u8_16(s: &String) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }

    let mut result = [0u8; 16];
    for i in 0..16 {
        let byte_str = &s[i * 2..i * 2 + 2];
        match u8::from_str_radix(byte_str, 16) {
            Ok(val) => result[i] = val,
            Err(_) => return None,
        }
    }
    Some(result)
}

fn kdf_miwear(secret_key: &[u8; 16], phone_nonce: &[u8; 16], watch_nonce: &[u8; 16]) -> [u8; 64] {
    type HmacSha256 = Hmac<Sha256>;

    // 1) hmac_key = HMAC(init_key, secret_key)
    let mut init_key = [0u8; 32];
    init_key[..16].copy_from_slice(phone_nonce);
    init_key[16..].copy_from_slice(watch_nonce);

    let mut mac = HmacSha256::new_from_slice(&init_key).expect("HMAC key length fixed");
    mac.update(secret_key);
    let hmac_key = mac.finalize().into_bytes(); // 32 B

    // 2) expand to 64 B
    let mut okm = [0u8; 64];
    let tag = b"miwear-auth";
    let mut offset = 0;
    let mut prev: Vec<u8> = Vec::new();
    for counter in 1u8..=3 {
        // 3*32 >= 64
        let mut mac = HmacSha256::new_from_slice(&hmac_key).unwrap();
        mac.update(&prev);
        mac.update(tag);
        mac.update(&[counter]);
        prev = mac.finalize().into_bytes().to_vec();
        let end = (offset + 32).min(64);
        okm[offset..end].copy_from_slice(&prev[..end - offset]);
        offset = end;
    }
    okm
}
