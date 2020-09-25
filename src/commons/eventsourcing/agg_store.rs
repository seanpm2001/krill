use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rpki::x509::Time;

use crate::commons::api::{CommandHistory, CommandHistoryCriteria, Handle};
use crate::commons::eventsourcing::cmd::{Command, StoredCommandBuilder};
use crate::commons::eventsourcing::{
    Aggregate, CommandKey, DiskKeyStore, Event, EventListener, KeyStore, KeyStoreError, KeyStoreVersion, StoredCommand,
    StoredValueInfo,
};

const SNAPSHOT_FREQ: u64 = 5;

pub type StoreResult<T> = Result<T, AggregateStoreError>;

pub trait AggregateStore<A: Aggregate>: Send + Sync
where
    A::Error: From<AggregateStoreError>,
{
    /// Gets the latest version for the given aggregate. Returns
    /// an AggregateStoreError::UnknownAggregate in case the aggregate
    /// does not exist.
    fn get_latest(&self, id: &Handle) -> StoreResult<Arc<A>>;

    /// Adds a new aggregate instance based on the init event.
    fn add(&self, init: A::InitEvent) -> StoreResult<Arc<A>>;

    /// Sends a command to the appropriate aggregate, and on
    /// success: save command and events, return aggregate
    /// no-op: do not save anything, return aggregate
    /// error: save command and error, return error
    fn command(&self, cmd: A::Command) -> Result<Arc<A>, A::Error>;

    /// Returns true if an instance exists for the id
    fn has(&self, id: &Handle) -> bool;

    /// Lists all known ids.
    fn list(&self) -> Vec<Handle>;

    /// Adds a listener that will receive a reference to all events as they
    /// are stored.
    fn add_listener<L: EventListener<A>>(&mut self, listener: Arc<L>);

    /// Lists the history for an aggregate.
    fn command_history(&self, id: &Handle, crit: CommandHistoryCriteria) -> StoreResult<CommandHistory>;

    /// Returns a stored command if it can be found.
    fn stored_command(
        &self,
        id: &Handle,
        key: &CommandKey,
    ) -> StoreResult<Option<StoredCommand<A::StorableCommandDetails>>>;

    /// Returns a stored event if it can be found.
    fn stored_event(&self, id: &Handle, version: u64) -> StoreResult<Option<A::Event>>;
}

/// This type defines possible Errors for the AggregateStore
#[derive(Debug, Display)]
pub enum AggregateStoreError {
    #[display(fmt = "{}", _0)]
    KeyStoreError(KeyStoreError),

    #[display(fmt = "{}", _0)]
    IoError(io::Error),

    #[display(fmt = "unknown entity: {}", _0)]
    UnknownAggregate(Handle),

    #[display(fmt = "init event exists, but cannot be applied")]
    InitError,

    #[display(fmt = "event not applicable to entity, id or version is off")]
    WrongEventForAggregate,

    #[display(fmt = "concurrent modification attempt for entity: '{}'", _0)]
    ConcurrentModification(Handle),

    #[display(fmt = "Aggregate '{}' does not have command with sequence '{}'", _0, _1)]
    UnknownCommand(Handle, u64),

    #[display(fmt = "Offset '{}' exceeds total '{}'", _0, _1)]
    CommandOffsetTooLarge(u64, u64),

    #[display(
        fmt = "Could not rebuild state for '{}', looks like a corrupt state on disk, you may want try --recover. Error was: {}",
        _0,
        _1
    )]
    WarmupFailed(Handle, String),

    #[display(
        fmt = "Could not recover state for '{}', aborting recover. Use consistent backup.",
        _0
    )]
    CouldNotRecover(Handle),
}

impl From<KeyStoreError> for AggregateStoreError {
    fn from(e: KeyStoreError) -> Self {
        AggregateStoreError::KeyStoreError(e)
    }
}

pub struct DiskAggregateStore<A: Aggregate> {
    store: DiskKeyStore,
    cache: RwLock<HashMap<Handle, Arc<A>>>,
    use_cache: bool,
    listeners: Vec<Arc<dyn EventListener<A>>>,
    outer_lock: RwLock<()>,
}

impl<A: Aggregate> DiskAggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    pub fn new(work_dir: &PathBuf, name_space: &str) -> StoreResult<Self> {
        let store = DiskKeyStore::under_work_dir(work_dir, name_space).map_err(AggregateStoreError::IoError)?;

        if store.aggregates().is_empty() {
            store
                .set_version(&KeyStoreVersion::V0_7)
                .map_err(AggregateStoreError::KeyStoreError)?;
        }

