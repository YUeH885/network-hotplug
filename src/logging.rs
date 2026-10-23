use serde_json::{Value, json};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log(level: &str, event: &str, mut fields: Value) {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if let Some(map) = fields.as_object_mut() {
        map.insert("timestamp_ms".into(), json!(timestamp_ms));
        map.insert("level".into(), json!(level));
        map.insert("event".into(), json!(event));
    }
    let stderr = std::io::stderr();
    let mut output = stderr.lock();
    let _ = writeln!(output, "{fields}");
}
