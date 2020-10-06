use std::any::Any;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::{fmt, fs, io};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::commons::util::file;

//------------ KeyStoreKey ---------------------------------------------------

#[derive(Debug, Clone)]
pub struct KeyStoreKey {
    scope: Option<String>,
    name: String,
}

impl KeyStoreKey {
    pub fn simple(name: String) -> Self {
        KeyStoreKey { scope: None, name }
    }

    pub fn scoped(scope: String, name: String) -> Self {
        KeyStoreKey {
            scope: Some(scope),
            name,
        }
    }

    pub fn scope(&self) -> Option<&String> {
        self.scope.as_ref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn sub_scope(&self, new: &str) -> Self {
        if new.is_empty() {
            self.clone()
        } else {
            let scope = match self.scope.as_ref() {
                Some(existing) => format!("{}/{}", existing, new),
                None => new.to_string(),
            };
            KeyStoreKey {
                scope: Some(scope),
                name: self.name.clone(),
            }
        }
    }

    pub fn archived(&self) -> Self {
        self.sub_scope("archived")
    }

    pub fn corrupt(&self) -> Self {
        self.sub_scope("corrupt")
    }

    pub fn surplus(&self) -> Self {
        self.sub_scope("surplus")
    }
}

impl fmt::Display for KeyStoreKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.scope.as_ref() {
            Some(scope) => write!(f, "{}/{}", scope, self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// Using an enum here, because we expect to have more implementations in future.
/// Not using generics because it's harder on the compiler.
pub enum KeyValueStore {
    Disk(KeyValueStoreDiskImpl),
}

impl KeyValueStore {
    pub fn disk(workdir: &PathBuf, name_space: &str) -> Result<Self, KeyValueError> {
        let mut base = workdir.clone();
        base.push(name_space);

        if !base.exists() {
            file::create_dir(&base)?;
        }

        Ok(KeyValueStore::Disk(KeyValueStoreDiskImpl { base }))
    }

    /// Stores a key value pair, serialized as json, overwrite existing
    pub fn store<V: Any + Serialize>(&self, key: &KeyStoreKey, value: &V) -> Result<(), KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.store(key, value),
        }
    }

    /// Stores a new key value pair, returns an error if the key exists
    pub fn store_new<V: Any + Serialize>(&self, key: &KeyStoreKey, value: &V) -> Result<(), KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.store_new(key, value),
        }
    }

    /// Gets a value for a key, returns an error if the value cannot be deserialized,
    /// returns None if it cannot be found.
    pub fn get<V: DeserializeOwned>(&self, key: &KeyStoreKey) -> Result<Option<V>, KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.get(key),
        }
    }

    /// Returns whether a key exists
    pub fn has(&self, key: &KeyStoreKey) -> Result<bool, KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => Ok(disk_store.has(key)),
        }
    }

    /// Delete a key-value pair
    pub fn drop(&self, key: &KeyStoreKey) -> Result<(), KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.drop(key),
        }
    }

    /// Move a value from one key to another
    pub fn move_key(&self, from: &KeyStoreKey, to: &KeyStoreKey) -> Result<(), KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.move_key(from, to),
        }
    }

    /// Archive a key
    pub fn archive(&self, key: &KeyStoreKey) -> Result<(), KeyValueError> {
        self.move_key(key, &key.archived())
    }

    /// Archive a key as corrupt
    pub fn archive_corrupt(&self, key: &KeyStoreKey) -> Result<(), KeyValueError> {
        self.move_key(key, &key.corrupt())
    }

    /// Archive a key as surplus
    pub fn archive_surplus(&self, key: &KeyStoreKey) -> Result<(), KeyValueError> {
        self.move_key(key, &key.surplus())
    }

    /// Returns all 1st level scopes
    pub fn scopes(&self) -> Result<Vec<String>, KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.scopes(),
        }
    }

    /// Returns whether a scope exists
    pub fn has_scope(&self, scope: String) -> Result<bool, KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.has_scope(scope),
        }
    }

    /// Returns all keys under a scope (scopes are exact strings, 'sub'-scopes
    /// would need to be specified explicitly.. e.g. 'ca' and 'ca/archived' are
    /// two distinct scopes.
    ///
    /// If matching is not empty then the key must contain the given `&str`.
    pub fn keys(&self, scope: Option<String>, matching: &str) -> Result<Vec<KeyStoreKey>, KeyValueError> {
        match self {
            KeyValueStore::Disk(disk_store) => disk_store.keys(scope, matching),
        }
    }
}

impl KeyValueStore {}

/// This type can store and retrieve values to/from disk, using json
/// serialization
pub struct KeyValueStoreDiskImpl {
    base: PathBuf,
}

impl KeyValueStoreDiskImpl {
    fn file_path(&self, key: &KeyStoreKey) -> PathBuf {
        let mut path = self.scope_path(key.scope.as_ref());
        path.push(key.name());
        path
    }

    fn scope_path(&self, scope: Option<&String>) -> PathBuf {
        let mut path = self.base.clone();
        if let Some(scope) = scope {
            path.push(scope);
        }
        path
    }

