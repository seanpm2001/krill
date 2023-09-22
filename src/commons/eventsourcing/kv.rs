use std::{fmt, str::FromStr};

pub use kvx::{namespace, segment, Key, Namespace, Scope, Segment, SegmentBuf};
use kvx::{KeyValueStoreBackend, NamespaceBuf, ReadStore, WriteStore};
use serde::{de::DeserializeOwned, Serialize};
use url::Url;

use crate::commons::error::KrillIoError;

pub trait SegmentExt {
    fn parse_lossy(value: &str) -> SegmentBuf;
    fn concat(lhs: impl Into<SegmentBuf>, rhs: impl Into<SegmentBuf>) -> SegmentBuf;
}

impl SegmentExt for Segment {
    fn parse_lossy(value: &str) -> SegmentBuf {
        match Segment::parse(value) {
            Ok(segment) => segment.to_owned(),
            Err(error) => {
                let sanitized = value.trim().replace(Scope::SEPARATOR, "+");
                let nonempty = sanitized.is_empty().then(|| "EMPTY".to_owned()).unwrap_or(sanitized);
                let segment = Segment::parse(&nonempty).unwrap(); // cannot panic as all checks are performed above
                warn!("{value} is not a valid Segment: {error}\nusing {segment} instead");
                segment.to_owned()
            }
        }
    }

    fn concat(lhs: impl Into<SegmentBuf>, rhs: impl Into<SegmentBuf>) -> SegmentBuf {
        Segment::parse(&format!("{}{}", lhs.into(), rhs.into()))
            .unwrap()
            .to_owned()
    }
}

#[derive(Debug)]
pub struct KeyValueStore {
    inner: kvx::KeyValueStore,
}

impl KeyValueStore {
    /// Creates a new KeyValueStore.
    pub fn create(storage_uri: &Url, namespace: &Namespace) -> Result<Self, KeyValueError> {
        kvx::KeyValueStore::new(storage_uri, namespace)
            .map(|inner| KeyValueStore { inner })
            .map_err(KeyValueError::KVError)
    }

    /// Creates a new KeyValueStore for upgrades.
    ///
    /// Adds the implicit prefix "upgrade-{version}-" to the given namespace.
    pub fn create_upgrade_store(storage_uri: &Url, namespace: &Namespace) -> Result<Self, KeyValueError> {
        let namespace = Self::prefixed_namespace(namespace, "upgrade")?;

        kvx::KeyValueStore::new(storage_uri, namespace)
            .map(|inner| KeyValueStore { inner })
            .map_err(KeyValueError::KVError)
    }

    fn prefixed_namespace(namespace: &Namespace, prefix: &str) -> Result<NamespaceBuf, KeyValueError> {
        let namespace_string = format!("{}_{}", prefix, namespace);
        NamespaceBuf::from_str(&namespace_string)
            .map_err(|e| KeyValueError::Other(format!("Cannot parse namespace: {}. Error: {}", namespace_string, e)))
    }

    /// Archive this store (i.e. for this namespace). Deletes
    /// any existing archive for this namespace if present.
    pub fn migrate_to_archive(&mut self, storage_uri: &Url, namespace: &Namespace) -> Result<(), KeyValueError> {
        let archive_ns = Self::prefixed_namespace(namespace, "archive")?;
        // Wipe any existing archive, before archiving this store.
        // We don't want to keep too much old data. See issue: #1088.
        let archive_store = KeyValueStore::create(storage_uri, namespace)?;
        archive_store.wipe()?;

        self.inner.migrate_namespace(archive_ns).map_err(KeyValueError::KVError)
    }

    /// Make this (upgrade) store the current store.
    ///
    /// Fails if there is a non-empty current store.
    pub fn migrate_to_current(&mut self, storage_uri: &Url, namespace: &Namespace) -> Result<(), KeyValueError> {
        let current_store = KeyValueStore::create(storage_uri, namespace)?;
        if !current_store.is_empty()? {
            Err(KeyValueError::Other(format!(
                "Abort migrate upgraded store for {} to current. The current store was not archived.",
                namespace
            )))
        } else {
            self.inner
                .migrate_namespace(namespace.into())
                .map_err(KeyValueError::KVError)
        }
    }

    /// Returns true if this KeyValueStore (with this namespace) has any entries
    pub fn is_empty(&self) -> Result<bool, KeyValueError> {
        self.inner.is_empty().map_err(KeyValueError::KVError)
    }

    /// Import all data from the given KV store into this
    pub fn import(&self, other: &Self) -> Result<(), KeyValueError> {
        let mut scopes = other.scopes()?;
        scopes.push(Scope::global()); // not explicitly listed but should be migrated as well.

        for scope in scopes {
            for key in other.keys(&scope, "")? {
                if let Some(value) = other.get_raw_value(&key)? {
                    self.store_raw_value(&key, value)?;
                }
            }
        }

        Ok(())
    }

