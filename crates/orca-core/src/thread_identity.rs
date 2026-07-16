use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

const TURN_ID_PREFIX: &str = "turn_";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn new() -> Self {
        Self(format!("{TURN_ID_PREFIX}{}", Uuid::now_v7()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let suffix = value
            .strip_prefix(TURN_ID_PREFIX)
            .ok_or_else(|| format!("turn id must start with {TURN_ID_PREFIX}"))?;
        let uuid = Uuid::parse_str(suffix).map_err(|error| format!("invalid turn id: {error}"))?;
        if uuid.get_version_num() != 7 {
            return Err("turn id must contain a UUIDv7 value".to_string());
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for TurnId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Borrow<str> for TurnId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for TurnId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for TurnId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_turn_ids_are_typed_unique_uuidv7_values() {
        let first = TurnId::new();
        let second = TurnId::new();

        assert_ne!(first, second);
        assert!(first.as_str().starts_with(TURN_ID_PREFIX));
        assert_eq!(
            TurnId::parse(first.to_string()).expect("parse fresh id"),
            first
        );
    }

    #[test]
    fn malformed_or_wrong_version_turn_ids_fail_closed() {
        assert!(TurnId::parse("turn-1").is_err());
        assert!(TurnId::parse(format!("turn_{}", Uuid::new_v4())).is_err());
    }
}
