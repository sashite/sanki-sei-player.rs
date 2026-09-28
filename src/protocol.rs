//! The SEI transport and envelope (§5–§7), the three small requests (`hello`,
//! `ping`, `configure`, `cancel`) and the events the engine emits (§9).
//!
//! Requests are **strict**: a duplicate key, a number outside I-JSON's
//! integer range, an unknown field or an unknown operation is refused (§6.4).
//! What is wrong with the *envelope* is fatal — an error without `re`, then
//! exit — and what is wrong with the *content* is attached to the request.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Map, Value};

/// The identifier the engine announces (SEI §3; *SEI Rules Document — Sanki*).
pub const RULES: &str = "sashite.sanki.kernel/1";
/// The only protocol version this engine speaks.
pub const VERSION: i64 = 1;
/// The largest integer a request may carry (I-JSON, SEI §6.3).
pub const MAX_INT: i64 = (1 << 53) - 1;
/// The domain of the `seed` option: `0` means "from entropy".
pub const SEED_MAX: i64 = i32::MAX as i64;

/// The `depth` option's default: the brain's own.
pub const DEPTH_DEFAULT: u8 = 4;

/// The `depth` option's ceiling.
pub const DEPTH_MAX: u8 = 12;

/// A request whose envelope was valid: its `id`, its `op`, and its other
/// fields, for the operation to validate.
pub struct Request {
    /// The request's `id`.
    pub id: i64,
    /// The request's `op`.
    pub op: String,
    /// Every field but `id` and `op`.
    pub fields: Map<String, Value>,
}

/// An envelope error: fatal (SEI §6.5).
pub struct Envelope(pub String);

/// A content error, attached to its request (SEI §9.4).
pub struct Error {
    code: &'static str,
    path: Option<String>,
    message: String,
}

impl Error {
    /// `invalid`: malformed, out of domain, or not negotiated.
    pub fn invalid(message: impl Into<String>, path: Option<&str>) -> Self {
        Self::new("invalid", message, path)
    }

    /// `illegal`: an illegal or non-canonical position or Move.
    pub fn illegal(message: impl Into<String>, path: Option<&str>) -> Self {
        Self::new("illegal", message, path)
    }

    /// `unsupported`: rules or a pairing this engine does not implement.
    pub fn unsupported(message: impl Into<String>, path: Option<&str>) -> Self {
        Self::new("unsupported", message, path)
    }

    /// `internal`: the request failed for a cause of the engine's own,
    /// before its search started.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message, None)
    }

    fn new(code: &'static str, message: impl Into<String>, path: Option<&str>) -> Self {
        Self {
            code,
            path: path.map(str::to_owned),
            message: message.into(),
        }
    }

    /// The error event attached to request `id`.
    pub fn attached(self, id: i64) -> Event {
        let mut object =
            json!({ "re": id, "ev": "error", "code": self.code, "message": self.message });
        if let (Some(path), Some(fields)) = (self.path, object.as_object_mut()) {
            fields.insert("path".to_owned(), Value::String(path));
        }
        Event(object)
    }
}

/// One event, ready to be written as one line.
pub struct Event(Value);

impl Event {
    /// An empty `done` for request `id`.
    pub fn done(id: i64) -> Self {
        Self(json!({ "re": id, "ev": "done" }))
    }

    /// Any event from its fields; `re` and `ev` included by the caller.
    pub fn from_value(value: Value) -> Self {
        Self(value)
    }

    /// The fatal error: no `re` (SEI §7 rule 7); `invalid` for an envelope
    /// error, `internal` for a failure of the engine itself.
    pub fn fatal(code: &str, message: &str) -> Self {
        Self(json!({ "ev": "error", "code": code, "message": message }))
    }

    /// Writes the event as one line and flushes. `Err(())` when the output is
    /// gone, which ends the session.
    pub fn write(&self, out: &mut impl Write) -> Result<(), ()> {
        let mut line = self.0.to_string();
        line.push('\n');
        out.write_all(line.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|_| ())
    }
}

// ---- parsing ------------------------------------------------------------------