    /// Stores a key value pair, serialized as json, overwrite existing
    pub fn store<V: Serialize>(&self, key: &Key, value: &V) -> Result<(), KeyValueError> {
        Ok(self.inner.store(key, serde_json::to_value(value)?)?)
    }

    /// Stores a key value pair, serialized as json, fails if existing
    pub fn store_new<V: Serialize>(&self, key: &Key, value: &V) -> Result<(), KeyValueError> {
        Ok(self.inner.transaction(
            key.scope(),
            &mut move |kv: &dyn KeyValueStoreBackend| match kv.get(key)? {
                None => kv.store(key, serde_json::to_value(value)?),
                _ => Err(kvx::Error::Unknown),
            },
        )?)
    }

    pub fn inner(&self) -> &kvx::KeyValueStore {
        &self.inner
    }

    /// Gets a value for a key, returns an error if the value cannot be deserialized,
    /// returns None if it cannot be found.
    pub fn get<V: DeserializeOwned>(&self, key: &Key) -> Result<Option<V>, KeyValueError> {
        if let Some(value) = self.get_raw_value(key)? {
            match serde_json::from_value(value) {
                Ok(result) => Ok(result),
                Err(e) => {
                    // Get the value again so that we can do a full error report
                    let value = self.get_raw_value(key)?;
                    let value_str = value.map(|v| v.to_string()).unwrap_or("".to_string());

                    let expected_type = std::any::type_name::<V>();

                    Err(KeyValueError::Other(format!(
                        "Could not deserialize value for key '{}'. Expected type: {}. Error: {}. Value was: {}",
                        key, expected_type, e, value_str
                    )))
                }
            }
        } else {
            Ok(None)
        }
    }

    fn get_raw_value(&self, key: &Key) -> Result<Option<serde_json::Value>, KeyValueError> {
        self.inner.get(key).map_err(KeyValueError::KVError)
    }

    fn store_raw_value(&self, key: &Key, value: serde_json::Value) -> Result<(), KeyValueError> {
        self.inner.store(key, value).map_err(KeyValueError::KVError)
    }

    /// Transactional `get`.
    pub fn get_transactional<V: DeserializeOwned>(&self, key: &Key) -> Result<Option<V>, KeyValueError> {
        let mut result: Option<V> = None;
        let result_ref = &mut result;
        self.inner
            .transaction(key.scope(), &mut move |kv: &dyn KeyValueStoreBackend| {
                if let Some(value) = kv.get(key)? {
                    *result_ref = Some(serde_json::from_value(value)?)
                }

                Ok(())
            })?;

        Ok(result)
    }

    /// Returns whether a key exists
    pub fn has(&self, key: &Key) -> Result<bool, KeyValueError> {
        Ok(self.inner.has(key)?)
    }

    /// Delete a key-value pair
    pub fn drop_key(&self, key: &Key) -> Result<(), KeyValueError> {
        Ok(self.inner.delete(key)?)
    }

    /// Delete a scope
    pub fn drop_scope(&self, scope: &Scope) -> Result<(), KeyValueError> {
        Ok(self.inner.delete_scope(scope)?)
    }

    /// Wipe the complete store. Needless to say perhaps.. use with care..
    pub fn wipe(&self) -> Result<(), KeyValueError> {
        Ok(self.inner.clear()?)
    }

    /// Move a value from one key to another
    pub fn move_key(&self, from: &Key, to: &Key) -> Result<(), KeyValueError> {
        Ok(self.inner.move_value(from, to)?)
    }

    /// Archive a key
    pub fn archive_key(&self, key: &Key) -> Result<(), KeyValueError> {
        self.move_key(key, &key.clone().with_sub_scope(segment!("archived")))
    }

    /// Archive a key as corrupt
    pub fn archive_corrupt(&self, key: &Key) -> Result<(), KeyValueError> {
        self.move_key(key, &key.clone().with_sub_scope(segment!("corrupt")))
    }

    /// Archive a key as surplus
    pub fn archive_surplus(&self, key: &Key) -> Result<(), KeyValueError> {
        self.move_key(key, &key.clone().with_sub_scope(segment!("surplus")))
    }

    /// Returns all scopes, including sub_scopes
    pub fn scopes(&self) -> Result<Vec<Scope>, KeyValueError> {
        Ok(self.inner.list_scopes()?)
    }

    /// Returns whether a scope exists
    pub fn has_scope(&self, scope: &Scope) -> Result<bool, KeyValueError> {
        Ok(self.inner.has_scope(scope)?)
    }

