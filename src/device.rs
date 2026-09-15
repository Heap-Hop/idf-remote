use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 {
            return Err("device ID must contain 1..256 bytes".into());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for DeviceId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAvailability {
    Available,
    PermissionRequired,
    Disconnected,
    IdentityMismatch,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceActivity {
    Idle,
    Busy,
    Monitoring,
    Reconnecting,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeviceStatus {
    pub availability: DeviceAvailability,
    pub activity: DeviceActivity,
}

impl DeviceStatus {
    pub const AVAILABLE_IDLE: Self = Self {
        availability: DeviceAvailability::Available,
        activity: DeviceActivity::Idle,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCapability {
    Probe,
    Flash,
    ReadFlash,
    EraseFlash,
    SerialWrite,
    Reset,
    Monitor,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TransportDescriptor {
    /// A stable protocol label such as `desktop_serial` or `android_usb`.
    pub kind: String,
    /// A backend address for display and local CLI selection. It is never an
    /// authoritative physical identity and is not accepted by operation APIs.
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeviceDescriptor {
    pub id: DeviceId,
    pub display_name: String,
    pub transport: TransportDescriptor,
    pub status: DeviceStatus,
    pub capabilities: Vec<DeviceCapability>,
}

impl DeviceDescriptor {
    pub fn supports(&self, capability: DeviceCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_reject_empty_and_oversized_wire_values() {
        assert!(serde_json::from_str::<DeviceId>(r#"""#).is_err());
        assert!(DeviceId::new("x".repeat(257)).is_err());
        assert_eq!(DeviceId::new("dev_123").unwrap().as_str(), "dev_123");
    }

    #[test]
    fn descriptor_keeps_address_as_non_authoritative_transport_data() {
        let descriptor = DeviceDescriptor {
            id: DeviceId::new("dev_123").unwrap(),
            display_name: "Test board".into(),
            transport: TransportDescriptor {
                kind: "desktop_serial".into(),
                address: Some("COM7".into()),
                metadata: BTreeMap::from([("serial_number".into(), "abc".into())]),
            },
            status: DeviceStatus::AVAILABLE_IDLE,
            capabilities: vec![DeviceCapability::Flash],
        };
        let json = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(json["id"], "dev_123");
        assert_eq!(json["transport"]["address"], "COM7");
        assert_eq!(json["status"]["availability"], "available");
        assert_eq!(json["status"]["activity"], "idle");
        assert!(descriptor.supports(DeviceCapability::Flash));
        assert!(!descriptor.supports(DeviceCapability::Monitor));
    }
}