    fn store<V: Any + Serialize>(&self, key: &KeyStoreKey, value: &V) -> Result<(), KeyValueError> {
        let mut f = file::create_file_with_path(&self.file_path(key))?;
        let json = serde_json::to_string_pretty(value)?;
        f.write_all(json.as_ref())?;
        Ok(())
    }

    fn store_new<V: Any + Serialize>(&self, key: &KeyStoreKey, value: &V) -> Result<(), KeyValueError> {
        let path = self.file_path(key);
        if path.exists() {
            Err(KeyValueError::DuplicateKey(key.clone()))
        } else {
            let mut f = file::create_file_with_path(&path)?;
            let json = serde_json::to_string_pretty(value)?;
            f.write_all(json.as_ref())?;
            Ok(())
        }
    }

    fn get<V: DeserializeOwned>(&self, key: &KeyStoreKey) -> Result<Option<V>, KeyValueError> {
        let path = self.file_path(key);
        let path_str = path.to_string_lossy().into_owned();

        if path.exists() {
            let f = File::open(path)?;
            let v = serde_json::from_reader(f)?;
            Ok(Some(v))
        } else {
            trace!("Could not find file at: {}", path_str);
            Ok(None)
        }
    }

    pub fn has(&self, key: &KeyStoreKey) -> bool {
        let path = self.file_path(key);
        path.exists()
    }

    pub fn drop(&self, key: &KeyStoreKey) -> Result<(), KeyValueError> {
        let path = self.file_path(key);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn move_key(&self, from: &KeyStoreKey, to: &KeyStoreKey) -> Result<(), KeyValueError> {
        let from_path = self.file_path(from);
        let to_path = self.file_path(to);

        if !from_path.exists() {
            Err(KeyValueError::UnknownKey(from.clone()))
        } else {
            if let Some(parent) = to_path.parent() {
                if !parent.exists() {
                    fs::create_dir(parent)?;
                }
            }

            fs::rename(from_path, to_path)?;

            Ok(())
        }
    }

    fn has_scope(&self, scope: String) -> Result<bool, KeyValueError> {
        Ok(self.scope_path(Some(&scope)).exists())
    }

    fn scopes(&self) -> Result<Vec<String>, KeyValueError> {
        Self::read_dir(&self.base, false, true)
    }

    fn keys(&self, scope: Option<String>, matching: &str) -> Result<Vec<KeyStoreKey>, KeyValueError> {
        let path = self.scope_path(scope.as_ref());

        let mut res = vec![];
        for name in Self::read_dir(&path, true, false)? {
            if matching.is_empty() || name.contains(matching) {
                match scope.as_ref() {
                    None => res.push(KeyStoreKey::simple(name)),
                    Some(scope) => res.push(KeyStoreKey::scoped(scope.clone(), name)),
                }
            }
        }

        Ok(res)
    }

    fn read_dir(dir: &PathBuf, files: bool, dirs: bool) -> Result<Vec<String>, KeyValueError> {
        match fs::read_dir(dir) {
            Err(e) => Err(KeyValueError::IoError(e)),
            Ok(dir) => {
                let mut res = vec![];

                for d in dir {
                    if let Ok(d) = d {
                        let path = d.path();
                        if (dirs && path.is_dir()) || (files && path.is_file()) {
                            if let Some(name) = path.file_name() {
                                res.push(name.to_string_lossy().to_string())
                            }
                        }
                    }
                }

                Ok(res)
            }
        }
    }
}

//------------ KeyValueError -------------------------------------------------

/// This type defines possible Errors for KeyStore
#[derive(Debug)]
pub enum KeyValueError {
    IoError(io::Error),
    JsonError(serde_json::Error),
    UnknownKey(KeyStoreKey),
    DuplicateKey(KeyStoreKey),
}

impl From<io::Error> for KeyValueError {
    fn from(e: io::Error) -> Self {
        KeyValueError::IoError(e)
    }
}

impl From<serde_json::Error> for KeyValueError {
    fn from(e: serde_json::Error) -> Self {
        KeyValueError::JsonError(e)
    }
}

impl fmt::Display for KeyValueError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            KeyValueError::IoError(e) => write!(f, "I/O error: {}", e),
            KeyValueError::JsonError(e) => write!(f, "JSON error: {}", e),
            KeyValueError::UnknownKey(key) => write!(f, "Unknown key: {}", key),
            KeyValueError::DuplicateKey(key) => write!(f, "Duplicate key: {}", key),
        }
    }
}

//------------ Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;
    use crate::test;

    #[test]
    fn diskstore_move_key() {
        test::test_under_tmp(|d| {
            let store = KeyValueStore::disk(&d, "store").unwrap();

            let content = "abc".to_string();
            let id = "id".to_string();
            let key = KeyStoreKey::simple(id.clone());
            let target = key.archived();

            store.store(&key, &content).unwrap();

            let mut expected_file_path = d.clone();
            expected_file_path.push("store");
            expected_file_path.push("id");
            assert!(expected_file_path.exists());

            store.move_key(&key, &target).unwrap();

            let mut expected_target = d.clone();
            expected_target.push("store");
            expected_target.push("archived");
            expected_target.push("id");

            assert!(!expected_file_path.exists());
            assert!(expected_target.exists());
        })
    }
}
