use nanorand::Rng;

pub fn vec_to_array_16_opt(v: &Vec<u8>) -> Option<[u8; 16]> {
    v.as_slice().try_into().ok()
}

pub fn to_hex_string(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn hex_stream_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("Hex string has an odd length".to_string());
    }

    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| format!("{:?}", e))
        })
        .collect()
}

pub fn generate_random_bytes(size: usize) -> Vec<u8> {
    let mut rng = nanorand::tls_rng();
    let mut buffer = vec![0u8; size];
    rng.fill_bytes(&mut buffer);
    buffer
}

pub fn calc_md5(data: &[u8]) -> Vec<u8> {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

pub fn calc_crc32_bytes(data: &[u8]) -> [u8; 4] {
    use crc::{CRC_32_ISO_HDLC, Crc};
    let crc = Crc::<u32>::new(&CRC_32_ISO_HDLC);
    crc.checksum(data).to_be_bytes()
}
