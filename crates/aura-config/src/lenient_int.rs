/// Serde helpers for accepting whole-number floats (e.g. `8000.0`) as integers.
///
/// Helm's YAML parser represents all numbers as Go float64, so `toToml`
/// renders `max_tokens = 8000.0` instead of `8000`. Rather than fixing this
/// in Helm templates, we accept both forms on the Rust side.
use serde::{Deserialize, Deserializer};

/// Deserialize a value that may be either an integer or a whole-number float.
pub fn deserialize_option_u32<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    Option::<f64>::deserialize(deserializer)?
        .map(|f| float_to_int(f, "u32"))
        .transpose()
}

pub fn deserialize_option_u64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    Option::<f64>::deserialize(deserializer)?
        .map(|f| float_to_int(f, "u64"))
        .transpose()
}

pub fn deserialize_option_usize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    Option::<f64>::deserialize(deserializer)?
        .map(|f| float_to_int(f, "usize"))
        .transpose()
}

fn float_to_int<T, E>(f: f64, type_name: &str) -> Result<T, E>
where
    T: TryFrom<u64>,
    E: serde::de::Error,
{
    if f < 0.0 {
        return Err(E::custom(format!(
            "expected non-negative number for {type_name}, got {f}"
        )));
    }
    if f.fract() != 0.0 {
        return Err(E::custom(format!(
            "expected whole number for {type_name}, got {f}"
        )));
    }
    let n = f as u64;
    T::try_from(n).map_err(|_| E::custom(format!("{f} out of range for {type_name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestU32 {
        #[serde(default, deserialize_with = "deserialize_option_u32")]
        val: Option<u32>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestU64 {
        #[serde(default, deserialize_with = "deserialize_option_u64")]
        val: Option<u64>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestUsize {
        #[serde(default, deserialize_with = "deserialize_option_usize")]
        val: Option<usize>,
    }

    fn from_json<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn accepts_integer() {
        let t: TestU32 = from_json(r#"{"val": 8000}"#).unwrap();
        assert_eq!(t.val, Some(8000));
    }

    #[test]
    fn accepts_whole_float() {
        let t: TestU32 = from_json(r#"{"val": 8000.0}"#).unwrap();
        assert_eq!(t.val, Some(8000));
    }

    #[test]
    fn accepts_zero_float() {
        let t: TestU32 = from_json(r#"{"val": 0.0}"#).unwrap();
        assert_eq!(t.val, Some(0));
    }

    #[test]
    fn rejects_fractional_float() {
        let result = from_json::<TestU32>(r#"{"val": 3.14}"#);
        assert!(result.is_err());
    }

    #[test]
    fn accepts_none() {
        let t: TestU32 = from_json(r#"{}"#).unwrap();
        assert_eq!(t.val, None);
    }

    #[test]
    fn u64_whole_float() {
        let t: TestU64 = from_json(r#"{"val": 100000.0}"#).unwrap();
        assert_eq!(t.val, Some(100000));
    }

    #[test]
    fn usize_whole_float() {
        let t: TestUsize = from_json(r#"{"val": 5.0}"#).unwrap();
        assert_eq!(t.val, Some(5));
    }

    #[test]
    fn rejects_negative() {
        let result = from_json::<TestU32>(r#"{"val": -1.0}"#);
        assert!(result.is_err());
    }
}