    /// Returns all keys under a scope (scopes are exact strings, 'sub'-scopes
    /// would need to be specified explicitly.. e.g. 'ca' and 'ca/archived' are
    /// two distinct scopes.
    ///
    /// If matching is not empty then the key must contain the given `&str`.
    pub fn keys(&self, scope: &Scope, matching: &str) -> Result<Vec<Key>, KeyValueError> {
        Ok(self
            .inner
            .list_keys(scope)?
            .into_iter()
            .filter(|key| matching.is_empty() || key.name().as_str().contains(matching))
            .collect())
    }
}

impl fmt::Display for KeyValueStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

//------------ KeyValueError -------------------------------------------------

/// This type defines possible Errors for KeyStore
#[derive(Debug)]
pub enum KeyValueError {
    UnknownScheme(String),
    IoError(KrillIoError),
    JsonError(serde_json::Error),
    UnknownKey(Key),
    DuplicateKey(Key),
    KVError(kvx::Error),
    Other(String),
}

impl From<KrillIoError> for KeyValueError {
    fn from(e: KrillIoError) -> Self {
        KeyValueError::IoError(e)
    }
}

impl From<serde_json::Error> for KeyValueError {
    fn from(e: serde_json::Error) -> Self {
        KeyValueError::JsonError(e)
    }
}

impl From<kvx::Error> for KeyValueError {
    fn from(e: kvx::Error) -> Self {
        KeyValueError::KVError(e)
    }
}

impl fmt::Display for KeyValueError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            KeyValueError::UnknownScheme(e) => write!(f, "Unknown Scheme: {}", e),
            KeyValueError::IoError(e) => write!(f, "I/O error: {}", e),
            KeyValueError::JsonError(e) => write!(f, "JSON error: {}", e),
            KeyValueError::UnknownKey(key) => write!(f, "Unknown key: {}", key),
            KeyValueError::DuplicateKey(key) => write!(f, "Duplicate key: {}", key),
            KeyValueError::KVError(e) => write!(f, "Store error: {}", e),
            KeyValueError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

