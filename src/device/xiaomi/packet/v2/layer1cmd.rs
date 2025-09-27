use std::collections::HashMap;

pub mod config_type {
    pub const CONFIG_TYPE_VERSION: u8 = 0x01;
    pub const CONFIG_TYPE_MPS: u8 = 0x02;
    pub const CONFIG_TYPE_TX_WIN: u8 = 0x03;
    pub const CONFIG_TYPE_SEND_TIMEOUT: u8 = 0x04;
    pub const CONFIG_TYPE_DEVICE_TYPE: u8 = 0x05;
    pub const CONFIG_TYPE_DEVICE_NAME: u8 = 0x06;
    pub const CONFIG_TYPE_OS_VERSION: u8 = 0x07;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CmdCode {
    CmdL1startReq = 1,
    CmdL1startRsp = 2,
    CmdL1stopReq = 3,
    CmdL1stopRsp = 4,
    Unknown(u8),
}

impl From<u8> for CmdCode {
    fn from(v: u8) -> Self {
        match v {
            1 => CmdCode::CmdL1startReq,
            2 => CmdCode::CmdL1startRsp,
            3 => CmdCode::CmdL1stopReq,
            4 => CmdCode::CmdL1stopRsp,
            other => CmdCode::Unknown(other),
        }
    }
}

impl CmdCode {
    pub fn as_u8(&self) -> u8 {
        match *self {
            CmdCode::CmdL1startReq => 1,
            CmdCode::CmdL1startRsp => 2,
            CmdCode::CmdL1stopReq => 3,
            CmdCode::CmdL1stopRsp => 4,
            CmdCode::Unknown(v) => v,
        }
    }
}

pub struct L1CmdPacket {
    pub cmd: CmdCode,
    pub config: HashMap<u8, Vec<u8>>,
}

pub struct L1CmdBuilder {
    cmd: Option<CmdCode>,
    config: HashMap<u8, Vec<u8>>,
}

impl L1CmdBuilder {
    pub fn new() -> Self {
        Self {
            cmd: None,
            config: HashMap::new(),
        }
    }

    pub fn cmd(mut self, cmd: CmdCode) -> Self {
        self.cmd = Some(cmd);
        self
    }

    pub fn version(mut self, major: u8, minor: u8, patch: u8) -> Self {
        self.config
            .insert(config_type::CONFIG_TYPE_VERSION, vec![major, minor, patch]);
        self
    }

    pub fn mps(mut self, mps: u16) -> Self {
        self.config
            .insert(config_type::CONFIG_TYPE_MPS, mps.to_le_bytes().to_vec());
        self
    }

    pub fn tx_win(mut self, win: u16) -> Self {
        self.config
            .insert(config_type::CONFIG_TYPE_TX_WIN, win.to_le_bytes().to_vec());
        self
    }

    pub fn send_timeout(mut self, timeout_ms: u16) -> Self {
        self.config.insert(
            config_type::CONFIG_TYPE_SEND_TIMEOUT,
            timeout_ms.to_le_bytes().to_vec(),
        );
        self
    }

    pub fn device_type(mut self, dev_type: u8) -> Self {
        self.config
            .insert(config_type::CONFIG_TYPE_DEVICE_TYPE, vec![dev_type]);
        self
    }

    pub fn device_name<S: AsRef<[u8]>>(mut self, name: S) -> Self {
        self.config
            .insert(config_type::CONFIG_TYPE_DEVICE_NAME, name.as_ref().to_vec());
        self
    }

    pub fn os_version(mut self, major: u8, minor: u8, patch: u8) -> Self {
        self.config.insert(
            config_type::CONFIG_TYPE_OS_VERSION,
            vec![major, minor, patch],
        );
        self
    }

    pub fn build(self) -> Option<L1CmdPacket> {
        Some(L1CmdPacket {
            cmd: self.cmd?,
            config: self.config,
        })
    }
}

impl L1CmdPacket {
    pub fn to_payload_bytes(&self) -> Vec<u8> {
        let mut payload = vec![self.cmd.as_u8()];

        for (&key, value) in &self.config {
            payload.push(key);
            payload.extend_from_slice(&(value.len() as u16).to_le_bytes());
            payload.extend_from_slice(value);
        }
        payload
    }

    pub fn from_payload_bytes(payload: &[u8]) -> Option<Self> {
        if payload.is_empty() {
            return None;
        }
        let cmd = CmdCode::from(payload[0]);
        let mut config = HashMap::new();
        let mut i = 1;
        while i + 3 <= payload.len() {
            let key = payload[i];
            let value_size = u16::from_le_bytes([payload[i + 1], payload[i + 2]]) as usize;
            i += 3;
            if i + value_size > payload.len() {
                break;
            }
            config.insert(key, payload[i..i + value_size].to_vec());
            i += value_size;
        }
        Some(L1CmdPacket { cmd, config })
    }

    pub fn get_version(&self) -> Option<(u8, u8, u8)> {
        self.config
            .get(&config_type::CONFIG_TYPE_VERSION)
            .and_then(|v| {
                if v.len() == 3 {
                    Some((v[0], v[1], v[2]))
                } else {
                    None
                }
            })
    }

    pub fn get_mps(&self) -> Option<u16> {
        self.config
            .get(&config_type::CONFIG_TYPE_MPS)
            .and_then(|v| {
                if v.len() == 2 {
                    Some(u16::from_le_bytes([v[0], v[1]]))
                } else {
                    None
                }
            })
    }

    pub fn get_tx_win(&self) -> Option<u16> {
        self.config
            .get(&config_type::CONFIG_TYPE_TX_WIN)
            .and_then(|v| {
                if v.len() == 2 {
                    Some(u16::from_le_bytes([v[0], v[1]]))
                } else {
                    None
                }
            })
    }

    pub fn get_send_timeout(&self) -> Option<u16> {
        self.config
            .get(&config_type::CONFIG_TYPE_SEND_TIMEOUT)
            .and_then(|v| {
                if v.len() == 2 {
                    Some(u16::from_le_bytes([v[0], v[1]]))
                } else {
                    None
                }
            })
    }

    pub fn get_device_type(&self) -> Option<u8> {
        self.config
            .get(&config_type::CONFIG_TYPE_DEVICE_TYPE)
            .and_then(|v| v.get(0).copied())
    }

    pub fn get_device_name(&self) -> Option<String> {
        self.config
            .get(&config_type::CONFIG_TYPE_DEVICE_NAME)
            .and_then(|v| String::from_utf8(v.clone()).ok())
    }

    pub fn get_os_version(&self) -> Option<(u8, u8, u8)> {
        self.config
            .get(&config_type::CONFIG_TYPE_OS_VERSION)
            .and_then(|v| {
                if v.len() == 3 {
                    Some((v[0], v[1], v[2]))
                } else {
                    None
                }
            })
    }
}
