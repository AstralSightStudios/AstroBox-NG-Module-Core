use std::{future::Future, pin::Pin, sync::Arc};

use parking_lot::Mutex as ParkingMutex;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::{
    device::{Device, DeviceKind},
    ecs::Component,
};

pub mod components;
pub mod crypto;
pub mod packet;
pub mod system;
pub mod transport;

use transport::{
    ble::split_v2_pdu_for_ble,
    vscp::{VscpMessage, VscpSar, VscpSarConfig},
};

pub type VivoProtocolResult<T> = std::result::Result<T, VivoProtocolError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VivoProtocolError {
    InvalidFrame(&'static str),
    LengthMismatch { declared: usize, actual: usize },
    CrcMismatch { declared: u16, computed: u16 },
    UnsupportedVersion(u8),
    Crypto(&'static str),
    IntConversion,
}

impl fmt::Display for VivoProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrame(msg) => write!(f, "invalid vivo VSCP frame: {msg}"),
            Self::LengthMismatch { declared, actual } => {
                write!(
                    f,
                    "vivo VSCP length mismatch: declared {declared}, actual {actual}"
                )
            }
            Self::CrcMismatch { declared, computed } => {
                write!(
                    f,
                    "vivo VSCP CRC mismatch: declared {declared:#06x}, computed {computed:#06x}"
                )
            }
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported vivo VSCP version: {version}")
            }
            Self::Crypto(msg) => write!(f, "vivo crypto error: {msg}"),
            Self::IntConversion => write!(f, "vivo integer conversion failed"),
        }
    }
}

impl std::error::Error for VivoProtocolError {}

