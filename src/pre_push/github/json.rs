//! JSON decoding which retains duplicate object members as errors.
//!
//! `serde_json::Value` normally keeps only the final value for a duplicate
//! object key. GraphQL aliases and receipt fields carry authority, so every
//! response crosses this boundary before it can become an ordinary value.

use std::fmt;

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};

pub(super) struct UniqueJson(Value);

impl UniqueJson {
    pub(super) fn decode(response: &[u8]) -> serde_json::Result<Self> {
        let mut deserializer = serde_json::Deserializer::from_slice(response);
        let value = Self::deserialize(&mut deserializer)?;
        deserializer.end()?;
        Ok(value)
    }

    pub(super) fn as_value(&self) -> &Value {
        &self.0
    }

    pub(super) fn into_value(self) -> Value {
        self.0
    }
}

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueJsonVisitor;

        impl<'de> Visitor<'de> for UniqueJsonVisitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one JSON value with unique object members")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(value.into())))
            }

            fn visit_i128<E>(self, value: i128) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let deserializer = de::value::I128Deserializer::new(value);
                Number::deserialize(deserializer).map(|number| UniqueJson(Value::Number(number)))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Number(value.into())))
            }

            fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let deserializer = de::value::U128Deserializer::new(value);
                Number::deserialize(deserializer).map(|number| UniqueJson(Value::Number(number)))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let number = Number::from_f64(value)
                    .ok_or_else(|| de::Error::custom("non-finite JSON number"))?;
                Ok(UniqueJson(Value::Number(number)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_string(value.to_owned())
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                UniqueJson::deserialize(deserializer)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = sequence.next_element::<UniqueJson>()? {
                    values.push(value);
                }
                Ok(UniqueJson(Value::Array(values)))
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON object member"));
                    }
                    let UniqueJson(value) = object.next_value::<UniqueJson>()?;
                    assert!(values.insert(key, value).is_none());
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_members_at_every_depth_and_trailing_values() {
        for response in [
            br#"{"one": 1, "one": 2}"#.as_slice(),
            br#"{"outer": {"one": 1, "one": 2}}"#,
            br#"[{"one": 1, "one": 2}]"#,
            br#"null null"#,
        ] {
            assert!(UniqueJson::decode(response).is_err());
        }
    }

    #[test]
    fn preserves_the_parser_recursion_limit() {
        let depth = 256;
        let response = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        assert!(UniqueJson::decode(response.as_bytes()).is_err());
    }
}
