#[derive(serde::Serialize, serde::Deserialize)]
pub struct TimeSyncProps {
    pub date: Date,
    pub time: Time,
    pub timezone: TimeZone,
    pub is_12_hour_format: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Date {
    pub year: u32,
    pub month: u32,
    pub day: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Time {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millisecond: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct TimeZone {
    pub offset: i32,
    pub dst_offset: i32,
    pub id: String,
}
