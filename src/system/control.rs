// Application control locks

use std::{sync::Arc, time::Duration};

use artisan_middleware::dusa_collection_utils::log::LogLevel;
use artisan_middleware::state_persistence::AppState;
use artisan_middleware::{
    control::ToggleControl,
    dusa_collection_utils::{errors::ErrorArrayItem, rwarc::LockWithTimeout},
};
use artisan_middleware::{dusa_collection_utils::log, identity::Identifier};
use once_cell::sync::Lazy;
use tokio::sync::Notify;
use crate::applications::child::{APP_STATUS_ARRAY, CLIENT_APPLICATION_HANDLER, SYSTEM_APPLICATION_HANDLER};

use super::portal::PortalAddr;

#[allow(dead_code)]
pub static APPLICATION_CONTROLS: Lazy<Arc<Controls>> = Lazy::new(|| Arc::new(Controls::new()));
pub static PORTAL_CONTROLS: Lazy<LockWithTimeout<PortalState>> =
    Lazy::new(|| LockWithTimeout::new(PortalState::new()));
// Diffrent locks for network communications and array locks

/// Struct to manage control operations within the application
pub struct Controls {
    status_lock: Arc<ToggleControl>,
    communication_lock: Arc<ToggleControl>,
    reload_notify: Arc<Notify>,
    shutdown_notify: Arc<Notify>,
}

/// Struct to manage the portal's state and ensure proper linkage and timing
#[derive(Clone, Debug)]
pub struct PortalState {
    portal_found: bool,
    portal_addrs: Vec<PortalAddr>,
    portal_identy: Option<Identifier>,
    portal_linked: bool,
    portal_intime: bool,
}

impl PortalState {
    pub fn new() -> Self {
        PortalState {
            portal_found: false,
            portal_addrs: vec![],
            portal_identy: None,
            portal_linked: false,
            portal_intime: false, // need a methode of tracking portal comms
        }
    }

    pub async fn set_identity(
        id: Option<Identifier>,
        portal_controls: LockWithTimeout<Self>,
    ) -> Result<(), ErrorArrayItem> {
        let mut portal_state_write_lock = portal_controls.try_write().await?;
        portal_state_write_lock.portal_identy = id;
        Ok(())
    }

    pub async fn get_identity(
        portal_controls: LockWithTimeout<Self>,
    ) -> Result<Option<Identifier>, ErrorArrayItem> {
        let portal_state_read_lock = portal_controls.try_read().await?;
        Ok(portal_state_read_lock.portal_identy.clone())
    }

    pub async fn portal_addrs(
        portal_controls: LockWithTimeout<Self>,
    ) -> Result<Vec<PortalAddr>, ErrorArrayItem> {
        let portal_state_read_lock = portal_controls.try_read().await?;
        Ok(portal_state_read_lock.portal_addrs.clone())
    }

    pub async fn set_portal_addrs(
        portal_controls: LockWithTimeout<Self>,
        portals: Vec<PortalAddr>,
    ) -> Result<(), ErrorArrayItem> {
        let mut portal_state_write_lock = portal_controls.try_write().await?;
        portal_state_write_lock.portal_addrs = portals;
        drop(portal_state_write_lock);
        Ok(())
    }

    pub async fn portal_found(
        portal_controls: LockWithTimeout<Self>,
    ) -> Result<(), ErrorArrayItem> {
        let mut portal_state_write_lock = portal_controls.try_write().await?;
        portal_state_write_lock.portal_found = true;
        Ok(())
    }

    pub async fn portal_linked(
        portal_controls: LockWithTimeout<Self>,
    ) -> Result<(), ErrorArrayItem> {
        let mut portal_state_write_lock = portal_controls.try_write().await?;
        portal_state_write_lock.portal_found = true;
        Ok(())
    }

