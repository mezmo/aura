//! Serializes a [`Duration`] as whole milliseconds, the unit every duration on
//! the wire is expressed in. Use it as
//! `#[serde(with = "aura_events::duration_ms")]`, or `duration_ms::option` for
//! an `Option<Duration>`.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .serialize(serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    u64::deserialize(deserializer).map(Duration::from_millis)
}

pub mod option {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        duration: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        duration
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Timed {
        #[serde(with = "super")]
        elapsed: Duration,
        #[serde(with = "super::option")]
        limit: Option<Duration>,
    }

    #[test]
    fn a_duration_is_whole_milliseconds() {
        let timed = Timed {
            elapsed: Duration::from_micros(1_500_999),
            limit: Some(Duration::from_millis(30_000)),
        };
        let json = serde_json::to_value(&timed).unwrap();
        assert_eq!(json, json!({ "elapsed": 1500, "limit": 30_000 }));
        assert_eq!(
            serde_json::from_value::<Timed>(json).unwrap(),
            Timed {
                elapsed: Duration::from_millis(1500),
                limit: Some(Duration::from_millis(30_000)),
            }
        );
    }

    #[test]
    fn an_absent_duration_is_null() {
        let timed = Timed {
            elapsed: Duration::ZERO,
            limit: None,
        };
        let json = serde_json::to_value(&timed).unwrap();
        assert_eq!(json, json!({ "elapsed": 0, "limit": null }));
        assert_eq!(serde_json::from_value::<Timed>(json).unwrap(), timed);
    }

    /// A duration too long for a u64 of milliseconds saturates rather than
    /// failing to serialize.
    #[test]
    fn a_duration_past_u64_milliseconds_saturates() {
        let timed = Timed {
            elapsed: Duration::MAX,
            limit: Some(Duration::MAX),
        };
        assert_eq!(
            serde_json::to_value(&timed).unwrap(),
            json!({ "elapsed": u64::MAX, "limit": u64::MAX })
        );
    }
}
