use std::convert::TryFrom;

use anyhow::Context;
use parking_lot::Mutex;
use tokio::{runtime::Handle, sync::oneshot};
use vivo_msgpack::{
    messages::{
        bind::{
            BID_BIND, BindAuthRequest, BindAuthResponse, BindBindRequest,
            BindConnectConfirmRequest, BindConnectConfirmResponse, BindHFPConnRequest,
            BindInitRequest, BindInitResponse, CID_AUTH, CID_BIND, CID_CONNECT_CONFIRM,
            CID_HFP_CONNECT, CID_INIT, DEFAULT_BID_VERSION,
        },
        device_info::{
            BID_DEVICE_INFO, BidVersion, CID_DEVICE_INFO, CID_WATCH_BID_VERSION, DeviceInfoRequest,
            FeatureItem, WatchBidVersionRequest, WatchFirstSyncResp,
        },
        response_cid,
    },
    msgpack::MsgpackReader,
};

use crate::{
    anyhow_site, bail_site,
    device::vivo::{
        VivoDevice, VivoDeviceConfig,
        crypto::{bind_aes_sign, verify_bind_aes_sign},
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::{Component, access::with_device_component_mut},
};

const WATCH_STATE_INIT: i32 = 0;
const WATCH_STATE_BIND: i32 = 1;
const WATCH_STATE_MID_CONN: i32 = 2;
const WATCH_STATE_MID_FACTORY: i32 = 3;
const WATCH_STATE_MID_REPAIR: i32 = 4;
const PHONE_BID_VERSION_PAIRS: &[(i32, i32)] = &[
    (1, 9),
    (2, 1),
    (3, 12),
    (4, 16),
    (5, 7),
    (6, 2),
    (7, 12),
    (8, 10),
    (9, 2),
    (10, 1),
    (11, 1),
    (12, 1),
    (13, 1),
    (14, 1),
    (15, 11),
    (16, 1),
    (17, 20),
    (18, 1),
    (38, 1),
    (19, 5),
    (20, 66),
    (21, 6),
    (22, 2),
    (23, 5),
    (24, 1),
    (25, 19),
    (26, 2),
    (27, 2),
    (28, 6),
    (29, 1),
    (31, 6),
    (33, 1),
    (34, 3),
    (35, 1),
    (36, 3),
    (47, 3),
    (48, 8),
    (61, 1),
    (62, 1),
    (63, 1),
    (64, 3),
    (65, 3),
    (24, 2),
    (30, 1),
    (39, 2),
    (40, 3),
    (69, 1),
    (49, 2),
    (50, 1),
    (51, 1),
    (54, 3),
    (41, 3),
    (42, 2),
    (102, 2),
];

#[derive(Component, serde::Serialize)]
pub struct AuthComponent {
    pub open_id: String,
    pub is_authed: bool,
    pub is_first_bind: bool,
    pub has_backup_list: bool,
    pub last_phone_random: Option<u16>,
    pub watch_sn: Option<String>,
    pub watch_random: Option<u16>,
    pub bid_version: Option<i64>,
    pub auth_seq: Option<i64>,
    pub init_seq: Option<i64>,
    pub watch_open_id: Option<String>,
    pub watch_version: Option<String>,
    pub watch_state: Option<i32>,
    pub phone_device_id: Option<String>,
    pub last_app_version: Option<String>,
    pub pack_size: Option<usize>,
    pub max_data_length: Option<usize>,
    pub magic_phone: Option<String>,
    pub phone_bid_version: Option<i32>,
    pub bid_versions: Vec<(i32, i32)>,
    pub features: Vec<(i32, String)>,
    pub watch_device_name: Option<String>,
    pub watch_ble_mac: Option<String>,
    pub watch_product_id: Option<i32>,
}

impl AuthComponent {
    pub fn new(open_id: String) -> Self {
        Self {
            open_id,
            is_authed: false,
            is_first_bind: false,
            has_backup_list: false,
            last_phone_random: None,
            watch_sn: None,
            watch_random: None,
            bid_version: None,
            auth_seq: None,
            init_seq: None,
            watch_open_id: None,
            watch_version: None,
            watch_state: None,
            phone_device_id: None,
            last_app_version: None,
            pack_size: None,
            max_data_length: None,
            magic_phone: None,
            phone_bid_version: None,
            bid_versions: Vec::new(),
            features: Vec::new(),
            watch_device_name: None,
            watch_ble_mac: None,
            watch_product_id: None,
        }
    }

    pub fn from_config(config: &VivoDeviceConfig) -> Self {
        let mut comp = Self::new(config.open_id.clone());
        comp.is_first_bind = config.is_first_bind;
        comp.has_backup_list = config.has_backup_list;
        comp.magic_phone = config.magic_phone.clone();
        comp
    }
}

#[derive(Component)]
pub struct AuthSystem {
    owner_id: String,
    tk_handle: Handle,
    auth_wait: Mutex<Option<oneshot::Sender<anyhow::Result<()>>>>,
}

impl AuthSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
            auth_wait: Mutex::new(None),
        }
    }

    pub fn prepare_auth(&mut self) -> anyhow::Result<oneshot::Receiver<anyhow::Result<()>>> {
        if self.auth_wait.lock().is_some() {
            bail_site!("vivo auth flow already in progress");
        }

        let random = generate_random_u16();
        let (open_id, magic_phone) =
            with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), move |comp| {
                comp.is_authed = false;
                comp.last_phone_random = Some(random);
                (comp.open_id.clone(), comp.magic_phone.clone())
            })
            .map_err(|err| anyhow_site!("failed to prepare vivo auth component: {err:?}"))?;

        if open_id.trim().is_empty() {
            bail_site!("vivo openId is required before auth");
        }

        let aes_sign = bind_aes_sign(&open_id, random)
            .map_err(|err| anyhow_site!("failed to build vivo bind aesSign: {err}"))?;
        let payload = BindAuthRequest {
            open_id,
            random: i32::from(random),
            aes_sign,
            bid_version: DEFAULT_BID_VERSION,
            magic_phone: magic_phone.unwrap_or_default(),
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo bind auth request: {err}"))?;

        let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
        *self.auth_wait.lock() = Some(tx);

        if let Err(err) = self.send_message(VscpMessage::new(BID_BIND, CID_AUTH, payload)) {
            self.auth_wait.lock().take();
            return Err(err.context("failed to send vivo bind auth request"));
        }

        Ok(rx)
    }

    pub async fn start_auth(&mut self) -> anyhow::Result<()> {
        let rx = self.prepare_auth()?;
        let result = rx.await.context("Vivo auth await response not received")?;
        result
    }

    fn handle_auth_response(&self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = BindAuthResponse::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo bind auth response: {err}"))?;
        if resp.code != 0 {
            bail_site!("vivo bind auth rejected by watch: code={}", resp.code);
        }

        let watch_random = u16::try_from(resp.random)
            .map_err(|_| anyhow_site!("vivo watch random out of u16 range: {}", resp.random))?;
        let sign_ok = verify_bind_aes_sign(&resp.sn, watch_random, &resp.aes_sign)
            .map_err(|err| anyhow_site!("failed to verify vivo watch aesSign: {err}"))?;
        if !sign_ok {
            bail_site!("vivo watch aesSign mismatch");
        }

        let auth_seq = resp.seq;
        let sn = resp.sn;
        let bid_version = resp.bid_version;
        with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.watch_sn = Some(sn);
            comp.watch_random = Some(watch_random);
            comp.bid_version = Some(bid_version);
            comp.auth_seq = Some(auth_seq);
        })
        .map_err(|err| anyhow_site!("failed to update vivo auth response state: {err:?}"))?;

        self.send_bind_init(auth_seq)
    }

    fn handle_init_response(&self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = BindInitResponse::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo bind init response: {err}"))?;
        if resp.code != 0 {
            bail_site!("vivo bind init rejected by watch: code={}", resp.code);
        }

        let pack_size = positive_i32_to_usize(resp.pack_size, "packSize")?;
        let max_data_length = positive_i32_to_usize(
            resp.max_data_length.unwrap_or(resp.pack_size),
            "maxDataLength",
        )?;
        let watch_open_id = normalize_watch_open_id(resp.open_id);
        let local_open_id =
            with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), |comp| {
                comp.open_id.clone()
            })
            .map_err(|err| anyhow_site!("failed to read vivo local openId: {err:?}"))?;

        ensure_watch_open_id_compatible_for_state(&local_open_id, &watch_open_id, resp.state)?;

        let init_seq = resp.seq;
        let watch_state = resp.state;
        let watch_version = resp.version;
        let phone_device_id = resp.phone_device_id;
        let last_app_version = resp.last_app_version;
        let magic_phone = resp.magic_phone;
        let watch_open_id_for_comp = watch_open_id.clone();
        with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.init_seq = Some(init_seq);
            comp.watch_open_id = Some(watch_open_id_for_comp);
            comp.watch_version = Some(watch_version);
            comp.watch_state = Some(watch_state);
            comp.phone_device_id = Some(phone_device_id);
            comp.last_app_version = Some(last_app_version);
            comp.pack_size = Some(pack_size);
            comp.max_data_length = Some(max_data_length);
            comp.magic_phone = magic_phone;
            if watch_state == WATCH_STATE_INIT {
                comp.is_first_bind = true;
            }
        })
        .map_err(|err| anyhow_site!("failed to update vivo init response state: {err:?}"))?;

        with_device_component_mut::<VivoDevice, _, _>(self.owner_id.clone(), move |dev| {
            dev.update_vscp_limits(pack_size, max_data_length);
        })
        .map_err(|err| anyhow_site!("failed to apply vivo VSCP limits: {err:?}"))?;

        match watch_state {
            WATCH_STATE_INIT => self.send_bind_confirm(true),
            WATCH_STATE_BIND
            | WATCH_STATE_MID_CONN
            | WATCH_STATE_MID_FACTORY
            | WATCH_STATE_MID_REPAIR => self.send_bid_version_sync(),
            state => bail_site!("unsupported vivo watch bind state: {}", state),
        }
    }

    fn handle_bind_response(&self, message: &VscpMessage) -> anyhow::Result<()> {
        let code = decode_response_code(&message.payload)?;
        if code != 0 {
            bail_site!("vivo bind confirm rejected by watch: code={code}");
        }

        let _init_seq =
            with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), |comp| {
                comp.init_seq
            })
            .map_err(|err| anyhow_site!("failed to read vivo init seq: {err:?}"))?
            .ok_or_else(|| anyhow_site!("vivo init seq missing before connect confirm"))?;

        self.send_bid_version_sync()
    }

    fn handle_bid_version_response(&self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = decode_watch_bid_version_response(&message.payload)?;
        if resp.code != 0 {
            bail_site!(
                "vivo BID version sync rejected by watch: code={}",
                resp.code
            );
        }

        let bid_versions_for_comp = resp
            .bid_versions
            .iter()
            .map(|item| (item.bid, item.version))
            .collect::<Vec<_>>();
        let features_for_comp = resp
            .features
            .iter()
            .map(|item| (item.key, item.value.clone()))
            .collect::<Vec<_>>();
        let device_info_bid_version = resp
            .bid_versions
            .iter()
            .find(|item| item.bid == i32::from(BID_DEVICE_INFO))
            .map(|item| item.version)
            .unwrap_or(0);
        let phone_bid_version = resp.phone_bid_version;
        with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.phone_bid_version = Some(phone_bid_version);
            comp.bid_versions = bid_versions_for_comp;
            comp.features = features_for_comp;
        })
        .map_err(|err| anyhow_site!("failed to update vivo BID version state: {err:?}"))?;

        self.send_device_info(device_info_bid_version)
    }

    fn handle_device_info_response(&self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = WatchFirstSyncResp::decode(&message.payload)
            .map_err(|err| anyhow_site!("failed to decode vivo device info response: {err}"))?;
        if resp.code != 0 {
            bail_site!("vivo device info rejected by watch: code={}", resp.code);
        }
        validate_watch_device_info(&resp)?;

        let device_name = resp.device_name;
        let ble_mac = resp.ble_mac;
        let product_id = resp.product_id;
        with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), move |comp| {
            comp.watch_device_name = Some(device_name);
            comp.watch_ble_mac = Some(ble_mac);
            comp.watch_product_id = Some(product_id);
        })
        .map_err(|err| anyhow_site!("failed to update vivo watch device info: {err:?}"))?;

        let init_seq =
            with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), |comp| {
                comp.init_seq
            })
            .map_err(|err| anyhow_site!("failed to read vivo init seq: {err:?}"))?
            .ok_or_else(|| anyhow_site!("vivo init seq missing before connect confirm"))?;

        self.send_connect_confirm(init_seq)
    }

    fn handle_connect_confirm_response(&self, message: &VscpMessage) -> anyhow::Result<()> {
        let resp = BindConnectConfirmResponse::decode(&message.payload).map_err(|err| {
            anyhow_site!("failed to decode vivo bind connect confirm response: {err}")
        })?;
        if resp.code != 0 {
            bail_site!("vivo connect confirm rejected by watch: code={}", resp.code);
        }

        with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), |comp| {
            comp.is_authed = true;
        })
        .map_err(|err| anyhow_site!("failed to mark vivo auth as complete: {err:?}"))?;

        if let Err(err) = self.send_hfp_connect_notice() {
            log::warn!("failed to send vivo HFP connect notice after auth: {err:?}");
        }

        self.complete_auth(Ok(()));
        Ok(())
    }

    fn send_bind_init(&self, auth_seq: i64) -> anyhow::Result<()> {
        let is_first_bind =
            with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), |comp| {
                comp.is_first_bind
            })
            .map_err(|err| anyhow_site!("failed to read vivo bind mode: {err:?}"))?;
        let (app_version, phone_model, phone_device_id) =
            with_device_component_mut::<VivoDevice, _, _>(self.owner_id.clone(), |dev| {
                (
                    dev.config.app_version.clone(),
                    dev.config.phone_model.clone(),
                    dev.config.phone_device_id.clone(),
                )
            })
            .map_err(|err| anyhow_site!("failed to read vivo init config: {err:?}"))?;

        let payload = BindInitRequest {
            os: 1,
            is_bind: !is_first_bind,
            version: app_version,
            module: phone_model.chars().take(32).collect(),
            phone_device_id,
            seq: auth_seq + 21,
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo bind init request: {err}"))?;

        self.send_message(VscpMessage::new(BID_BIND, CID_INIT, payload))
    }

    fn send_bind_confirm(&self, confirm: bool) -> anyhow::Result<()> {
        let payload = BindBindRequest { confirm }
            .payload()
            .map_err(|err| anyhow_site!("failed to encode vivo bind confirm request: {err}"))?;
        self.send_message(VscpMessage::new(BID_BIND, CID_BIND, payload))
    }

    fn send_bid_version_sync(&self) -> anyhow::Result<()> {
        let product_series_type =
            with_device_component_mut::<VivoDevice, _, _>(self.owner_id.clone(), |dev| {
                dev.config.product_series_type
            })
            .map_err(|err| anyhow_site!("failed to read vivo product series type: {err:?}"))?;
        let payload = WatchBidVersionRequest {
            sync_bid_versions: true,
            sync_features: true,
            phone_bid_version: 0,
            bid_versions: default_phone_bid_versions(product_series_type),
            features: None,
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo BID version sync request: {err}"))?;
        self.send_message(VscpMessage::new(
            BID_DEVICE_INFO,
            CID_WATCH_BID_VERSION,
            payload,
        ))
    }

    fn send_device_info(&self, device_info_bid_version: i32) -> anyhow::Result<()> {
        let (unix_time_sec, millis) = current_unix_time_parts()?;
        let timezone_offset_sec = local_timezone_offset_sec();
        let timezone_offset_field = if device_info_bid_version > 1 {
            Some(timezone_offset_sec)
        } else {
            None
        };
        let payload = DeviceInfoRequest {
            unix_time_sec,
            gmt_timezone: gmt_timezone_from_offset(timezone_offset_sec),
            os_type: 1,
            millis,
            timezone_offset_sec: timezone_offset_field,
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo device info request: {err}"))?;
        self.send_message(VscpMessage::new(BID_DEVICE_INFO, CID_DEVICE_INFO, payload))
    }

    fn send_connect_confirm(&self, init_seq: i64) -> anyhow::Result<()> {
        let has_backup_list =
            with_device_component_mut::<AuthComponent, _, _>(self.owner_id.clone(), |comp| {
                comp.has_backup_list
            })
            .map_err(|err| anyhow_site!("failed to read vivo backup-list flag: {err:?}"))?;

        let payload = BindConnectConfirmRequest {
            seq: init_seq + 61,
            has_backup_list,
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo connect confirm request: {err}"))?;
        self.send_message(VscpMessage::new(BID_BIND, CID_CONNECT_CONFIRM, payload))
    }

    fn send_hfp_connect_notice(&self) -> anyhow::Result<()> {
        let payload = BindHFPConnRequest {}
            .payload()
            .map_err(|err| anyhow_site!("failed to encode vivo HFP connect notice: {err}"))?;
        self.send_message(VscpMessage::new(BID_BIND, CID_HFP_CONNECT, payload))
    }

    fn send_message(&self, message: VscpMessage) -> anyhow::Result<()> {
        let send_parts =
            with_device_component_mut::<VivoDevice, _, _>(self.owner_id.clone(), move |dev| {
                dev.transport_send_parts(message)
            })
            .map_err(|err| anyhow_site!("failed to prepare vivo transport send: {err:?}"))?;
        let (sender, packets) =
            send_parts.map_err(|err| anyhow_site!("failed to encode vivo message: {err:?}"))?;
        self.tk_handle
            .block_on(async move { (sender)(packets).await })
            .map_err(|err| anyhow_site!("failed to send vivo message: {err:?}"))
    }

    fn complete_auth(&self, result: anyhow::Result<()>) {
        if let Some(waiter) = self.auth_wait.lock().take() {
            if let Err(err) = waiter.send(result) {
                log::debug!("Vivo auth completion receiver dropped before delivery: {err:?}");
            }
        } else if let Err(err) = result {
            log::warn!("Vivo auth flow failed without a pending waiter: {err:?}");
        }
    }
}

impl VivoSystemExt for AuthSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_BIND && message.bid != BID_DEVICE_INFO {
            return;
        }

        let result = match message.cid {
            _ if message.bid == BID_BIND && message.cid == BindAuthResponse::CID => {
                self.handle_auth_response(message)
            }
            _ if message.bid == BID_BIND && message.cid == BindInitResponse::CID => {
                self.handle_init_response(message)
            }
            _ if message.bid == BID_BIND && message.cid == response_cid(CID_BIND) => {
                self.handle_bind_response(message)
            }
            _ if message.bid == BID_BIND && message.cid == BindConnectConfirmResponse::CID => {
                self.handle_connect_confirm_response(message)
            }
            _ if message.bid == BID_DEVICE_INFO
                && message.cid == response_cid(CID_WATCH_BID_VERSION) =>
            {
                self.handle_bid_version_response(message)
            }
            _ if message.bid == BID_DEVICE_INFO && message.cid == WatchFirstSyncResp::CID => {
                self.handle_device_info_response(message)
            }
            _ => Ok(()),
        };

        if let Err(err) = result {
            log::warn!("[VivoDevice] auth flow failed: {err:?}");
            self.complete_auth(Err(err));
        }
    }
}