    pub async fn is_portal_linked(
        portal_controls: LockWithTimeout<Self>,
    ) -> Result<bool, ErrorArrayItem> {
        let portal_state_read_lock = portal_controls.try_read().await?;
        Ok(portal_state_read_lock.portal_linked.clone())
    }
}

impl Controls {
    pub fn new() -> Self {
        Self {
            status_lock: Arc::new(ToggleControl::new()),
            communication_lock: Arc::new(ToggleControl::new()),
            reload_notify: Arc::new(Notify::new()),
            shutdown_notify: Arc::new(Notify::new()),
        }
    }

    pub async fn wait_for_network_control(&self) {
        self.communication_lock.wait_if_paused().await;
    }

    pub async fn wait_for_network_control_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), &str> {
        self.communication_lock.wait_with_timeout(timeout).await
    }

    pub fn signal_reload(&self) {
        self.reload_notify.notify_one();
    }

    pub fn signal_shutdown(&self) {
        self.shutdown_notify.notify_one();
    }

    /// Pauses all controls and ensures they are in a paused state.
    pub async fn pause_all_controls(&self) -> bool {
        self.communication_lock.wait_if_paused().await;
        self.communication_lock.pause();
        self.status_lock.wait_if_paused().await;
        self.status_lock.pause();

        self.communication_lock.is_paused().await && self.status_lock.is_paused().await
    }

    /// Pauses all controls and ensures they are in a paused state.
    pub async fn resume_all_controls(&self) -> bool {
        self.communication_lock.resume();
        self.status_lock.resume();

        !self.communication_lock.is_paused().await && !self.status_lock.is_paused().await
    }

    pub fn start_contol_monitor(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = self.reload_notify.notified() => {
                        self.pause_all_controls().await;

                        // Clearning the handlers
                        let client_handler = &CLIENT_APPLICATION_HANDLER.clone();
                        let system_handler = &SYSTEM_APPLICATION_HANDLER.clone();
                        let applications = &APP_STATUS_ARRAY.clone();

                        match client_handler.try_write().await {
                            Ok(mut clients) => {
                                clients.clear();
                                clients.shrink_to_fit();
                            },
                            Err(err) => {
                                log!(LogLevel::Error, "Failed to lock client handler, dumping: {}", err);
                            },
                        };

                        match system_handler.try_write().await {
                            Ok(mut systems) => {
                                systems.clear();
                                systems.shrink_to_fit();
                            },
                            Err(err) => {
                                log!(LogLevel::Error, "Failed to lock system handler, dumping: {}", err);
                            },
                        };
                        
                        match applications.try_write().await {
                            Ok(mut apps) => {
                                apps.clear();
                                apps.shrink_to_fit();
                            },
                            Err(err) => {
                                log!(LogLevel::Error, "Failed to lock application array, dumping: {}", err);
                            },
                        };
                        
                        // Refilling the locks and vecs


                        self.resume_all_controls().await;
                    }

                    _ = self.shutdown_notify.notified() => {
                        self.pause_all_controls().await;

                        std::process::exit(0)
                    }
                }
            }
        });
    }

    pub fn start_signal_monitors(&self) {
        _signal_monitor(self.reload_notify.clone(), self.shutdown_notify.clone());
    }
}

// Function to start signal monitors
fn _signal_monitor(reload_notify: Arc<Notify>, shutdown_notify: Arc<Notify>) {
    // Monitor SIGHUP for reload
    tokio::spawn(async move {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("Failed to register SIGHUP");
        while sighup.recv().await.is_some() {
            log!(LogLevel::Info, "Received SIGHUP, signaling reload...");
            reload_notify.notify_one();
        }
    });

    // Monitor SIGUSR1 for shutdown
    tokio::spawn(async move {
        let mut sigusr1 =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("Failed to register SIGUSR1");
        while sigusr1.recv().await.is_some() {
            log!(LogLevel::Info, "Received SIGUSR1, signaling shutdown...");
            shutdown_notify.notify_one();
        }
    });
}
