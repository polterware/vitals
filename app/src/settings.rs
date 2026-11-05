//! The app's settings, in `settings.json` next to the saved hosts: the file the Electron app
//! wrote (`shared/settings.ts`), read the way it read it. A field that is missing or out of
//! range falls back to its default, and fields this app does not know are kept.

use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::protocol::Signal;
use crate::session::Reconnect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkUnit {
    Bytes,
    Bits,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// Opaque window chrome instead of the frosted glass.
    pub reduce_transparency: bool,
    pub network_unit: NetworkUnit,
    /// 2, 5, or 10.
    pub history_window_minutes: u32,
    pub confirm_before_kill: bool,
    pub confirm_before_close_session: bool,
    pub default_kill_signal: Signal,
    pub auto_reconnect: bool,
    /// 1 to 300.
    pub reconnect_delay_seconds: f64,
}

pub const HISTORY_WINDOWS: [u32; 3] = [2, 5, 10];
pub const RECONNECT_DELAY: std::ops::RangeInclusive<f64> = 1.0..=300.0;

impl Default for Settings {
    fn default() -> Self {
        Self {
            reduce_transparency: false,
            network_unit: NetworkUnit::Bytes,
            history_window_minutes: 2,
            confirm_before_kill: true,
            confirm_before_close_session: true,
            default_kill_signal: Signal::Term,
            auto_reconnect: true,
            reconnect_delay_seconds: 3.0,
        }
    }
}

impl Settings {
    /// `normalizeSettings`: each known field when it holds an allowed value, else its default.
    pub fn from_value(value: &Value) -> Self {
        let mut settings = Self::default();
        let Some(object) = value.as_object() else {
            return settings;
        };
        let flag = |key: &str| object.get(key).and_then(Value::as_bool);
        if let Some(on) = flag("reduceTransparency") {
            settings.reduce_transparency = on;
        }
        match object.get("networkUnit").and_then(Value::as_str) {
            Some("bytes") => settings.network_unit = NetworkUnit::Bytes,
            Some("bits") => settings.network_unit = NetworkUnit::Bits,
            _ => {}
        }
        if let Some(minutes) = object
            .get("historyWindowMinutes")
            .and_then(Value::as_f64)
            .filter(|minutes| {
                HISTORY_WINDOWS
                    .iter()
                    .any(|allowed| *minutes == *allowed as f64)
            })
        {
            settings.history_window_minutes = minutes as u32;
        }
        if let Some(on) = flag("confirmBeforeKill") {
            settings.confirm_before_kill = on;
        }
        if let Some(on) = flag("confirmBeforeCloseSession") {
            settings.confirm_before_close_session = on;
        }
        match object.get("defaultKillSignal").and_then(Value::as_str) {
            Some("TERM") => settings.default_kill_signal = Signal::Term,
            Some("KILL") => settings.default_kill_signal = Signal::Kill,
            _ => {}
        }
        if let Some(on) = flag("autoReconnect") {
            settings.auto_reconnect = on;
        }
        if let Some(delay) = object
            .get("reconnectDelaySeconds")
            .and_then(Value::as_f64)
            .filter(|delay| RECONNECT_DELAY.contains(delay))
        {
            settings.reconnect_delay_seconds = delay;
        }
        settings
    }

    /// The fields as the Electron app writes them, over `base` (the file as it was read), so
    /// its other fields, such as `theme`, stay.
    pub fn to_value(&self, base: &Value) -> Value {
        let mut object = base.as_object().cloned().unwrap_or_else(Map::new);
        let fields = json!({
            "reduceTransparency": self.reduce_transparency,
            "networkUnit": match self.network_unit {
                NetworkUnit::Bytes => "bytes",
                NetworkUnit::Bits => "bits",
            },
            "historyWindowMinutes": self.history_window_minutes,
            "confirmBeforeKill": self.confirm_before_kill,
            "confirmBeforeCloseSession": self.confirm_before_close_session,
            "defaultKillSignal": signal_name(self.default_kill_signal),
            "autoReconnect": self.auto_reconnect,
            // A whole number of seconds stays an integer, as JavaScript writes it.
            "reconnectDelaySeconds": if self.reconnect_delay_seconds.fract() == 0.0 {
                json!(self.reconnect_delay_seconds as u64)
            } else {
                json!(self.reconnect_delay_seconds)
            },
        });
        if let Value::Object(fields) = fields {
            object.extend(fields);
        }
        object.entry("theme").or_insert_with(|| json!("dark"));
        Value::Object(object)
    }

    /// Samples a chart keeps: one a second over the history window.
    pub fn history_samples(&self) -> usize {
        self.history_window_minutes as usize * 60
    }

    pub fn reconnect(&self) -> Reconnect {
        Reconnect {
            enabled: self.auto_reconnect,
            delay: Duration::from_secs_f64(self.reconnect_delay_seconds),
        }
    }
}

pub fn signal_name(signal: Signal) -> &'static str {
    match signal {
        Signal::Term => "TERM",
        Signal::Kill => "KILL",
    }
}

/// The settings in `dir`, and the file as it was read (empty when there is none).
pub fn load(dir: &Path) -> (Settings, Value) {
    let raw = std::fs::read_to_string(dir.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    (Settings::from_value(&raw), raw)
}

/// Writes `settings` over `base` into `dir`, through a temporary file; returns what it wrote.
pub fn save(dir: &Path, settings: &Settings, base: &Value) -> std::io::Result<Value> {
    let value = settings.to_value(base);
    std::fs::create_dir_all(dir)?;
    let path = dir.join("settings.json");
    let tmp = dir.join("settings.json.tmp");
    let text = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_electron_wrote() {
        let raw = json!({
            "theme": "dark",
            "reduceTransparency": true,
            "networkUnit": "bits",
            "historyWindowMinutes": 10,
            "confirmBeforeKill": false,
            "confirmBeforeCloseSession": false,
            "defaultKillSignal": "KILL",
            "autoReconnect": false,
            "reconnectDelaySeconds": 30
        });
        let settings = Settings::from_value(&raw);
        assert!(settings.reduce_transparency);
        assert_eq!(settings.network_unit, NetworkUnit::Bits);
        assert_eq!(settings.history_samples(), 600);
        assert!(!settings.confirm_before_kill && !settings.confirm_before_close_session);
        assert_eq!(settings.default_kill_signal, Signal::Kill);
        assert!(!settings.reconnect().enabled);
        assert_eq!(settings.reconnect().delay, Duration::from_secs(30));
        assert_eq!(settings.to_value(&raw), raw);
    }

    #[test]
    fn bad_values_fall_back() {
        let raw = json!({
            "networkUnit": "octets",
            "historyWindowMinutes": 3,
            "defaultKillSignal": "HUP",
            "reconnectDelaySeconds": 0,
            "autoReconnect": "yes"
        });
        assert_eq!(Settings::from_value(&raw), Settings::default());
        assert_eq!(Settings::from_value(&json!([1])), Settings::default());
    }

    #[test]
    fn saving_keeps_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("vitals-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = json!({ "theme": "light", "later": 1 });
        let settings = Settings {
            network_unit: NetworkUnit::Bits,
            ..Settings::default()
        };
        save(&dir, &settings, &base).unwrap();
        let (read, raw) = load(&dir);
        assert_eq!(read, settings);
        assert_eq!(raw["theme"], "light");
        assert_eq!(raw["later"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
