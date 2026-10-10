use anyhow::{Context, bail, ensure};
use hmac::{Hmac, Mac};
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToEncodedPoint};
use pb::xiaomi::protocol::{self as proto, account::AccountId, account::Payload};
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::crypto::aesccm::aes128_ccm_encrypt;
use crate::device::xiaomi::XiaomiDevice;
use crate::device::xiaomi::components::auth::AuthComponent;
use crate::device::xiaomi::packet::v2::layer2::L2Packet;
use crate::device::xiaomi::system::{L2PbExt, register_xiaomi_system_ext_on_l2packet};
use crate::ecs::{Component, access::with_device_component_mut};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LocalBindConfig {
    pub user_id: String,
    pub app_device_id: String,
}

impl LocalBindConfig {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.user_id.is_empty() && self.user_id.len() <= 64, "User ID must contain 1..64 bytes");
        ensure!(!self.app_device_id.is_empty() && self.app_device_id.len() <= 64, "App device ID must contain 1..64 bytes");
        ensure!(!self.user_id.contains('\0') && !self.app_device_id.contains('\0'), "Binding identifiers cannot contain NUL");
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct XiaomiConnectOptions {
    pub local_bind: Option<LocalBindConfig>,
    pub app_device_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Info,
    Verify,
    Confirm,
    Result,
}

struct Exchange {
    secret: SecretKey,
    app_random: [u8; 16],
    keys: Option<[u8; 64]>,
}

impl Exchange {
    fn new() -> anyhow::Result<Self> {
        let secret = loop {
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("Random source failed: {e}"))?;
            if let Ok(secret) = SecretKey::from_slice(&bytes) {
                break secret;
            }
        };
        let mut app_random = [0u8; 16];
        getrandom::getrandom(&mut app_random).map_err(|e| anyhow::anyhow!("Random source failed: {e}"))?;
        Ok(Self { secret, app_random, keys: None })
    }

    fn public_key(&self) -> Vec<u8> {
        self.secret.public_key().to_encoded_point(false).as_bytes()[1..].to_vec()
    }

    fn verify(&mut self, verify: proto::bind_local::DeviceVerify) -> anyhow::Result<proto::bind_local::AppConfirm> {
        ensure!(verify.device_public_key.len() == 64, "Invalid device P-256 public key length");
        ensure!(verify.device_random.len() == 16 && verify.device_sign.len() == 32, "Invalid device nonce/signature length");
        let mut encoded = vec![4];
        encoded.extend_from_slice(&verify.device_public_key);
        let public = PublicKey::from_sec1_bytes(&encoded).context("Invalid device P-256 point")?;
        let shared = diffie_hellman(self.secret.to_nonzero_scalar(), public.as_affine());
        let mut mac = Hmac::<Sha256>::new_from_slice(shared.raw_secret_bytes()).unwrap();
        mac.update(&verify.device_random);
        mac.verify_slice(&verify.device_sign).context("Device binding signature mismatch")?;
        let mut mac = Hmac::<Sha256>::new_from_slice(shared.raw_secret_bytes()).unwrap();
        mac.update(&self.app_random);
        let mut salt = self.app_random.to_vec();
        salt.extend_from_slice(&verify.device_random);
        let mut keys = [0u8; 64];
        hkdf::Hkdf::<Sha256>::new(Some(&salt), shared.raw_secret_bytes())
            .expand(b"miwear-bind", &mut keys).map_err(|_| anyhow::anyhow!("Binding key derivation failed"))?;
        self.keys = Some(keys);
        Ok(proto::bind_local::AppConfirm {
            app_random: self.app_random.to_vec(),
            app_sign: mac.finalize().into_bytes().to_vec(),
        })
    }

    fn result(&self, config: &LocalBindConfig) -> proto::BindResultV2 {
        let keys = self.keys.as_ref().unwrap();
        let info = proto::bind_local::ResultInfo {
            user_id: config.user_id.clone(),
            companion_device: proto::CompanionDevice {
                device_type: proto::companion_device::DeviceType::Android as i32,
                system_version: None,
                device_name: "AstroBox".into(),
                app_capability: Some(u32::MAX),
                region: None,
                server_prefix: None,
            },
        };
        // LOCAL BIND uses this protocol nonce/AAD, not the transport packet counter.
        let nonce = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b];
        proto::BindResultV2 {
            encrypt_result_info: aes128_ccm_encrypt(
                keys[16..32].try_into().unwrap(), &nonce, b"bind-data", &info.encode_to_vec(),
            ),
        }
    }
}

#[derive(Component)]
pub struct BindSystem {
    owner_id: String,
    config: LocalBindConfig,
    stage: Stage,
    exchange: Option<Exchange>,
    waiter: Option<oneshot::Sender<anyhow::Result<()>>>,
}

impl BindSystem {
    pub fn new(owner_id: String, config: LocalBindConfig) -> Self {
        register_xiaomi_system_ext_on_l2packet::<Self>();
        Self { owner_id, config, stage: Stage::Idle, exchange: None, waiter: None }
    }