fn generate_random_u16() -> u16 {
    let bytes = crate::tools::generate_random_bytes(2);
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn positive_i32_to_usize(value: i32, field: &str) -> anyhow::Result<usize> {
    if value <= 0 {
        bail_site!("invalid vivo {field}: {value}");
    }
    usize::try_from(value).map_err(|_| anyhow_site!("invalid vivo {field}: {value}"))
}

fn normalize_watch_open_id(open_id: String) -> String {
    if open_id.starts_with("000000") {
        String::new()
    } else {
        open_id
    }
}

fn is_watch_open_id_compatible_for_state(
    local_open_id: &str,
    watch_open_id: &str,
    watch_state: i32,
) -> bool {
    match watch_state {
        WATCH_STATE_INIT | WATCH_STATE_MID_FACTORY => {
            watch_open_id.is_empty() || watch_open_id == local_open_id
        }
        WATCH_STATE_BIND | WATCH_STATE_MID_CONN | WATCH_STATE_MID_REPAIR => {
            watch_open_id == local_open_id
        }
        _ => true,
    }
}

fn ensure_watch_open_id_compatible_for_state(
    local_open_id: &str,
    watch_open_id: &str,
    watch_state: i32,
) -> anyhow::Result<()> {
    if is_watch_open_id_compatible_for_state(local_open_id, watch_open_id, watch_state) {
        return Ok(());
    }
    match watch_state {
        WATCH_STATE_MID_FACTORY => {
            bail_site!("vivo watch openId does not match local openId; refusing lock-mode path")
        }
        WATCH_STATE_MID_REPAIR => {
            bail_site!(
                "vivo watch openId is not reusable in repair state; refusing bind reset path"
            )
        }
        _ => bail_site!("vivo watch openId does not match local openId; refusing bind reset path"),
    }
}

fn validate_watch_device_info(resp: &WatchFirstSyncResp) -> anyhow::Result<()> {
    if resp.mac.is_empty() {
        bail_site!("vivo device info response missing watch mac");
    }
    if resp.sn.is_empty() {
        bail_site!("vivo device info response missing watch sn");
    }
    if resp.device_name.is_empty() {
        bail_site!("vivo device info response missing device name");
    }
    Ok(())
}

fn default_phone_bid_versions(product_series_type: Option<i32>) -> Vec<BidVersion> {
    PHONE_BID_VERSION_PAIRS
        .iter()
        .map(|(bid, version)| {
            let version = if *bid == 19 && product_series_type.is_some_and(|value| value < 3) {
                2
            } else {
                *version
            };
            BidVersion { bid: *bid, version }
        })
        .collect()
}

fn decode_response_code(payload: &[u8]) -> anyhow::Result<i32> {
    let mut reader = MsgpackReader::new(payload);
    reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo response code: {err}"))
}

struct WatchBidVersionResponseData {
    code: i32,
    phone_bid_version: i32,
    bid_versions: Vec<BidVersion>,
    features: Vec<FeatureItem>,
}

fn decode_watch_bid_version_response(
    payload: &[u8],
) -> anyhow::Result<WatchBidVersionResponseData> {
    let mut reader = MsgpackReader::new(payload);
    let code = reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo BID version response code: {err}"))?;
    let phone_bid_version = reader
        .read_i32()
        .map_err(|err| anyhow_site!("failed to decode vivo phone BID version: {err}"))?;
    let bid_version_count = reader
        .read_array_len()
        .map_err(|err| anyhow_site!("failed to decode vivo BID version array: {err}"))?;
    let mut bid_versions = Vec::with_capacity(bid_version_count);
    for _ in 0..bid_version_count {
        let payload = reader
            .read_bin()
            .map_err(|err| anyhow_site!("failed to decode vivo BID version entry: {err}"))?;
        bid_versions.push(decode_bid_version(&payload)?);
    }

    let mut features = Vec::new();
    if reader.has_next() {
        let feature_count = reader
            .read_array_len()
            .map_err(|err| anyhow_site!("failed to decode vivo feature array: {err}"))?;
        features.reserve(feature_count);
        for _ in 0..feature_count {
            let payload = reader
                .read_bin()
                .map_err(|err| anyhow_site!("failed to decode vivo feature entry: {err}"))?;
            features.push(decode_feature_item(&payload)?);
        }
    }

    Ok(WatchBidVersionResponseData {
        code,
        phone_bid_version,
        bid_versions,
        features,
    })
}

fn decode_bid_version(payload: &[u8]) -> anyhow::Result<BidVersion> {
    let mut reader = MsgpackReader::new(payload);
    Ok(BidVersion {
        bid: reader
            .read_i32()
            .map_err(|err| anyhow_site!("failed to decode vivo BID id: {err}"))?,
        version: reader
            .read_i32()
            .map_err(|err| anyhow_site!("failed to decode vivo BID version: {err}"))?,
    })
}

fn decode_feature_item(payload: &[u8]) -> anyhow::Result<FeatureItem> {
    let mut reader = MsgpackReader::new(payload);
    Ok(FeatureItem {
        key: reader
            .read_i32()
            .map_err(|err| anyhow_site!("failed to decode vivo feature key: {err}"))?,
        value: reader
            .read_str()
            .map_err(|err| anyhow_site!("failed to decode vivo feature value: {err}"))?,
    })
}

fn current_unix_time_parts() -> anyhow::Result<(i32, i32)> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let now = chrono::Local::now();
        let sec = i32::try_from(now.timestamp())
            .map_err(|_| anyhow_site!("current unix timestamp does not fit i32"))?;
        let millis = i32::try_from(now.timestamp_subsec_millis())
            .map_err(|_| anyhow_site!("current millis does not fit i32"))?;
        Ok((sec, millis))
    }

    #[cfg(target_arch = "wasm32")]
    {
        use web_time::{SystemTime, UNIX_EPOCH};

        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| anyhow_site!("system time before unix epoch: {err:?}"))?;
        let sec = i32::try_from(duration.as_secs())
            .map_err(|_| anyhow_site!("current unix timestamp does not fit i32"))?;
        let millis = i32::try_from(duration.subsec_millis())
            .map_err(|_| anyhow_site!("current millis does not fit i32"))?;
        Ok((sec, millis))
    }
}

