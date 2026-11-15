//! What the application remembers between runs: `ui-settings.json` in the state folder (the
//! same file and keys the earlier application used, plus a few of its own).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use amui::config::Config;
use amui::util;
use gtk::glib;
use serde_json::{Map, Value};

pub struct Settings {
    path: Option<PathBuf>,
    values: RefCell<Map<String, Value>>,
    scheduled: Cell<bool>,
}

impl Settings {
    /// The saved settings; `persistent == false` (the demonstration) keeps them in memory only.
    pub fn load(config: &Config, persistent: bool) -> Rc<Settings> {
        let path = persistent.then(|| config.state_dir.join("ui-settings.json"));
        let values = path.as_deref().and_then(util::read_json_object).unwrap_or_default();
        Rc::new(Settings { path, values: RefCell::new(values), scheduled: Cell::new(false) })
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.values.borrow().get(key).and_then(Value::as_bool).unwrap_or(default)
    }

    pub fn number(&self, key: &str, default: f64) -> f64 {
        self.values.borrow().get(key).and_then(Value::as_f64).unwrap_or(default)
    }

    pub fn text(&self, key: &str, default: &str) -> String {
        self.values.borrow().get(key).and_then(Value::as_str).unwrap_or(default).to_string()
    }

    /// Remember a value; the file is written shortly afterwards (one write for many changes).
    pub fn set(self: &Rc<Self>, key: &str, value: impl Into<Value>) {
        let value = value.into();
        if self.values.borrow().get(key) == Some(&value) {
            return;
        }
        self.values.borrow_mut().insert(key.to_string(), value);
        if self.path.is_some() && !self.scheduled.replace(true) {
            let settings = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(400), move || settings.save());
        }
    }

    /// Write the file now (also called when the application quits).
    pub fn save(&self) {
        self.scheduled.set(false);
        if let Some(path) = &self.path {
            let _ = util::write_private_json(path, &Value::Object(self.values.borrow().clone()));
        }
    }
}
