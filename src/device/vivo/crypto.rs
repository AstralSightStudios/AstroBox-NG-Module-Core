use aes::Aes128;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use crc::{CRC_32_ISO_HDLC, Crc};

use super::{VivoProtocolError, VivoProtocolResult};

type Aes128CbcEncryptor = cbc::Encryptor<Aes128>;
type Aes128CbcDecryptor = cbc::Decryptor<Aes128>;

pub const VSCP_AES_KEY: [u8; 16] = [
    0x0A, 0x0B, 0x0C, 0x0D, 0x01, 0x02, 0x03, 0x03, 0x0A, 0x0A, 0x0A, 0x0A, 0x0F, 0x0F, 0x0F, 0x0F,
];

pub const VSCP_AES_IV: [u8; 16] = VSCP_AES_KEY;

pub fn crc32_iso_hdlc(data: &[u8]) -> u32 {
    Crc::<u32>::new(&CRC_32_ISO_HDLC).checksum(data)
}

pub fn vscp_crc16(data: &[u8]) -> u16 {
    (crc32_iso_hdlc(data) >> 16) as u16
}

pub fn aes_cbc_pkcs7_encrypt(
    plain: &[u8],
    key: &[u8; 16],
    iv: &[u8; 16],
) -> VivoProtocolResult<Vec<u8>> {
    let cipher = Aes128CbcEncryptor::new_from_slices(key, iv)
        .map_err(|_| VivoProtocolError::Crypto("invalid AES key or IV"))?;
    let mut buffer = vec![0u8; plain.len() + 16];
    buffer[..plain.len()].copy_from_slice(plain);
    let out = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plain.len())
        .map_err(|_| VivoProtocolError::Crypto("AES-CBC padding failed"))?;
    Ok(out.to_vec())
}

pub fn aes_cbc_pkcs7_decrypt(
    ciphertext: &[u8],
    key: &[u8; 16],
    iv: &[u8; 16],
) -> VivoProtocolResult<Vec<u8>> {
    let cipher = Aes128CbcDecryptor::new_from_slices(key, iv)
        .map_err(|_| VivoProtocolError::Crypto("invalid AES key or IV"))?;
    let mut buffer = ciphertext.to_vec();
    let out = cipher
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| VivoProtocolError::Crypto("AES-CBC decrypt failed"))?;
    Ok(out.to_vec())
}

pub fn vscp_encrypt(plain: &[u8]) -> VivoProtocolResult<Vec<u8>> {
    aes_cbc_pkcs7_encrypt(plain, &VSCP_AES_KEY, &VSCP_AES_IV)
}

pub fn vscp_decrypt(ciphertext: &[u8]) -> VivoProtocolResult<Vec<u8>> {
    aes_cbc_pkcs7_decrypt(ciphertext, &VSCP_AES_KEY, &VSCP_AES_IV)
}

pub fn bind_aes_sign(id: &str, random_low16: u16) -> VivoProtocolResult<Vec<u8>> {
    let mut first = Vec::with_capacity(id.len() + 2);
    first.extend_from_slice(id.as_bytes());
    first.extend_from_slice(&random_low16.to_be_bytes());

    let crc = vscp_crc16(&first);
    let mut plain = Vec::with_capacity(first.len() + 2);
    plain.extend_from_slice(&first);
    plain.extend_from_slice(&crc.to_be_bytes());
    vscp_encrypt(&plain)
}

pub fn verify_bind_aes_sign(
    id: &str,
    random_low16: u16,
    expected: &[u8],
) -> VivoProtocolResult<bool> {
    Ok(bind_aes_sign(id, random_low16)? == expected)
}
