use tokio::runtime::Handle;
use vivo_msgpack::{
    messages::{
        device_info::{BID_DEVICE_INFO, CID_DEVICE_INFO, DeviceInfoRequest},
        generated::typed::{PhoneTimeForMatRequest, SyncInfoSetLanguageV2Request},
        response_cid,
    },
    msgpack::MsgpackReader,
};

use crate::{
    anyhow_site,
    device::vivo::{
        system::{VivoSystemExt, register_vivo_system_ext_on_message},
        transport::vscp::VscpMessage,
    },
    ecs::Component,
    models::sync::TimeSyncProps,
};

use super::shared::{HasVivoRequestContext, VivoRequestExt};

#[derive(Component)]
pub struct SyncSystem {
    owner_id: String,
    tk_handle: Handle,
}

impl SyncSystem {
    pub fn new(owner_id: String, tk_handle: Handle) -> Self {
        register_vivo_system_ext_on_message::<Self>();
        Self {
            owner_id,
            tk_handle,
        }
    }

    pub fn sync_time(&mut self, props: TimeSyncProps) -> anyhow::Result<()> {
        let time = build_vivo_time_sync(&props)?;
        let device_info_payload = DeviceInfoRequest {
            unix_time_sec: time.unix_time_sec,
            gmt_timezone: gmt_timezone_from_offset(time.timezone_offset_sec),
            os_type: 1,
            millis: time.millis,
            timezone_offset_sec: Some(time.timezone_offset_sec),
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo time sync request: {err}"))?;

        log::info!(
            "[VivoDevice.Sync] syncing time unix={} millis={} tz_offset={} is_24h={}",
            time.unix_time_sec,
            time.millis,
            time.timezone_offset_sec,
            !props.is_12_hour_format
        );

        self.send_vivo_message(
            VscpMessage::new(BID_DEVICE_INFO, CID_DEVICE_INFO, device_info_payload),
            "VivoSyncSystem::sync_time.device_info",
        )?;

        let time_format_payload = PhoneTimeForMatRequest {
            is24_time: !props.is_12_hour_format,
        }
        .payload()
        .map_err(|err| anyhow_site!("failed to encode vivo time format request: {err}"))?;
        self.send_vivo_message(
            VscpMessage::new(
                BID_DEVICE_INFO,
                PhoneTimeForMatRequest::CID,
                time_format_payload,
            ),
            "VivoSyncSystem::sync_time.time_format",
        )
    }

    pub fn set_language(&mut self, locale: String) -> anyhow::Result<()> {
        let (language, country) = split_locale(&locale);
        log::info!(
            "[VivoDevice.Sync] setting language locale={} language={} country={}",
            locale,
            language,
            country
        );
        let payload = SyncInfoSetLanguageV2Request { language, country }
            .payload()
            .map_err(|err| anyhow_site!("failed to encode vivo language request: {err}"))?;
        self.send_vivo_message(
            VscpMessage::new(BID_DEVICE_INFO, SyncInfoSetLanguageV2Request::CID, payload),
            "VivoSyncSystem::set_language",
        )
    }
}

impl HasVivoRequestContext for SyncSystem {
    fn owner_id(&self) -> &str {
        &self.owner_id
    }

    fn tk_handle(&self) -> &Handle {
        &self.tk_handle
    }
}

impl VivoSystemExt for SyncSystem {
    fn on_vivo_message(&mut self, message: &VscpMessage) {
        if message.bid != BID_DEVICE_INFO {
            return;
        }

        match message.cid {
            cid if cid == response_cid(PhoneTimeForMatRequest::CID) => {
                log_common_response("[VivoDevice.Sync] time-format response", &message.payload);
            }
            cid if cid == response_cid(SyncInfoSetLanguageV2Request::CID) => {
                log_common_response("[VivoDevice.Sync] language response", &message.payload);
            }
            _ => {}
        }
    }
}

#[derive(Component, serde::Serialize)]
pub struct SyncComponent {}

impl SyncComponent {
    pub fn new() -> Self {
        Self {}
    }
}

struct VivoTimeSync {
    unix_time_sec: i32,
    millis: i32,
    timezone_offset_sec: i32,
}

fn build_vivo_time_sync(props: &TimeSyncProps) -> anyhow::Result<VivoTimeSync> {
    let offset_minutes = (props.timezone.offset - 32) * 15;
    let offset_sec = offset_minutes
        .checked_mul(60)
        .ok_or_else(|| anyhow_site!("vivo timezone offset overflow"))?;

    validate_time_parts(props)?;
    let days = days_from_civil(props.date.year as i32, props.date.month, props.date.day);
    let local_seconds = days
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(i64::from(props.time.hour) * 3600))
        .and_then(|value| value.checked_add(i64::from(props.time.minute) * 60))
        .and_then(|value| value.checked_add(i64::from(props.time.second)))
        .ok_or_else(|| anyhow_site!("vivo sync timestamp overflow"))?;
    let unix_time_sec = i32::try_from(local_seconds - i64::from(offset_sec))
        .map_err(|_| anyhow_site!("vivo sync timestamp does not fit i32"))?;
    let millis = i32::try_from(props.time.millisecond)
        .map_err(|_| anyhow_site!("vivo sync millis does not fit i32"))?;

    Ok(VivoTimeSync {
        unix_time_sec,
        millis,
        timezone_offset_sec: offset_sec,
    })
}

fn validate_time_parts(props: &TimeSyncProps) -> anyhow::Result<()> {
    if props.date.month == 0 || props.date.month > 12 {
        return Err(anyhow_site!(
            "invalid vivo sync month: {}",
            props.date.month
        ));
    }
    let max_day = days_in_month(props.date.year as i32, props.date.month);
    if props.date.day == 0 || props.date.day > max_day {
        return Err(anyhow_site!("invalid vivo sync day: {}", props.date.day));
    }
    if props.time.hour > 23 || props.time.minute > 59 || props.time.second > 59 {
        return Err(anyhow_site!(
            "invalid vivo sync time: {:02}:{:02}:{:02}",
            props.time.hour,
            props.time.minute,
            props.time.second
        ));
    }
    if props.time.millisecond > 999 {
        return Err(anyhow_site!(
            "invalid vivo sync millisecond: {}",
            props.time.millisecond
        ));
    }
    Ok(())
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = div_floor(year, 400);
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

fn div_floor(value: i32, divisor: i32) -> i32 {
    let quotient = value / divisor;
    let remainder = value % divisor;
    if remainder != 0 && ((remainder > 0) != (divisor > 0)) {
        quotient - 1
    } else {
        quotient
    }
}

fn split_locale(locale: &str) -> (String, String) {
    let normalized = locale.replace('-', "_");
    let mut parts = normalized.split('_').filter(|part| !part.is_empty());
    let language = parts.next().unwrap_or("en").to_ascii_lowercase();
    let country = parts.next().unwrap_or("").to_ascii_uppercase();
    (language, country)
}

fn gmt_timezone_from_offset(offset_sec: i32) -> i32 {
    let hours = offset_sec / 3600;
    if hours < 0 { hours + 24 } else { hours }
}

fn log_common_response(prefix: &str, payload: &[u8]) {
    let mut reader = MsgpackReader::new(payload);
    match reader.read_i32() {
        Ok(code) => log::info!("{prefix}: code={code}"),
        Err(err) => log::warn!("{prefix}: failed to decode code: {err}"),
    }
}
