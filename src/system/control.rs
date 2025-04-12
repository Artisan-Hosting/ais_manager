use std::collections::HashMap;
use std::sync::RwLock;
// Application control locks
use std::{sync::Arc, time::Duration};

use artisan_middleware::config::AppConfig;
use artisan_middleware::dusa_collection_utils::errors::Errors;
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::types::pathtype::PathType;
use artisan_middleware::dusa_collection_utils::types::rwarc::LockWithTimeout;
use artisan_middleware::historics::UsageLedger;
use artisan_middleware::state_persistence::AppState;
use artisan_middleware::{control::ToggleControl, dusa_collection_utils::errors::ErrorArrayItem};
use artisan_middleware::{dusa_collection_utils::log, identity::Identifier};
use tokio::net::TcpStream;
use tokio::sync::{Notify, OnceCell};

use super::config::{generate_state, get_config};
use super::ebpf::BandwidthTracker;
use super::portal::PortalAddr;
use super::state::get_state_path;

pub static GLOBAL_STATE: OnceCell<Arc<GlobalState>> = OnceCell::const_new();
pub const LEDGER_PATH: &str = "/opt/artisan/ladger.json"; // make this encrypted at some point

pub struct GlobalState {
    pub signals: Arc<Signals>,
    pub locks: Arc<Locks>,
    pub portal_state: PortalState,
    pub network_monitor: Arc<BandwidthTracker>,
    pub ledger: LockWithTimeout<UsageLedger>,
    pub app_state: Arc<RwLock<AppState>>,
    pub app_state_path: PathType,
}

#[allow(dead_code)]
impl GlobalState {
    pub async fn initialize_global_state() -> Result<(), ErrorArrayItem> {
        let signals: Arc<Signals> = Arc::new(Signals::new());
        let locks: Arc<Locks> = Arc::new(Locks::new());
        let portal_state: PortalState = PortalState::new()?;
        let network_monitor: Arc<BandwidthTracker> = Arc::new(BandwidthTracker::new().await?);
        let ledger: UsageLedger = UsageLedger::load_from_disk(LEDGER_PATH).unwrap_or_else(|_| UsageLedger::new());

        let app_state_data: (Arc<RwLock<AppState>>, PathType) = {
            let config: AppConfig = get_config();
            let state: AppState = match generate_state(&config).await {
                Ok(state) => state,
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "Failed to save state off rip. Let's try that again: {}",
                        err.err_mesg
                    );
                    signals.signal_shutdown();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    unreachable!("Failed to shutdown application. State file failed to load");
                },
            };
            let state_path: PathType = get_state_path(&config);

            let wrapped_app_state = Arc::new(RwLock::new(state));
            (wrapped_app_state, state_path)
        };

        let state: GlobalState = GlobalState {
            portal_state,
            network_monitor,
            signals,
            locks,
            app_state: app_state_data.0,
            app_state_path: app_state_data.1,
            ledger: LockWithTimeout::new(ledger),
        };

        if let Err(err) = GLOBAL_STATE.set(Arc::new(state)) {
            if err.is_already_init_err() {
                log!(
                    LogLevel::Warn,
                    "Someone tried to re_initialize the global state"
                );
            }

            if err.is_initializing_err() {
                let err_item = ErrorArrayItem::new(
                    Errors::AppState,
                    format!("Error Initializing global state: {}", err.to_string()),
                );
                return Err(err_item);
            }
        }

        Ok(())
    }

    pub async fn initialized(&self) -> bool {
        if let None = GLOBAL_STATE.get() {
            false
        } else {
            true
        }
    }

    pub async fn get_identity(&self) -> Identifier {
        self.portal_state.get_identity().await
    }

    pub async fn get_state_clone(&self) -> Result<AppState, ErrorArrayItem> {
        Ok(self
            .app_state
            .try_read()
            .map_err(|_| {
                ErrorArrayItem::new(
                    Errors::AppState,
                    "Failed to get the app state from the global state",
                )
            })?
            .clone())
    }
}

pub struct Signals {
    pub reload_notify: Arc<Notify>,
    pub shutdown_notify: Arc<Notify>,
}

impl Signals {
    pub fn new() -> Self {
        Self {
            reload_notify: Arc::new(Notify::new()),
            shutdown_notify: Arc::new(Notify::new()),
        }
    }

    pub fn signal_shutdown(&self) {
        self.shutdown_notify.notify_one();
    }

    pub fn signal_reload(&self) {
        self.reload_notify.notify_one();
    }
}

pub struct Locks {
    // status_lock: Arc<ToggleControl>, No need for it yet
    communication_lock: Arc<ToggleControl>,
}

impl Locks {
    pub fn new() -> Self {
        Self {
            communication_lock: Arc::new(ToggleControl::new()),
        }
    }

