use serde_json::{Value, json};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log(level: &str, event: &str, fields: Value) {
    let mut record = json!({
        "timestamp_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
        "level": level, "event": event
    });
    if let Some(fields) = fields.as_object() {
        record.as_object_mut().unwrap().extend(fields.clone());
    }
    let stderr = std::io::stderr();
    let mut output = stderr.lock();
    let _ = writeln!(output, "{record}");
}