//------------ Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    use rand::{distributions::Alphanumeric, Rng};

    fn random_segment() -> SegmentBuf {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect::<String>()
            .parse()
            .unwrap()
    }

    fn random_namespace() -> NamespaceBuf {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect::<String>()
            .parse()
            .unwrap()
    }

    fn get_storage_uri() -> Url {
        env::var("KRILL_KV_STORAGE_URL")
            .ok()
            .and_then(|s| Url::parse(&s).ok())
            .unwrap_or_else(|| Url::parse("memory:///tmp").unwrap())
    }

    #[test]
    fn test_store() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let key = Key::new_global(random_segment());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());
        assert_eq!(store.get(&key).unwrap(), Some(content));
    }

    #[test]
    fn test_store_new() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let key = Key::new_global(random_segment());

        assert!(store.store_new(&key, &content).is_ok());
        assert!(store.store_new(&key, &content).is_err());
    }

    #[test]
    fn test_store_scoped() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let id = random_segment();
        let scope = Scope::from_segment(segment!("scope"));
        let key = Key::new_scoped(scope.clone(), id.clone());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());
        assert_eq!(store.get(&key).unwrap(), Some(content.clone()));
        assert!(store.has_scope(&scope).unwrap());

        let simple = Key::new_global(id);
        store.store(&simple, &content).unwrap();
        assert!(store.has(&simple).unwrap());
        assert_eq!(store.get(&simple).unwrap(), Some(content));
    }

    #[test]
    fn test_get() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let key = Key::new_global(random_segment());
        assert_eq!(store.get::<String>(&key).unwrap(), None);

        store.store(&key, &content).unwrap();
        assert_eq!(store.get(&key).unwrap(), Some(content));
    }

    #[test]
    fn test_get_transactional() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let key = Key::new_global(random_segment());
        assert_eq!(store.get_transactional::<String>(&key).unwrap(), None);

        store.store(&key, &content).unwrap();
        assert_eq!(store.get_transactional(&key).unwrap(), Some(content));
    }

    #[test]
    fn test_has() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let key = Key::new_global(random_segment());
        assert!(!store.has(&key).unwrap());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());
    }

    #[test]
    fn test_drop_key() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let key = Key::new_global(random_segment());
        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());

        store.drop_key(&key).unwrap();
        assert!(!store.has(&key).unwrap());
    }

    #[test]
    fn test_drop_scope() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let scope = Scope::from_segment(random_segment());
        let key = Key::new_scoped(scope.clone(), random_segment());
        let key2 = Key::new_scoped(Scope::from_segment(random_segment()), random_segment());
        store.store(&key, &content).unwrap();
        store.store(&key2, &content).unwrap();
        assert!(store.has_scope(&scope).unwrap());
        assert!(store.has(&key).unwrap());
        assert!(store.has(&key2).unwrap());

        store.drop_scope(&scope).unwrap();
        assert!(!store.has_scope(&scope).unwrap());
        assert!(!store.has(&key).unwrap());
        assert!(store.has(&key2).unwrap());
    }

    #[test]
    fn test_wipe() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let scope = Scope::from_segment(segment!("scope"));
        let key = Key::new_scoped(scope.clone(), random_segment());
        store.store(&key, &content).unwrap();
        assert!(store.has_scope(&scope).unwrap());
        assert!(store.has(&key).unwrap());

        store.wipe().unwrap();
        assert!(!store.has_scope(&scope).unwrap());
        assert!(!store.has(&key).unwrap());
        assert!(store.keys(&Scope::global(), "").unwrap().is_empty());
    }

    #[test]
    fn test_move_key() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_string();
        let key = Key::new_global(random_segment());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());

        let target = Key::new_global(random_segment());
        store.move_key(&key, &target).unwrap();
        assert!(!store.has(&key).unwrap());
        assert!(store.has(&target).unwrap());
    }

    #[test]
    fn test_archive() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_string();
        let key = Key::new_global(random_segment());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());

        store.archive_key(&key).unwrap();
        assert!(!store.has(&key).unwrap());
        assert!(store.has(&key.with_sub_scope(segment!("archived"))).unwrap());
    }

    #[test]
    fn test_archive_corrupt() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_string();
        let key = Key::new_global(random_segment());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());

        store.archive_corrupt(&key).unwrap();
        assert!(!store.has(&key).unwrap());
        assert!(store.has(&key.with_sub_scope(segment!("corrupt"))).unwrap());
    }

    #[test]
    fn test_archive_surplus() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_string();
        let key = Key::new_global(random_segment());

        store.store(&key, &content).unwrap();
        assert!(store.has(&key).unwrap());

        store.archive_surplus(&key).unwrap();
        assert!(!store.has(&key).unwrap());
        assert!(store.has(&key.with_sub_scope(segment!("surplus"))).unwrap());
    }

    #[test]
    fn test_scopes() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let id = segment!("id");
        let scope = Scope::from_segment(random_segment());
        let key = Key::new_scoped(scope.clone(), id);

        assert!(store.scopes().unwrap().is_empty());

        store.store(&key, &content).unwrap();
        assert_eq!(store.scopes().unwrap(), [scope.clone()]);

        let scope2 = Scope::from_segment(random_segment());
        let key2 = Key::new_scoped(scope2.clone(), id);
        store.store(&key2, &content).unwrap();

        let mut scopes = store.scopes().unwrap();
        scopes.sort();
        let mut expected = vec![scope.clone(), scope2.clone()];
        expected.sort();
        assert_eq!(scopes, expected);

        store.drop_scope(&scope2).unwrap();
        assert_eq!(store.scopes().unwrap(), vec![scope]);
    }

    #[test]
    fn test_has_scope() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let scope = Scope::from_segment(random_segment());
        let key = Key::new_scoped(scope.clone(), segment!("id"));
        assert!(!store.has_scope(&scope).unwrap());

        store.store(&key, &content).unwrap();
        assert!(store.has_scope(&scope).unwrap());
    }

    #[test]
    fn test_keys() {
        let storage_uri = get_storage_uri();

        let store = KeyValueStore::create(&storage_uri, &random_namespace()).unwrap();
        let content = "content".to_owned();
        let id = segment!("command--id");
        let scope = Scope::from_segment(segment!("command"));
        let key = Key::new_scoped(scope.clone(), id);

        let id2 = segment!("command--ls");
        let id3 = random_segment();
        let key2 = Key::new_scoped(scope.clone(), id2);
        let key3 = Key::new_global(id3.clone());

        store.store(&key, &content).unwrap();
        store.store(&key2, &content).unwrap();
        store.store(&key3, &content).unwrap();

        let mut keys = store.keys(&scope, "command--").unwrap();
        keys.sort();
        let mut expected = vec![key.clone(), key2.clone()];
        expected.sort();

        assert_eq!(keys, expected);
        assert_eq!(store.keys(&scope, id2.as_str()).unwrap(), [key2.clone()]);
        assert_eq!(store.keys(&scope, id3.as_str()).unwrap(), []);
        assert_eq!(store.keys(&Scope::global(), id3.as_str()).unwrap(), [key3]);

        let mut keys = store.keys(&scope, "").unwrap();
        keys.sort();
        let mut expected = vec![key, key2];
        expected.sort();

        assert_eq!(keys, expected);
    }
}
