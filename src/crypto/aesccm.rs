use aes::Aes128;
use ccm::{
    Ccm, KeyInit,
    aead::{Aead, Payload, generic_array::GenericArray},
    consts::{U4, U12},
};

// 定义：Tag 长度 4 字节，Nonce 长度 12 字节
type Aes128Ccm = Ccm<Aes128, U4, U12>;

/// CCM 加密  
/// - `key`: 16 字节  
/// - `nonce`: 12 字节  
/// - `aad`：可选附加认证数据  
/// 返回的 Vec 包含：密文 ‖ 4 字节 Tag
pub fn aes128_ccm_encrypt(
    key: &[u8; 16],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    let cipher = Aes128Ccm::new(GenericArray::from_slice(key));
    cipher
        .encrypt(
            GenericArray::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("CCM 加密失败")
}

/// CCM 解密  
/// - 输入的 `ciphertext` 必须是：密文 ‖ Tag（4 字节）
/// - 解密失败会 `Err`（包括 Tag 校验未通过）
pub fn aes128_ccm_decrypt(
    key: &[u8; 16],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext_and_tag: &[u8],
) -> Result<Vec<u8>, ccm::aead::Error> {
    let cipher = Aes128Ccm::new(GenericArray::from_slice(key));
    cipher.decrypt(
        GenericArray::from_slice(nonce),
        Payload {
            msg: ciphertext_and_tag,
            aad,
        },
    )
}
