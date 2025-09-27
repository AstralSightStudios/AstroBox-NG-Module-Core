use crate::device::xiaomi::config::ResConfig;

pub fn get_watchface_id(data: &[u8], config: &ResConfig) -> Option<String> {
    let offset = config.watchface_id_offset;
    let field_len = config.watchface_id_field_len;
    if data.len() < offset + field_len {
        return None;
    }
    let field = &data[offset..offset + field_len];

    let start = field.iter().position(|&b| (b as char).is_ascii_digit())?;
    let digits: String = field[start..]
        .iter()
        .take_while(|&&b| (b as char).is_ascii_digit())
        .map(|&b| b as char)
        .collect();

    Some(digits)
}
