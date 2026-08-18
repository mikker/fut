//! A dependency-free Fut API v1 extension executable.
//!
//! Build with:
//!   rustc --edition=2024 -O -o bin/rust-status src/main.rs

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs::OpenOptions,
    io::{Read, Write},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("rust-status: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mode = env::args().nth(1).ok_or("expected command or hook mode")?;
    let config_source = required_env("FUT_EXTENSION_CONFIG")?;
    let config = JsonParser::parse_object(&config_source)?;
    let label = string_field(&config, "label")?.unwrap_or("rust-status");

    match mode.as_str() {
        "command" => run_command(label, &config_source),
        "hook" => run_hook(label, &config_source),
        _ => Err(format!("unsupported mode {mode:?}").into()),
    }
}

fn run_command(label: &str, config_source: &str) -> Result<()> {
    let command = required_env("FUT_EXTENSION_COMMAND")?;
    record(
        config_source,
        &format!("command={command} label={label} config={config_source}"),
    )?;
    println!("Rust extension {label:?} received command {command:?}.");
    Ok(())
}

fn run_hook(label: &str, config_source: &str) -> Result<()> {
    let event = required_env("FUT_EVENT")?;
    let event_version = required_env("FUT_EVENT_VERSION")?;
    if event_version != "1" {
        return Err(format!("unsupported FUT_EVENT_VERSION {event_version:?}").into());
    }

    let mut payload_source = String::new();
    std::io::stdin().read_to_string(&mut payload_source)?;
    let payload = JsonParser::parse_object(&payload_source)?;
    if integer_field(&payload, "version")? != Some(1) {
        return Err("unsupported or missing hook payload version".into());
    }
    if string_field(&payload, "event")? != Some(event.as_str()) {
        return Err("hook payload event does not match FUT_EVENT".into());
    }

    record(
        config_source,
        &format!(
            "hook={event} label={label} payload={}",
            payload_source.trim()
        ),
    )?;
    publish_workspace_token(&event)
}

fn publish_workspace_token(value: &str) -> Result<()> {
    let status = Command::new(required_env("FUT_BIN")?)
        .args([
            "--socket",
            &required_env("FUT_SOCKET")?,
            "token",
            "publish",
            &required_env("FUT_EXTENSION_ID")?,
            "last_event",
            value,
            "--workspace-id",
            &required_env("FUT_WORKSPACE_ID")?,
        ])
        .status()?;
    if !status.success() {
        return Err(format!("fut token publish exited with {status}").into());
    }
    Ok(())
}

fn record(config_source: &str, line: &str) -> Result<()> {
    let config = JsonParser::parse_object(config_source)?;
    let Some(path) = string_field(&config, "log_path")? else {
        return Ok(());
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).map_err(|_| {
        format!("required environment variable {name} is not set or is not UTF-8").into()
    })
}

#[derive(Debug)]
enum JsonValue {
    String(String),
    Integer(u64),
    Object(BTreeMap<String, JsonValue>),
    Other,
}

fn string_field<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    name: &str,
) -> Result<Option<&'a str>> {
    match object.get(name) {
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("JSON field {name:?} must be a string").into()),
        None => Ok(None),
    }
}

