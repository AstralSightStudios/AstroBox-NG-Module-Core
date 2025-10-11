#[derive(Debug, Clone, serde::Serialize)]
pub struct TransportConfig {
    pub chunk_size_spp: usize,
    pub chunk_size_ble: usize,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            chunk_size_spp: 977,
            chunk_size_ble: 244,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SarConfig {
    pub tx_win_overrun_allowance: u8,
}

impl Default for SarConfig {
    fn default() -> Self {
        Self {
            tx_win_overrun_allowance: 2,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MassConfig {
    pub ack_wait_timeout_secs: u64,
    pub ack_poll_interval_ms: u64,
    pub ack_stall_default_ms: u64,
    pub ack_stall_min_ms: u64,
    pub ack_stall_max_ms: u64,
    pub backlog_multiplier: usize,
    pub max_batch_parts: usize,
    pub fallback_batch_parts: usize,
    pub fallback_backlog_limit: usize,
}

impl Default for MassConfig {
    fn default() -> Self {
        Self {
            ack_wait_timeout_secs: 30,
            ack_poll_interval_ms: 50,
            ack_stall_default_ms: 400,
            ack_stall_min_ms: 120,
            ack_stall_max_ms: 900,
            backlog_multiplier: 6,
            max_batch_parts: 32,
            fallback_batch_parts: 8,
            fallback_backlog_limit: 96,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResConfig {
    pub watchface_id_offset: usize,
    pub watchface_id_field_len: usize,
}

impl Default for ResConfig {
    fn default() -> Self {
        Self {
            watchface_id_offset: 34,
            watchface_id_field_len: 24,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct XiaomiDeviceConfig {
    pub transport: TransportConfig,
    pub sar: SarConfig,
    pub mass: MassConfig,
    pub res: ResConfig,
}

impl Default for XiaomiDeviceConfig {
    fn default() -> Self {
        Self {
            transport: TransportConfig::default(),
            sar: SarConfig::default(),
            mass: MassConfig::default(),
            res: ResConfig::default(),
        }
    }
}