        let cache = RwLock::new(HashMap::new());
        let use_cache = true;
        let listeners = vec![];
        let lock = RwLock::new(());
        Ok(DiskAggregateStore {
            store,
            cache,
            use_cache,
            listeners,
            outer_lock: lock,
        })
    }

    /// Warms up the cache, to be used after startup. Will fail if any aggregates fail to load.
    /// In that case the user may want to use the recover option to see what can be salvaged.
    pub fn warm(&self) -> StoreResult<()> {
        for handle in self.list() {
            self.get_latest(&handle)
                .map_err(|e| AggregateStoreError::WarmupFailed(handle, e.to_string()))?;
        }
        Ok(())
    }

    /// Recovers the aggregates by verifying all commands, and the corresponding events.
    /// Use this in case the state on disk is found to be inconsistent. I.e. the `warm`
    /// function failed and Krill exited.
    pub fn recover(&self) -> StoreResult<()> {
        let criteria = CommandHistoryCriteria::default();
        for handle in self.list() {
            info!("Will recover state for '{}'", &handle);

            // Check
            // - All commands, archive bad commands
            // - All events, archive bad events
            // - Keep track of last known good command and event
            // - Archive all commands and events after
            //
            // Rebuild state up to event:
            //   - use snapshot - archive if bad
            //   - use back-up snapshot if snapshot is no good - archive if bad
            //   - start from init event if back-up snapshot is bad, or if the version exceeds last good event
            //   - process events from (back-up) snapshot up to last good event
            //
            //  If still good:
            //   - save snapshot
            //   - save info

            let mut last_good_cmd = 0;
            let mut last_good_evt = 0;
            let mut last_update = Time::now();

            // Check all commands and associated events
            let mut hunkydory = true;
            for key in self.store.command_keys_ascending(&handle, &criteria) {
                let key = key.into();
                if hunkydory {
                    if let Ok(cmd) = self.store.get_command::<A::StorableCommandDetails>(&handle, &key) {
                        if let Some(events) = cmd.effect().events() {
                            for version in events {
                                // TODO: When archiving is in place, allow missing (archived) events as long as they are from before the snapshot or backup snapshot
                                if let Ok(Some(_)) = self.store.get_event::<A::Event>(&handle, *version) {
                                    last_good_evt = *version;
                                } else {
                                    hunkydory = false;
                                }
                            }
                        }
                        last_good_cmd = cmd.sequence();
                        last_update = cmd.time();
                    } else {
                        hunkydory = false;
                    }
                }
                if !hunkydory {
                    // Bad command or event encountered.. archive surplus commands
                    // note that we will clean surplus events later
                    self.store.archive_surplus(&handle, &key)?;
                }
            }

            self.store.archive_surplus_events(&handle, last_good_evt + 1)?;

            let agg = self
                .store
                .get_aggregate::<A>(&handle, Some(last_good_evt))?
                .ok_or_else(|| AggregateStoreError::CouldNotRecover(handle.clone()))?;

            let snapshot_version = agg.version();

            let info = StoredValueInfo {
                last_event: last_good_evt,
                last_command: last_good_cmd,
                last_update,
                snapshot_version,
            };

            self.store
                .store_snapshot(&handle, &agg)
                .map_err(AggregateStoreError::KeyStoreError)?;

            self.cache_update(&handle, Arc::new(agg));

            self.store.save_info(&handle, &info)?;
        }

        Ok(())
    }
}

impl<A: Aggregate> DiskAggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    fn has_updates(&self, id: &Handle, aggregate: &A) -> StoreResult<bool> {
        Ok(self.store.get_event::<A::Event>(id, aggregate.version())?.is_some())
    }

    fn cache_get(&self, id: &Handle) -> Option<Arc<A>> {
        if self.use_cache {
            self.cache.read().unwrap().get(id).cloned()
        } else {
            None
        }
    }

    fn cache_update(&self, id: &Handle, arc: Arc<A>) {
        if self.use_cache {
            self.cache.write().unwrap().insert(id.clone(), arc);
        }
    }

    fn get_latest_no_lock(&self, handle: &Handle) -> StoreResult<Arc<A>> {
        trace!("Trying to load aggregate id: {}", handle);
        match self.cache_get(handle) {
            None => match self.store.get_aggregate(handle, None)? {
                None => {
                    error!("Could not load aggregate with id: {} from disk", handle);
                    Err(AggregateStoreError::UnknownAggregate(handle.clone()))
                }
                Some(agg) => {
                    let arc: Arc<A> = Arc::new(agg);
                    self.cache_update(handle, arc.clone());
                    trace!("Loaded aggregate id: {} from disk", handle);
                    Ok(arc)
                }
            },
            Some(mut arc) => {
                if self.has_updates(handle, &arc)? {
                    let agg = Arc::make_mut(&mut arc);
                    self.store.update_aggregate(handle, agg, None)?;
                }
                trace!("Loaded aggregate id: {} from memory", handle);
                Ok(arc)
            }
        }
    }
}