fn integer_field(object: &BTreeMap<String, JsonValue>, name: &str) -> Result<Option<u64>> {
    match object.get(name) {
        Some(JsonValue::Integer(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("JSON field {name:?} must be a non-negative integer").into()),
        None => Ok(None),
    }
}

/// Small JSON reader for the two top-level string/integer fields this example
/// consumes. It still validates and skips nested JSON instead of matching text.
struct JsonParser<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> JsonParser<'a> {
    fn parse_object(source: &'a str) -> Result<BTreeMap<String, JsonValue>> {
        let mut parser = Self {
            bytes: source.as_bytes(),
            offset: 0,
        };
        let JsonValue::Object(object) = parser.value()? else {
            return Err("expected a JSON object".into());
        };
        parser.whitespace();
        if parser.offset != parser.bytes.len() {
            return Err("trailing bytes after JSON value".into());
        }
        Ok(object)
    }

    fn value(&mut self) -> Result<JsonValue> {
        self.whitespace();
        match self.peek() {
            Some(b'"') => Ok(JsonValue::String(self.string()?)),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err("expected a JSON value".into()),
        }
    }

    fn object(&mut self) -> Result<JsonValue> {
        self.expect(b'{')?;
        let mut object = BTreeMap::new();
        self.whitespace();
        if self.take(b'}') {
            return Ok(JsonValue::Object(object));
        }
        loop {
            let key = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            if object.insert(key, self.value()?).is_some() {
                return Err("duplicate JSON object key".into());
            }
            self.whitespace();
            if self.take(b'}') {
                return Ok(JsonValue::Object(object));
            }
            self.expect(b',')?;
            self.whitespace();
        }
    }

    fn array(&mut self) -> Result<JsonValue> {
        self.expect(b'[')?;
        self.whitespace();
        if self.take(b']') {
            return Ok(JsonValue::Other);
        }
        loop {
            self.value()?;
            self.whitespace();
            if self.take(b']') {
                return Ok(JsonValue::Other);
            }
            self.expect(b',')?;
        }
    }

    fn string(&mut self) -> Result<String> {
        self.whitespace();
        self.expect(b'"')?;
        let mut decoded = String::new();
        let mut chunk = self.offset;
        loop {
            match self.peek().ok_or("unterminated JSON string")? {
                b'"' => {
                    decoded.push_str(std::str::from_utf8(&self.bytes[chunk..self.offset])?);
                    self.offset += 1;
                    return Ok(decoded);
                }
                b'\\' => {
                    decoded.push_str(std::str::from_utf8(&self.bytes[chunk..self.offset])?);
                    self.offset += 1;
                    let escape = self.next().ok_or("unterminated JSON escape")?;
                    match escape {
                        b'"' => decoded.push('"'),
                        b'\\' => decoded.push('\\'),
                        b'/' => decoded.push('/'),
                        b'b' => decoded.push('\u{0008}'),
                        b'f' => decoded.push('\u{000c}'),
                        b'n' => decoded.push('\n'),
                        b'r' => decoded.push('\r'),
                        b't' => decoded.push('\t'),
                        b'u' => decoded.push(self.unicode_escape()?),
                        _ => return Err("invalid JSON escape".into()),
                    }
                    chunk = self.offset;
                }
                0x00..=0x1f => return Err("control byte in JSON string".into()),
                _ => self.offset += 1,
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char> {
        let first = self.hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            self.expect(b'\\')?;
            self.expect(b'u')?;
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err("invalid JSON surrogate pair".into());
            }
            0x10000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else {
            u32::from(first)
        };
        char::from_u32(scalar).ok_or_else(|| "invalid JSON Unicode escape".into())
    }

    fn hex_quad(&mut self) -> Result<u16> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let digit = self.next().ok_or("short JSON Unicode escape")?;
            value = value * 16
                + u16::from(
                    (digit as char)
                        .to_digit(16)
                        .ok_or("invalid JSON Unicode escape")? as u8,
                );
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<JsonValue> {
        let start = self.offset;
        while self.peek().is_some_and(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')
        }) {
            self.offset += 1;
        }
        let source = std::str::from_utf8(&self.bytes[start..self.offset])?;
        source.parse::<f64>().map_err(|_| "invalid JSON number")?;
        Ok(source
            .parse::<u64>()
            .map(JsonValue::Integer)
            .unwrap_or(JsonValue::Other))
    }

    fn literal(&mut self, literal: &[u8]) -> Result<JsonValue> {
        if self.bytes.get(self.offset..self.offset + literal.len()) != Some(literal) {
            return Err("invalid JSON literal".into());
        }
        self.offset += literal.len();
        Ok(JsonValue::Other)
    }

    fn whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.offset += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<()> {
        if self.take(expected) {
            Ok(())
        } else {
            Err(format!("expected JSON byte {:?}", char::from(expected)).into())
        }
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.offset += 1;
        Some(byte)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }
}