fn local_timezone_offset_sec() -> i32 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        chrono::Local::now().offset().local_minus_utc()
    }

    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

fn gmt_timezone_from_offset(offset_sec: i32) -> i32 {
    let hours = offset_sec / 3600;
    if hours < 0 { hours + 24 } else { hours }
}

#[cfg(test)]
mod tests {
    use super::{
        WATCH_STATE_BIND, WATCH_STATE_INIT, WATCH_STATE_MID_CONN, WATCH_STATE_MID_FACTORY,
        WATCH_STATE_MID_REPAIR, default_phone_bid_versions, gmt_timezone_from_offset,
        is_watch_open_id_compatible_for_state, normalize_watch_open_id,
    };

    #[test]
    fn watch_open_id_compatibility_follows_bind_state() {
        assert!(is_watch_open_id_compatible_for_state(
            "local",
            "",
            WATCH_STATE_INIT
        ));
        assert!(is_watch_open_id_compatible_for_state(
            "local",
            "local",
            WATCH_STATE_BIND
        ));
        assert!(!is_watch_open_id_compatible_for_state(
            "local",
            "",
            WATCH_STATE_BIND
        ));
        assert!(!is_watch_open_id_compatible_for_state(
            "local",
            "other",
            WATCH_STATE_MID_CONN
        ));
        assert!(is_watch_open_id_compatible_for_state(
            "local",
            "",
            WATCH_STATE_MID_FACTORY
        ));
        assert!(!is_watch_open_id_compatible_for_state(
            "local",
            "",
            WATCH_STATE_MID_REPAIR
        ));
    }

    #[test]
    fn zero_prefixed_watch_open_id_is_treated_as_empty() {
        assert_eq!(normalize_watch_open_id("000000abcdef".to_string()), "");
        assert_eq!(normalize_watch_open_id("abcdef".to_string()), "abcdef");
    }

    #[test]
    fn vivo_gmt_timezone_matches_app_wraparound() {
        assert_eq!(gmt_timezone_from_offset(8 * 3600), 8);
        assert_eq!(gmt_timezone_from_offset(-5 * 3600), 19);
    }

    #[test]
    fn phone_bid_versions_follow_java_defaults() {
        let versions = default_phone_bid_versions(None);
        assert_eq!(
            versions.iter().find(|item| item.bid == 48).unwrap().version,
            8
        );
        assert_eq!(
            versions.iter().find(|item| item.bid == 19).unwrap().version,
            5
        );
        assert_eq!(versions.iter().filter(|item| item.bid == 24).count(), 2);

        let legacy_versions = default_phone_bid_versions(Some(2));
        assert_eq!(
            legacy_versions
                .iter()
                .find(|item| item.bid == 19)
                .unwrap()
                .version,
            2
        );
    }
}