    pub fn prepare_bind(&mut self) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        self.config.validate()?;
        ensure!(self.stage == Stage::Idle, "Binding already in progress");
        self.exchange = Some(Exchange::new()?);
        let (tx, rx) = oneshot::channel();
        self.waiter = Some(tx);
        self.stage = Stage::Info;
        if let Err(err) = self.send(AccountId::BindStartV2, Payload::BindStartV2(proto::BindStartV2 {
            check_dynamic_code: false,
            hash_user_id: md5::Md5::digest(self.config.user_id.as_bytes()).to_vec(),
            device_name: "AstroBox".into(),
            pid: None,
        })) {
            self.finish(Err(err));
        }
        Ok(rx)
    }

    fn send(&self, id: AccountId, payload: Payload) -> anyhow::Result<()> {
        let packet = proto::WearPacket {
            r#type: proto::wear_packet::Type::Account as i32,
            id: id as u32,
            payload: Some(proto::wear_packet::Payload::Account(proto::Account { payload: Some(payload) })),
        };
        with_device_component_mut::<XiaomiDevice, _, _>(self.owner_id.clone(), move |dev| {
            dev.sar.lock().enqueue(L2Packet::pb_write(packet).to_bytes());
        }).map_err(|err| anyhow::anyhow!("Could not send binding packet: {err:?}"))
    }

    fn finish(&mut self, result: anyhow::Result<()>) {
        self.stage = Stage::Idle;
        self.exchange = None;
        if let Some(tx) = self.waiter.take() {
            let _ = tx.send(result);
        }
    }

    fn receive(&mut self, id: u32, payload: Payload) -> anyhow::Result<()> {
        match (self.stage, id, payload) {
            (Stage::Info, 17, Payload::BindInfoV2(info)) => {
                ensure!(info.verify_mode == proto::VerifyMode::AppLocal as i32, "Device requires server PSK binding, not local binding");
                ensure!(matches!(proto::OobMode::try_from(info.oob_mode), Ok(proto::OobMode::NoOob | proto::OobMode::ButtonConfirm)), "Device requires an unsupported out-of-band binding method");
                self.stage = Stage::Verify;
                log::info!("[XiaomiDevice.Bind] Confirm the binding request on the device");
                self.send(AccountId::BindVerify, Payload::LocalAppVerify(proto::bind_local::AppVerify {
                    app_device_id: self.config.app_device_id.clone(),
                    app_public_key: self.exchange.as_ref().unwrap().public_key(),
                }))?;
            }
            (Stage::Verify, 18, Payload::LocalDeviceVerify(verify)) => {
                let confirm = self.exchange.as_mut().unwrap().verify(verify)?;
                self.stage = Stage::Confirm;
                self.send(AccountId::BindConfirm, Payload::LocalAppConfirm(confirm))?;
            }
            (Stage::Confirm, 19, Payload::LocalDeviceConfirm(confirm)) => {
                ensure!(confirm.confirm_result, "Device declined local binding");
                self.stage = Stage::Result;
                self.send(AccountId::BindResultV2, Payload::BindResultV2(self.exchange.as_ref().unwrap().result(&self.config)))?;
            }
            (Stage::Result, 25, Payload::ErrorCode(0)) => {
                let keys = self.exchange.as_ref().unwrap().keys.unwrap();
                let app_device_id = self.config.app_device_id.clone();
                with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), move |auth| {
                    auth.authkey = hex::encode(&keys[40..56]);
                    auth.app_device_id = Some(app_device_id);
                    auth.dec_key = keys[..16].to_vec();
                    auth.enc_key = keys[16..32].to_vec();
                    auth.dec_nonce = keys[32..36].to_vec();
                    auth.enc_nonce = keys[36..40].to_vec();
                    auth.is_authed = true;
                }).map_err(|err| anyhow::anyhow!("Could not install binding keys: {err:?}"))?;
                log::info!("[XiaomiDevice.Bind] Local binding completed");
                self.finish(Ok(()));
            }
            (_, 17 | 18 | 19 | 25, Payload::ErrorCode(code)) => bail!("Device rejected local binding: account error {code}"),
            _ => {}
        }
        Ok(())
    }
}

impl L2PbExt for BindSystem {
    fn on_pb_packet(&mut self, packet: proto::WearPacket) {
        if !self.waiter.as_ref().is_some_and(|w| !w.is_closed()) {
            return;
        }
        if let Some(proto::wear_packet::Payload::Account(proto::Account { payload: Some(payload) })) = packet.payload {
            if let Err(err) = self.receive(packet.id, payload) {
                self.finish(Err(err));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aesccm::aes128_ccm_decrypt;

    #[test]
    fn local_bind_key_agreement_and_result() {
        let mut app = Exchange::new().unwrap();
        let device = Exchange::new().unwrap();
        let shared = diffie_hellman(device.secret.to_nonzero_scalar(), app.secret.public_key().as_affine());
        let mut mac = Hmac::<Sha256>::new_from_slice(shared.raw_secret_bytes()).unwrap();
        mac.update(&device.app_random);
        let verify = proto::bind_local::DeviceVerify {
            device_public_key: device.public_key(), device_random: device.app_random.to_vec(),
            device_sign: mac.finalize().into_bytes().to_vec(),
        };
        let confirm = app.verify(verify.clone()).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(shared.raw_secret_bytes()).unwrap();
        mac.update(&confirm.app_random);
        mac.verify_slice(&confirm.app_sign).unwrap();
        let config = LocalBindConfig { user_id: "emulator".into(), app_device_id: "astrobox-test".into() };
        let result = app.result(&config);
        let nonce = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b];
        let plain = aes128_ccm_decrypt(app.keys.unwrap()[16..32].try_into().unwrap(), &nonce, b"bind-data", &result.encrypt_result_info).unwrap();
        assert_eq!(proto::bind_local::ResultInfo::decode(plain.as_slice()).unwrap().user_id, "emulator");
        let mut bad = verify;
        bad.device_sign[0] ^= 1;
        assert!(app.verify(bad.clone()).is_err());
        bad.device_public_key.clear();
        assert!(app.verify(bad).is_err());
    }
}
