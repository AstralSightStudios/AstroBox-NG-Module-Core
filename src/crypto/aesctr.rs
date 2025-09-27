use aes::{
    Aes128,
    cipher::{KeyIvInit, StreamCipher},
};
use ctr::Ctr128BE;

type Aes128Ctr = Ctr128BE<Aes128>;

/// 对 `plaintext` 做 AES-128-CTR 加密／解密（CTR 是对称的）
/// - `key` 长度 16 字节
/// - `iv` 长度 16 字节（你可以根据协议构造 12 字节 nonce + 4 字节计数器）
/// 返回：与输入等长的密文或明文
pub fn aes128_ctr_crypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut buffer = data.to_vec();
    let mut cipher = Aes128Ctr::new(key.into(), iv.into());
    cipher.apply_keystream(&mut buffer);
    buffer
}
