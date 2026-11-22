//! Application settings are replaced atomically. Eframe still owns its window metadata.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Preferences {
    path: PathBuf,
    values: BTreeMap<String, String>,
    dirty: bool,
}

impl Preferences {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("preferences.json");
        let values = match std::fs::File::open(&path) {
            Ok(file) => serde_json::from_reader(io::BufReader::new(file)).unwrap_or_else(|e| {
                log::warn!("preferences: invalid settings file: {e}");
                BTreeMap::new()
            }),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("preferences: unable to read settings: {e}");
                }
                BTreeMap::new()
            }
        };
        Self { path, values, dirty: false }
    }

    pub fn get(&self, key: &str, legacy: Option<&dyn eframe::Storage>) -> Option<String> {
        self.values.get(key).cloned().or_else(|| legacy.and_then(|s| s.get_string(key)))
    }

    pub fn edit<'a>(&'a mut self, window: &'a mut dyn eframe::Storage) -> Writer<'a> {
        Writer { preferences: self, window }
    }

    fn commit(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let parent = self.path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        std::fs::create_dir_all(parent)?;
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        {
            let mut writer = io::BufWriter::new(replacement.as_file_mut());
            serde_json::to_writer_pretty(&mut writer, &self.values)?;
            writer.flush()?;
        }
        replacement.as_file().sync_all()?;
        replacement.persist(&self.path)?;
        self.dirty = false;
        Ok(())
    }
}

pub struct Writer<'a> {
    preferences: &'a mut Preferences,
    window: &'a mut dyn eframe::Storage,
}

impl eframe::Storage for Writer<'_> {
    fn get_string(&self, key: &str) -> Option<String> {
        if matches!(key, "window" | "egui") {
            return self.window.get_string(key);
        }
        self.preferences.get(key, Some(self.window))
    }

    fn set_string(&mut self, key: &str, value: String) {
        if matches!(key, "window" | "egui") {
            self.window.set_string(key, value);
        } else if self.preferences.values.get(key) != Some(&value) {
            self.preferences.values.insert(key.to_owned(), value);
            self.preferences.dirty = true;
        }
    }

    fn remove_string(&mut self, key: &str) {
        self.window.remove_string(key);
        self.preferences.dirty |= self.preferences.values.remove(key).is_some();
    }

    fn flush(&mut self) {
        if let Err(e) = self.preferences.commit() {
            log::warn!("preferences: unable to save settings: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_commit_replaces_the_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut prefs = Preferences::load(dir.path());
        for volume in ["0.5", "0.8"] {
            prefs.values.insert("volume".to_owned(), volume.to_owned());
            prefs.dirty = true;
            prefs.commit().unwrap();
            let saved = Preferences::load(dir.path());
            assert_eq!(saved.get("volume", None).as_deref(), Some(volume));
        }
    }
}
