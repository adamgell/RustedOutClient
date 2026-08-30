use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

const MIN_VMID: u32 = 100;
const MAX_VMID: u32 = 99_999_999;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ModelError {
    #[error(
        "SSH target must be 1-255 printable non-whitespace characters and cannot begin with '-'"
    )]
    InvalidSshTarget,
    #[error("node must match [A-Za-z0-9][A-Za-z0-9._-]{{0,63}}")]
    InvalidNodeName,
    #[error("VMID must be between {MIN_VMID} and {MAX_VMID}")]
    InvalidVmId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshTarget(String);

impl SshTarget {
    pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        let valid = (1..=255).contains(&value.len())
            && !value.starts_with('-')
            && value.bytes().all(|byte| byte.is_ascii_graphic());

        if valid {
            Ok(Self(value))
        } else {
            Err(ModelError::InvalidSshTarget)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for SshTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SshTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|value| Self::parse(value).map_err(de::Error::custom))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeName(String);

impl NodeName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid = value.len() <= 64
            && bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));

        if valid {
            Ok(Self(value))
        } else {
            Err(ModelError::InvalidNodeName)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for NodeName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NodeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)
            .and_then(|value| Self::parse(value).map_err(de::Error::custom))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct VmId(u32);

impl VmId {
    pub fn new(value: u32) -> Result<Self, ModelError> {
        if (MIN_VMID..=MAX_VMID).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ModelError::InvalidVmId)
        }
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for VmId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for VmId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for VmId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        u32::deserialize(deserializer).and_then(|value| Self::new(value).map_err(de::Error::custom))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PveProfile {
    pub name: String,
    pub ssh_target: SshTarget,
    pub node: NodeName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleMode {
    Fit,
    OneToOne,
}