/// Reads one line as a request with a valid envelope, or the envelope error
/// that ends the session.
pub fn parse(text: &str, last_id: Option<i64>, opened: bool) -> Result<Request, Envelope> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| Envelope(format!("not an I-JSON object: {e}")))?;
    let Value::Object(mut fields) = value else {
        return Err(Envelope(
            "not an I-JSON object: the line is not an object".to_owned(),
        ));
    };
    Strict::check(text)?;
    let id = match fields.remove("id") {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(id) if (0..=MAX_INT).contains(&id) => id,
            _ => {
                return Err(Envelope(
                    "`id` is not an integer in [0, 2^53 − 1]".to_owned(),
                ))
            }
        },
        Some(_) => return Err(Envelope("`id` is not an integer".to_owned())),
        None => return Err(Envelope("`id` is missing".to_owned())),
    };
    if let Some(last) = last_id {
        if id <= last {
            return Err(Envelope(format!(
                "`id` {id} does not increase (last was {last})"
            )));
        }
    }
    let op = match fields.remove("op") {
        Some(Value::String(op)) => op,
        Some(_) => return Err(Envelope("`op` is not a string".to_owned())),
        None => return Err(Envelope("`op` is missing".to_owned())),
    };
    if !opened && op != "hello" && op != "ping" {
        return Err(Envelope(format!("`{op}` before a successful `hello`")));
    }
    if opened && op == "hello" {
        return Err(Envelope("`hello` after a successful `hello`".to_owned()));
    }
    Ok(Request { id, op, fields })
}

/// The I-JSON checks `serde_json` does not make: no duplicate key in any
/// object, and every number an integer within ±(2^53 − 1). Implemented as a
/// visitor that walks the text a second time and keeps nothing.
struct Strict;

impl Strict {
    fn check(text: &str) -> Result<(), Envelope> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        StrictValue::deserialize(&mut deserializer)
            .map(|_| ())
            .map_err(|e| Envelope(format!("not an I-JSON object: {e}")))
    }
}

struct StrictValue;

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an I-JSON value")
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        if v.abs() > MAX_INT {
            return Err(E::custom("number outside ±(2^53 − 1)"));
        }
        Ok(StrictValue)
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        if v > MAX_INT as u64 {
            return Err(E::custom("number outside ±(2^53 − 1)"));
        }
        Ok(StrictValue)
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        // A fraction or an exponent is a content error, attached to its
        // request by the field's check; only the I-JSON range is the envelope's.
        if !v.is_finite() || v.abs() > MAX_INT as f64 {
            return Err(E::custom("number outside ±(2^53 − 1)"));
        }
        Ok(StrictValue)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<StrictValue>()?.is_some() {}
        Ok(StrictValue)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate key `{key}`")));
            }
            map.next_value::<StrictValue>()?;
        }
        Ok(StrictValue)
    }
}

// ---- field helpers ---------------------------------------------------------------

/// `invalid` for any field of `fields` outside `allowed` (strict requests).
pub fn only(fields: &Map<String, Value>, allowed: &[&str], prefix: &str) -> Result<(), Error> {
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(Error::invalid(
                format!("unknown field `{key}`"),
                Some(&format!("{prefix}/{key}")),
            ));
        }
    }
    Ok(())
}

/// An integer field within `[min, max]`, or `None` when absent.
pub fn int(
    fields: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
    prefix: &str,
) -> Result<Option<i64>, Error> {
    let path = format!("{prefix}/{key}");
    match fields.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(v) if (min..=max).contains(&v) => Ok(Some(v)),
            _ => Err(Error::invalid(
                format!("`{key}` must be an integer in [{min}, {max}]"),
                Some(&path),
            )),
        },
        Some(_) => Err(Error::invalid(
            format!("`{key}` must be an integer"),
            Some(&path),
        )),
    }
}

/// A boolean field, or `None` when absent.
pub fn boolean(
    fields: &Map<String, Value>,
    key: &str,
    prefix: &str,
) -> Result<Option<bool>, Error> {
    match fields.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(Error::invalid(
            format!("`{key}` must be a boolean"),
            Some(&format!("{prefix}/{key}")),
        )),
    }
}

/// A required object field.
pub fn object<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
    prefix: &str,
) -> Result<Option<&'a Map<String, Value>>, Error> {
    match fields.get(key) {
        None => Ok(None),
        Some(Value::Object(o)) => Ok(Some(o)),
        Some(_) => Err(Error::invalid(
            format!("`{key}` must be an object"),
            Some(&format!("{prefix}/{key}")),
        )),
    }
}

