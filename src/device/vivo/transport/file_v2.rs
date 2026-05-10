use vivo_msgpack::{
    Result as MsgpackResult,
    msgpack::{MsgpackReader, write_bin, write_i32, write_str},
};

use crate::{anyhow_site, bail_site, device::vivo::VivoConnectType};

pub const CID_SETUP: u8 = 1;
pub const CID_SEND: u8 = 2;
pub const CID_END: u8 = 3;
pub const CID_SETUP_RESPONSE: u8 = 0x81;
pub const CID_SEND_RESPONSE: u8 = 0x82;
pub const CID_END_RESPONSE: u8 = 0x83;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileV2Channel {
    BLE,
    BT,
}

impl From<VivoConnectType> for FileV2Channel {
    fn from(value: VivoConnectType) -> Self {
        match value {
            VivoConnectType::BLE => Self::BLE,
            VivoConnectType::SPP => Self::BT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileV2Direction {
    PhoneToWatch = 1,
    WatchToPhone = 2,
}

pub fn business_id(direction: FileV2Direction, channel: FileV2Channel) -> u8 {
    match (direction, channel) {
        (FileV2Direction::PhoneToWatch, FileV2Channel::BLE) => 62,
        (FileV2Direction::PhoneToWatch, FileV2Channel::BT) => 64,
        (FileV2Direction::WatchToPhone, FileV2Channel::BLE) => 63,
        (FileV2Direction::WatchToPhone, FileV2Channel::BT) => 65,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetUpRequestV2 {
    pub file_id: String,
    pub file_path: String,
    pub file_name: String,
    pub file_size: i32,
    pub check_type: i32,
    pub n_bytes_write_tmp_file: i32,
    pub pack_write_status_file: i32,
    pub timeout: i32,
    pub extra: Option<Vec<u8>>,
}

impl SetUpRequestV2 {
    pub fn payload(&self) -> MsgpackResult<Vec<u8>> {
        let mut out = Vec::new();
        write_str(&mut out, &self.file_id)?;
        write_str(&mut out, &self.file_path)?;
        write_str(&mut out, &self.file_name)?;
        write_i32(&mut out, self.file_size);
        write_i32(&mut out, self.check_type);
        write_i32(&mut out, self.n_bytes_write_tmp_file);
        write_i32(&mut out, self.pack_write_status_file);
        write_i32(&mut out, self.timeout);
        if let Some(extra) = &self.extra {
            write_bin(&mut out, extra)?;
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequestV2<'a> {
    pub file_id: &'a str,
    pub offset: i64,
    pub if_need_rsp: bool,
    pub data: &'a [u8],
}

impl SendRequestV2<'_> {
    pub fn payload(&self) -> anyhow::Result<Vec<u8>> {
        let send_req_length = i32::try_from(self.data.len())
            .map_err(|_| anyhow_site!("vivo FileV2 send chunk too large: {}", self.data.len()))?;
        let offset = i32::try_from(self.offset)
            .map_err(|_| anyhow_site!("vivo FileV2 offset does not fit i32: {}", self.offset))?;
        let mut out = Vec::new();
        write_str(&mut out, self.file_id)?;
        write_i32(&mut out, offset);
        write_i32(&mut out, send_req_length);
        write_i32(&mut out, if self.if_need_rsp { 1 } else { 2 });
        write_bin(&mut out, self.data)?;
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndAction {
    Default = 1,
    Cancel = 2,
    Pause = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndRequestV2 {
    pub file_id: String,
    pub result_code: i32,
    pub action: EndAction,
}

impl EndRequestV2 {
    pub fn payload(&self) -> MsgpackResult<Vec<u8>> {
        let mut out = Vec::new();
        write_str(&mut out, &self.file_id)?;
        write_i32(&mut out, self.result_code);
        write_i32(&mut out, self.action as i32);
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferProgressResponse {
    pub code: i32,
    pub file_id: String,
    pub offset: i64,
    pub file_accumulate_crc: i32,
    pub resp_pack_num: i32,
}

impl FileTransferProgressResponse {
    /// 容错解码：error 路径上手表只填 code 和 fileId（见 jadx
    /// `SetUpResponseV2#parsePayload` 的 catch — 它静默吃掉剩余字段的解码异常）。
    /// 我们至少要拿到 code 才能告诉上层「为什么被拒绝」。
    pub fn decode(payload: &[u8]) -> anyhow::Result<Self> {
        let mut reader = MsgpackReader::new(payload);
        let code = reader.read_i32().map_err(|err| {
            anyhow::Error::msg(format!(
                "failed to decode file_v2 response code: {err}; raw payload (hex)={}",
                hex(payload)
            ))
        })?;
        let file_id = reader.read_str().unwrap_or_default();
        let offset = reader.read_i64().unwrap_or(0);
        let file_accumulate_crc = reader.read_i32().unwrap_or(0);
        let resp_pack_num = reader.read_i32().unwrap_or(0);
        Ok(Self {
            code,
            file_id,
            offset,
            file_accumulate_crc,
            resp_pack_num,
        })
    }

    pub fn ensure_success(&self, ctx: &'static str) -> anyhow::Result<()> {
        if self.code == 0 {
            Ok(())
        } else {
            bail_site!("{ctx} failed: code={}", self.code)
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndResponseV2 {
    pub code: i32,
    pub file_id: String,
}

impl EndResponseV2 {
    pub fn decode(payload: &[u8]) -> anyhow::Result<Self> {
        let mut reader = MsgpackReader::new(payload);
        Ok(Self {
            code: reader.read_i32()?,
            file_id: reader.read_str()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use vivo_msgpack::msgpack::MsgpackReader;

    use super::{
        EndAction, EndRequestV2, FileV2Channel, FileV2Direction, SendRequestV2, SetUpRequestV2,
        business_id,
    };

    #[test]
    fn file_v2_business_id_matches_java_base_file_request() {
        assert_eq!(
            business_id(FileV2Direction::PhoneToWatch, FileV2Channel::BLE),
            62
        );
        assert_eq!(
            business_id(FileV2Direction::PhoneToWatch, FileV2Channel::BT),
            64
        );
        assert_eq!(
            business_id(FileV2Direction::WatchToPhone, FileV2Channel::BLE),
            63
        );
        assert_eq!(
            business_id(FileV2Direction::WatchToPhone, FileV2Channel::BT),
            65
        );
    }

    #[test]
    fn setup_request_payload_follows_java_field_order() {
        let payload = SetUpRequestV2 {
            file_id: "fid".to_string(),
            file_path: "/tmp/a.zip".to_string(),
            file_name: "a.zip".to_string(),
            file_size: 123,
            check_type: 1,
            n_bytes_write_tmp_file: 2,
            pack_write_status_file: 3,
            timeout: 4,
            extra: Some(vec![0xaa, 0xbb]),
        }
        .payload()
        .unwrap();

        let mut reader = MsgpackReader::new(&payload);
        assert_eq!(reader.read_str().unwrap(), "fid");
        assert_eq!(reader.read_str().unwrap(), "/tmp/a.zip");
        assert_eq!(reader.read_str().unwrap(), "a.zip");
        assert_eq!(reader.read_i32().unwrap(), 123);
        assert_eq!(reader.read_i32().unwrap(), 1);
        assert_eq!(reader.read_i32().unwrap(), 2);
        assert_eq!(reader.read_i32().unwrap(), 3);
        assert_eq!(reader.read_i32().unwrap(), 4);
        assert_eq!(reader.read_bin().unwrap(), vec![0xaa, 0xbb]);
        assert!(!reader.has_next());
    }

    #[test]
    fn send_and_end_payloads_follow_java_field_order() {
        let payload = SendRequestV2 {
            file_id: "fid",
            offset: 9,
            if_need_rsp: true,
            data: &[1, 2, 3],
        }
        .payload()
        .unwrap();
        let mut reader = MsgpackReader::new(&payload);
        assert_eq!(reader.read_str().unwrap(), "fid");
        assert_eq!(reader.read_i32().unwrap(), 9);
        assert_eq!(reader.read_i32().unwrap(), 3);
        assert_eq!(reader.read_i32().unwrap(), 1);
        assert_eq!(reader.read_bin().unwrap(), vec![1, 2, 3]);
        assert!(!reader.has_next());

        let payload = EndRequestV2 {
            file_id: "fid".to_string(),
            result_code: 0,
            action: EndAction::Default,
        }
        .payload()
        .unwrap();
        let mut reader = MsgpackReader::new(&payload);
        assert_eq!(reader.read_str().unwrap(), "fid");
        assert_eq!(reader.read_i32().unwrap(), 0);
        assert_eq!(reader.read_i32().unwrap(), 1);
        assert!(!reader.has_next());
    }
}
