use super::super::{VivoProtocolError, VivoProtocolResult};

pub const VIVO_VSCP_SERVICE_UUID: &str = "00002760-08c2-11e1-9073-0e8ac72e1011";
pub const VIVO_VSCP_WRITE_UUID: &str = "00002760-08c2-11e1-9073-0e8ac72e0011";
pub const VIVO_VSCP_NOTIFY_UUID: &str = "00002760-08c2-11e1-9073-0e8ac72e0012";
pub const VIVO_VSCP_SPARE_WRITE_UUID: &str = "00002760-08c2-11e1-9073-0e8ac72e0013";

pub fn split_v2_pdu_for_ble(pdu: &[u8], att_mtu: usize) -> VivoProtocolResult<Vec<Vec<u8>>> {
    let max_chunk = att_mtu
        .checked_sub(3)
        .ok_or(VivoProtocolError::InvalidFrame("BLE ATT MTU must be >= 4"))?;
    if max_chunk == 0 {
        return Err(VivoProtocolError::InvalidFrame(
            "BLE ATT MTU chunk size is zero",
        ));
    }
    Ok(pdu.chunks(max_chunk).map(|chunk| chunk.to_vec()).collect())
}