impl<A: Aggregate> AggregateStore<A> for DiskAggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    fn get_latest(&self, handle: &Handle) -> StoreResult<Arc<A>> {
        let _lock = self.outer_lock.read().unwrap();
        self.get_latest_no_lock(handle)
    }

    fn add(&self, init: A::InitEvent) -> StoreResult<Arc<A>> {
        let _lock = self.outer_lock.write().unwrap();

        self.store.store_event(&init)?;

        let handle = init.handle().clone();

        let aggregate = A::init(init).map_err(|_| AggregateStoreError::InitError)?;
        self.store.store_snapshot(&handle, &aggregate)?;

        let info = StoredValueInfo::default();
        self.store.save_info(&handle, &info)?;

        let arc = Arc::new(aggregate);
        self.cache_update(&handle, arc.clone());

        Ok(arc)
    }

    fn command(&self, cmd: A::Command) -> Result<Arc<A>, A::Error> {
        let _lock = self.outer_lock.write().unwrap();

        // Get the latest arc.
        let handle = cmd.handle().clone();

        let mut info = self
            .store
            .get_info(&handle)
            .map_err(AggregateStoreError::KeyStoreError)?;
        info.last_update = Time::now();
        info.last_command += 1;

        let mut latest = self.get_latest_no_lock(&handle)?;

        if let Some(version) = cmd.version() {
            if version != latest.version() {
                error!(
                    "Version conflict updating '{}', expected version: {}, found: {}",
                    handle,
                    version,
                    latest.version()
                );

                return Err(A::Error::from(AggregateStoreError::ConcurrentModification(handle)));
            }
        }

        let stored_command_builder = StoredCommandBuilder::new(&cmd, latest.version(), info.last_command);

        let res = match latest.process_command(cmd) {
            Err(e) => {
                let stored_command = stored_command_builder.finish_with_error(&e);
                self.store
                    .store_command(stored_command)
                    .map_err(AggregateStoreError::KeyStoreError)?;
                Err(e)
            }
            Ok(events) => {
                if events.is_empty() {
                    return Ok(latest); // otherwise the version info will be updated
                } else {
                    let agg = Arc::make_mut(&mut latest);

                    // Using a lock on the hashmap here to ensure that all updates happen sequentially.
                    // It would be better to get a lock only for this specific aggregate. So it may be
                    // worth rethinking the structure.
                    //
                    // That said.. saving and applying events is really quick, so this should not hurt
                    // performance much.
                    //
                    // Also note that we don't need the lock to update the inner arc in the cache. We
                    // just need it to be in scope until we are done updating.
                    let mut cache = self.cache.write().unwrap();

                    // It should be impossible to get events for the wrong aggregate, and the wrong
                    // versions, because we are doing the update here inside the outer lock, and aggregates
                    // generally do not lie about who do they are.
                    //
                    // Still.. some defensive coding in case we do have some issue. Double check that the
                    // events are for this aggregate, and are a contiguous sequence of version starting with
                    // this version.
                    let version_before = agg.version();
                    let nr_events = events.len() as u64;

                    info.last_event += nr_events;

                    for i in 0..nr_events {
                        let event = &events[i as usize];
                        if event.version() != version_before + i || event.handle() != &handle {
                            return Err(A::Error::from(AggregateStoreError::WrongEventForAggregate));
                        }
                    }

                    // Time to start saving things.
                    let stored_command = stored_command_builder.finish_with_events(events.as_slice());
                    self.store
                        .store_command(stored_command)
                        .map_err(AggregateStoreError::KeyStoreError)?;

                    for event in &events {
                        self.store
                            .store_event(event)
                            .map_err(AggregateStoreError::KeyStoreError)?;

                        agg.apply(event.clone());
                        if agg.version() % SNAPSHOT_FREQ == 0 {
                            info.snapshot_version = agg.version();

                            self.store
                                .store_snapshot(&handle, agg)
                                .map_err(AggregateStoreError::KeyStoreError)?;
                        }
                    }

                    cache.insert(handle.clone(), Arc::new(agg.clone()));

                    // Only send this to listeners after everything has been saved.
                    for event in events {
                        for listener in &self.listeners {
                            listener.as_ref().listen(agg, &event);
                        }
                    }

                    Ok(latest)
                }
            }
        };

        self.store
            .save_info(&handle, &info)
            .map_err(AggregateStoreError::KeyStoreError)?;

        res
    }

    fn has(&self, id: &Handle) -> bool {
        let _lock = self.outer_lock.read().unwrap();
        self.store.has_aggregate(id)
    }

    fn list(&self) -> Vec<Handle> {
        let _lock = self.outer_lock.read().unwrap();
        self.store.aggregates()
    }

    fn add_listener<L: EventListener<A>>(&mut self, listener: Arc<L>) {
        let _lock = self.outer_lock.write().unwrap();
        self.listeners.push(listener)
    }

    fn command_history(&self, id: &Handle, crit: CommandHistoryCriteria) -> StoreResult<CommandHistory> {
        self.store
            .command_history::<A>(id, crit)
            .map_err(AggregateStoreError::KeyStoreError)
    }

    fn stored_command(
        &self,
        id: &Handle,
        key: &CommandKey,
    ) -> StoreResult<Option<StoredCommand<<A as Aggregate>::StorableCommandDetails>>> {
        self.store
            .get(id, &key.into())
            .map_err(AggregateStoreError::KeyStoreError)
    }

    fn stored_event(&self, id: &Handle, version: u64) -> StoreResult<Option<<A as Aggregate>::Event>> {
        let key = DiskKeyStore::key_for_event(version);
        self.store.get(id, &key).map_err(AggregateStoreError::KeyStoreError)
    }
}