/// An array of strings, or `None` when absent.
pub fn strings<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
    prefix: &str,
) -> Result<Option<Vec<&'a str>>, Error> {
    match fields.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                item.as_str().ok_or_else(|| {
                    Error::invalid("expected a string", Some(&format!("{prefix}/{key}/{i}")))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(Error::invalid(
            format!("`{key}` must be an array"),
            Some(&format!("{prefix}/{key}")),
        )),
    }
}

// ---- the small requests -----------------------------------------------------------

/// `ping` carries nothing but `id` and `op` (§8.2).
pub fn check_ping(request: &Request) -> Result<(), Error> {
    only(&request.fields, &[], "")
}

/// `hello`: `versions` non-empty, integers ≥ 1; every other field ignored
/// (§8.1). Returns whether version 1 is common — the only case the opening
/// succeeds.
pub fn check_hello(request: &Request) -> Result<bool, Error> {
    match request.fields.get("versions") {
        Some(Value::Array(versions)) if !versions.is_empty() => {
            let mut common = false;
            for (i, v) in versions.iter().enumerate() {
                match v.as_i64() {
                    Some(n) if n >= 1 => common |= n == VERSION,
                    _ => {
                        return Err(Error::invalid(
                            "a version must be an integer ≥ 1",
                            Some(&format!("/versions/{i}")),
                        ))
                    }
                }
            }
            Ok(common)
        }
        _ => Err(Error::invalid(
            "`versions` must be a non-empty array of integers",
            Some("/versions"),
        )),
    }
}

/// The `done` of `hello`: the engine, its rules, features and options (§8.1).
/// `version` is present only when the host speaks 1.
pub fn hello_done(id: i64, common: bool) -> Event {
    let mut done = json!({
        "re": id,
        "ev": "done",
        "versions": [VERSION],
        "engine": {
            "name": "sanki-sei-player",
            "version": env!("CARGO_PKG_VERSION"),
            "author": "Cyril Kato",
            "about": "The reference search of sashite-sanki-player, under SEI's clock."
        },
        "rules": { RULES: {} },
        "features": { "roots": {} },
        "options": {
            "depth": {
                "type": "int", "default": DEPTH_DEFAULT, "min": 1, "max": DEPTH_MAX,
                "about": "The iterative-deepening ceiling under a clock, and of an infinite search; limits alone bind on their own."
            },
            "seed": {
                "type": "int", "default": 0, "min": 0, "max": SEED_MAX,
                "about": "Breaks the ties among equal-best root moves; 0 draws one from the clock."
            }
        }
    });
    if let (true, Some(fields)) = (common, done.as_object_mut()) {
        fields.insert("version".to_owned(), json!(VERSION));
    }
    Event(done)
}

/// The effective configuration (§8.3): every option at its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The iterative-deepening ceiling.
    pub depth: u8,
    /// The tie-breaking seed; `0` draws one from the clock.
    pub seed: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            depth: DEPTH_DEFAULT,
            seed: 0,
        }
    }
}

/// `configure`: the complete configuration; an option left out takes its
/// default (§8.3).
pub fn check_configure(request: &Request) -> Result<Options, Error> {
    only(&request.fields, &["options"], "")?;
    let Some(options) = object(&request.fields, "options", "")? else {
        return Ok(Options::default());
    };
    only(options, &["depth", "seed"], "/options")?;
    let depth = int(options, "depth", 1, i64::from(DEPTH_MAX), "/options")?
        .unwrap_or(i64::from(DEPTH_DEFAULT));
    let seed = int(options, "seed", 0, SEED_MAX, "/options")?.unwrap_or(0);
    Ok(Options {
        depth: u8::try_from(depth).unwrap_or(DEPTH_DEFAULT),
        seed: u32::try_from(seed).unwrap_or(0),
    })
}

/// `cancel` carries nothing else (§8.5).
pub fn check_cancel(request: &Request) -> Result<(), Error> {
    only(&request.fields, &[], "")
}

/// A JSON Pointer to element `i` of `key`.
pub fn pointer(key: &str, i: usize) -> String {
    let mut s = String::new();
    let _ = write!(s, "/{key}/{i}");
    s
}