    #[allow(dead_code)]
    pub async fn wait_for_network_control(&self) {
        self.communication_lock.wait_if_paused().await;
    }

    #[allow(dead_code)]
    pub async fn resume_network(&self) {
        self.communication_lock.resume();
    }

    #[allow(dead_code)]
    pub async fn pause_network(&self) {
        self.communication_lock.pause();
    }

    pub async fn wait_for_network_control_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), &str> {
        self.communication_lock.wait_with_timeout(timeout).await
    }
}

#[derive(Clone, Debug)]
pub struct PortalIntance {
    address: PortalAddr,
    intime: bool, // we take note of the addr and time's it requests data
}

#[allow(dead_code)]
impl PortalIntance {
    pub fn new(address: PortalAddr) -> PortalIntance {
        PortalIntance {
            address,
            intime: false,
        }
    }

    /// Attepmts to conntect to a given portal instance returning a [`tokio::net::TcpStream`] on success
    pub async fn connect(&self) -> Result<TcpStream, ErrorArrayItem> {
        TcpStream::connect(format!("{}:{}", self.address.addr, self.address.port))
            .await
            .map_err(ErrorArrayItem::from)
    }

    /// Sets a given instance as 'intime' meaning it's  requesting and getting data
    pub fn in_time(&mut self) {
        self.intime = true
    }

    /// Sets a given instance as 'outtime' meaning we've found it but it may be unhealthy
    pub fn out_time(&mut self) {
        self.intime = false
    }

    pub fn get_address(&self) -> PortalAddr {
        self.address.clone()
    }
}

/// Struct to manage the portal's state and ensure proper linkage and timing
#[derive(Clone, Debug)]
pub struct PortalState {
    identity: Identifier,
    lock: LockWithTimeout<HashMap<PortalAddr, PortalIntance>>,
}

#[allow(dead_code)]
impl PortalState {
    pub fn new() -> Result<Self, ErrorArrayItem> {
        let array: HashMap<PortalAddr, PortalIntance> = HashMap::new();
        let identity: Identifier = Identifier::load_from_file()?;
        let lock: LockWithTimeout<HashMap<PortalAddr, PortalIntance>> = LockWithTimeout::new(array);
        Ok(PortalState { identity, lock })
    }

    pub async fn insert(&self, address: PortalAddr) -> Result<(), ErrorArrayItem> {
        let mut write_guard: tokio::sync::RwLockWriteGuard<'_, HashMap<PortalAddr, PortalIntance>> =
            self.lock.try_write().await?;
        let instance: PortalIntance = PortalIntance::new(address.clone());
        write_guard.insert(address, instance);
        Ok(())
    }

    pub async fn remove(
        &self,
        address: PortalAddr,
    ) -> Result<Option<PortalIntance>, ErrorArrayItem> {
        let mut write_guard: tokio::sync::RwLockWriteGuard<'_, HashMap<PortalAddr, PortalIntance>> =
            self.lock.try_write().await?;
        Ok(write_guard.remove(&address))
    }

    pub async fn contains(&self, address: PortalAddr) -> Result<bool, ErrorArrayItem> {
        let read_guard: tokio::sync::RwLockReadGuard<'_, HashMap<PortalAddr, PortalIntance>> =
            self.lock.try_read().await?;
        Ok(read_guard.contains_key(&address))
    }

    pub async fn get_time(&self, address: PortalAddr) -> Result<bool, ErrorArrayItem> {
        let read_guard: tokio::sync::RwLockReadGuard<'_, HashMap<PortalAddr, PortalIntance>> =
            self.lock.try_read().await?;

        if let Some(portal) = read_guard.get(&address) {
            Ok(portal.intime)
        } else {
            Ok(false)
        }
    }

    pub async fn set_time(&self, address: PortalAddr, intime: bool) -> Result<(), ErrorArrayItem> {
        let mut write_guard: tokio::sync::RwLockWriteGuard<'_, HashMap<PortalAddr, PortalIntance>> =
            self.lock.try_write().await?;

        match write_guard.get_mut(&address) {
            Some(instance) => {
                instance.intime = intime;
                Ok(())
            }
            None => Err(ErrorArrayItem::new(
                Errors::NotFound,
                "refrenced portal instance not found ",
            )),
        }
    }

    pub async fn get_identity(&self) -> Identifier {
        self.identity.clone()
    }

    pub async fn get_portals(&self) -> Result<Vec<PortalIntance>, ErrorArrayItem> {
        let mut portal_array: Vec<PortalIntance> = Vec::new();
        let read_guard: tokio::sync::RwLockReadGuard<'_, HashMap<PortalAddr, PortalIntance>> =
            self.lock.try_read().await?;

        read_guard.clone().into_values().for_each(|instance| {
            portal_array.push(instance);
        });

        Ok(portal_array)
    }
}
