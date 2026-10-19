//! Bounded JSON with duplicate rejection even in ignored metadata.
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use super::{Error, MAX_BODY, Projection};

struct Seed<'a> {
    remaining: &'a mut usize,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, decoder: D) -> Result<Value, D::Error> {
        if self.depth > 32 || *self.remaining == 0 {
            return Err(de::Error::custom("JSON budget"));
        }
        *self.remaining -= 1;
        decoder.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            remaining: self.remaining,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if *self.remaining == 0 || values.contains_key(&key) {
                return Err(de::Error::custom("JSON key"));
            }
            *self.remaining -= 1;
            let value = map.next_value_seed(Seed {
                remaining: self.remaining,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub(super) fn read(body: &[u8]) -> Result<Projection, Error> {
    if body.len() > MAX_BODY {
        return Err(Error::InvalidResponse);
    }
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let root = Seed {
        remaining: &mut 4096,
        depth: 1,
    }
    .deserialize(&mut decoder)
    .map_err(|_| Error::InvalidResponse)?;
    decoder.end().map_err(|_| Error::InvalidResponse)?;
    if !root.is_object() {
        return Err(Error::InvalidResponse);
    }
    let projection: Projection =
        serde_json::from_value(root).map_err(|_| Error::InvalidResponse)?;
    let id = &projection.session_id;
    if id.trim().is_empty() || id.len() > 2048 || id.chars().any(char::is_control) {
        return Err(Error::InvalidResponse);
    }
    Ok(projection)
}
