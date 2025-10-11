use num_enum::{IntoPrimitive, TryFromPrimitive};

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
pub enum ConnectType {
    SPP = 0,
    BLE = 1,
}

impl serde::Serialize for ConnectType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let value = match self {
            ConnectType::SPP => "SPP",
            ConnectType::BLE => "BLE",
        };
        serializer.serialize_str(value)
    }
}