impl From<std::num::TryFromIntError> for VivoProtocolError {
    fn from(_: std::num::TryFromIntError) -> Self {
        Self::IntConversion
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum VivoConnectType {
    SPP = 0,
    BLE = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VivoBindStartType {
    Bind,
    UserConnect,
    AutoConnect,
    ReconnInner,
    ReconnBackup,
}

impl Default for VivoBindStartType {
    fn default() -> Self {
        Self::UserConnect
    }
}

#[derive(Debug)]
pub enum SendError {
    Disconnected,
    Io(String),
}

type SendFuture = Pin<Box<dyn Future<Output = Result<(), SendError>> + Send>>;
type SendFn = Arc<dyn Fn(Vec<Vec<u8>>) -> SendFuture + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VivoDeviceConfig {
    pub open_id: String,
    pub phone_device_id: String,
    pub app_version: String,
    pub phone_model: String,
    pub bind_start_type: VivoBindStartType,
    pub has_backup_list: bool,
    pub magic_phone: Option<String>,
    pub product_series_type: Option<i32>,
    pub ble_att_mtu: usize,
    pub vscp: VscpSarConfig,
}

impl Default for VivoDeviceConfig {
    fn default() -> Self {
        Self {
            open_id: String::new(),
            phone_device_id: "astrobox-vivo-phone".to_string(),
            app_version: "1".to_string(),
            phone_model: "AstroBox".to_string(),
            bind_start_type: VivoBindStartType::default(),
            has_backup_list: false,
            magic_phone: None,
            product_series_type: None,
            ble_att_mtu: 247,
            vscp: VscpSarConfig::default(),
        }
    }
}

#[derive(Component, Serialize)]
pub struct VivoDevice {
    #[serde(flatten)]
    device: Device,
    pub connect_type: VivoConnectType,
    pub config: VivoDeviceConfig,
    #[serde(skip_serializing)]
    sender: SendFn,
    #[serde(skip_serializing)]
    pub sar: ParkingMutex<VscpSar>,
}

pub fn cleanup_cached_state(_device_id: &str) {}

impl VivoDevice {
    pub fn new<F, Fut>(
        name: String,
        addr: String,
        connect_type: VivoConnectType,
        config: VivoDeviceConfig,
        sender: F,
    ) -> Self
    where
        F: Fn(Vec<Vec<u8>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), SendError>> + Send + 'static,
    {
        let sender: SendFn = Arc::new(move |data| Box::pin(sender(data)));
        let sar = VscpSar::new(config.vscp.clone());
        Self {
            device: Device::new(name, addr, DeviceKind::Vivo),
            connect_type,
            config,
            sender,
            sar: ParkingMutex::new(sar),
        }
    }

    pub fn encode_message_packets(&self, message: VscpMessage) -> Result<Vec<Vec<u8>>, SendError> {
        let (frames, pack_size, max_data_length) = {
            let mut sar = self.sar.lock();
            let pack_size = sar.config().pack_size;
            let max_data_length = sar.config().max_data_length;
            let frames = sar
                .encode_message(&message)
                .map_err(|err| SendError::Io(err.to_string()))?;
            (frames, pack_size, max_data_length)
        };

        let mut packets = Vec::new();
        let frame_count = frames.len();
        for frame in frames {
            match self.connect_type {
                VivoConnectType::SPP => packets.push(frame.into_bytes()),
                VivoConnectType::BLE => {
                    let chunks = split_v2_pdu_for_ble(frame.bytes(), self.config.ble_att_mtu)
                        .map_err(|err| SendError::Io(err.to_string()))?;
                    packets.extend(chunks);
                }
            }
        }

        let packet_lens = packets.iter().map(Vec::len).collect::<Vec<_>>();
        log::info!(
            "[VivoDevice.Transport] TX addr={} bid={} cid={} encrypted={} payload_len={} connect_type={:?} vscp_pack_size={} vscp_max_data_length={} frame_count={} packet_count={} packet_lens={:?}",
            self.addr(),
            message.bid,
            message.cid,
            message.encrypted,
            message.payload.len(),
            self.connect_type,
            pack_size,
            max_data_length,
            frame_count,
            packets.len(),
            packet_lens
        );

        Ok(packets)
    }

    pub(in crate::device::vivo) fn transport_send_parts(
        &self,
        message: VscpMessage,
    ) -> Result<(SendFn, Vec<Vec<u8>>), SendError> {
        let packets = self.encode_message_packets(message)?;
        Ok((self.sender.clone(), packets))
    }

    pub async fn send_message(&self, message: VscpMessage) -> Result<(), SendError> {
        let (sender, packets) = self.transport_send_parts(message)?;
        (sender)(packets).await
    }

    pub fn update_vscp_limits(&self, pack_size: usize, max_data_length: usize) {
        self.sar.lock().update_config(VscpSarConfig {
            pack_size,
            max_data_length,
        });
    }

    pub fn on_transport_data(&self, data: &[u8]) -> VivoProtocolResult<Vec<VscpMessage>> {
        let messages = self.sar.lock().push_bytes(data)?;
        if messages.is_empty() {
            log::debug!(
                "[VivoDevice.Transport] RX addr={} raw_len={} buffered/no complete message yet",
                self.addr(),
                data.len()
            );
        } else {
            for message in &messages {
                log::info!(
                    "[VivoDevice.Transport] RX addr={} bid={} cid={} encrypted={} payload_len={}",
                    self.addr(),
                    message.bid,
                    message.cid,
                    message.encrypted,
                    message.payload.len()
                );
            }
        }
        Ok(messages)
    }

    pub fn base(&self) -> &Device {
        &self.device
    }

    pub fn name(&self) -> &str {
        self.device.name()
    }

    pub fn addr(&self) -> &str {
        self.device.addr()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        crypto::{bind_aes_sign, verify_bind_aes_sign},
        transport::{
            ble::split_v2_pdu_for_ble,
            vscp::{VscpMessage, VscpSar, VscpSarConfig, encode_message_with_identifier},
        },
    };

    #[test]
    fn bind_aes_sign_verifies() {
        let sign = bind_aes_sign("SN123", 0x2211).unwrap();

        assert!(verify_bind_aes_sign("SN123", 0x2211, &sign).unwrap());
        assert!(!verify_bind_aes_sign("SN124", 0x2211, &sign).unwrap());
    }

    #[test]
    fn vscp_v2_sar_roundtrips_fragmented_plaintext() {
        let config = VscpSarConfig {
            pack_size: 18,
            max_data_length: 64,
        };
        let message = VscpMessage::new(15, 2, b"hello-vivo-msgpack".to_vec());
        let frames = encode_message_with_identifier(&config, 0x42, &message).unwrap();

        assert!(frames.len() > 1);

        let mut sar = VscpSar::new(config);
        let mut decoded = Vec::new();
        for frame in frames {
            for chunk in frame.bytes().chunks(3) {
                decoded.extend(sar.push_bytes(chunk).unwrap());
            }
        }

        assert_eq!(decoded, vec![message]);
    }

    #[test]
    fn vscp_v2_encrypted_message_roundtrips() {
        let config = VscpSarConfig {
            pack_size: 32,
            max_data_length: 96,
        };
        let message = VscpMessage::new(47, 1, b"secret-cloud-payload".to_vec()).encrypted(true);
        let frames = encode_message_with_identifier(&config, 7, &message).unwrap();

        assert!(frames[0].encrypted().unwrap());

        let mut sar = VscpSar::new(config);
        let mut decoded = Vec::new();
        for frame in frames {
            decoded.extend(sar.push_bytes(frame.bytes()).unwrap());
        }

        assert_eq!(decoded, vec![message]);
    }

    #[test]
    fn ble_v2_split_has_no_bear_flag_byte() {
        let config = VscpSarConfig::default();
        let message = VscpMessage::new(15, 1, vec![0x55; 400]);
        let frames = encode_message_with_identifier(&config, 1, &message).unwrap();
        let chunks = split_v2_pdu_for_ble(frames[0].bytes(), 247).unwrap();

        assert!(chunks.iter().all(|chunk| chunk.len() <= 244));

        let flattened = chunks.into_iter().flatten().collect::<Vec<_>>();
        assert_eq!(flattened, frames[0].bytes());
    }
}
