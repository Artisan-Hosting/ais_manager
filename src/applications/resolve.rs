use artisan_middleware::config::AppConfig;
use artisan_middleware::config_bundle::ApplicationConfig;
use artisan_middleware::dusa_collection_utils::errors::{ErrorArrayItem, Errors};
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::types::pathtype::PathType;
use artisan_middleware::dusa_collection_utils::types::stringy::Stringy;
use artisan_middleware::enviornment::definitions::Enviornment;
use artisan_middleware::git_actions::GitCredentials;
use artisan_middleware::state_persistence::{AppState, StatePersistence};
use serde::{Deserialize, Serialize};
use std::{fmt, fs};
use tokio::task;

use super::child::{CLIENT_APPLICATION_ARRAY, SYSTEM_APPLICATION_ARRAY};

// pub static SYSTEMAPPLICATIONS: [&'static str; 4] = ["gitmon", "ids", "self", "messenger"];
pub static SYSTEMAPPLICATIONS: [&'static str; 3] = ["gitmon", "self", "mailler"];

#[derive(Debug, Serialize, Deserialize)]
pub enum Applications {
    System(Vec<SystemApplication>),
    Client(Vec<ClientApplication>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Application {
    System(SystemApplication),
    Client(ClientApplication),
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemApplication {
    pub name: Stringy,
    pub path: PathType,
    pub exists: bool,
    pub config: ApplicationConfig,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientApplication {
    pub name: Stringy,
    pub path: PathType,
    pub exists: bool,
    pub config: ApplicationConfig,
}

impl fmt::Display for ClientApplication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ClientApplication {{\n  name: {},\n  path: {},\n  exists: {},\n  state: {},\n  environ: {}\n}}",
            self.name,
            self.path,
            self.exists,
            self.config.state,
            self.config.enviornment
                .as_ref()
                .map_or("None".to_string(), |e| e.to_string())
        )
    }
}

#[allow(unused_assignments)]
pub async fn resolve_system_applications() -> Result<(), ErrorArrayItem> {
    let system_application_names: Vec<Stringy> = SYSTEMAPPLICATIONS
        .iter()
        .map(|app_name| {
            if *app_name == "self" {
                "ais_manager".into()
            } else {
                format!("ais_{}", app_name).into()
            }
        })
        .collect();

    // assemble the Struct from the array
    let mut tasks: Vec<task::JoinHandle<Result<SystemApplication, ()>>> = Vec::new();

    for name in system_application_names {
        let name = name.clone();
        tasks.push(task::spawn(async move {
            let application_path = PathType::Content(format!("/opt/artisan/bin/{}", name));
            let application_state_path = PathType::Content(format!("/tmp/.{}.state", name));

            if !application_state_path.exists() {
                log!(
                    LogLevel::Error,
                    "Couldn't find state file for: {}\nWe won't manage it",
                    name
                );
                return Err(());
            };

            let state: AppState = match StatePersistence::load_state(&application_state_path).await
            {
                Ok(state) => state,
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "Couldn't load system app state data: {}",
                        err
                    );
                    return Err(());
                }
            };

            let mut system_application = SystemApplication {
                name: name.clone(),
                path: application_path.clone(),
                exists: application_path.exists(),
                config: ApplicationConfig::new(state, None, None),
            };

            if name == "ais_manager".into() {
                system_application.path =
                    PathType::Content(format!("/opt/artisan/bin/ais_manager"));
            }

            Ok(system_application)
        }));
    }

    let mut results: Vec<SystemApplication> = Vec::new();
    for task in tasks {
        match task.await {
            Ok(system) => {
                if let Ok(sys_app) = system {
                    results.push(sys_app);
                }
            }
            Err(err) => {
                log!(
                    LogLevel::Error,
                    "Join error assembling system applications: {}",
                    err
                );
            }
        }
    }

    // Writing to the system array
    let mut system_application_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, SystemApplication>,
    > = SYSTEM_APPLICATION_ARRAY.try_write().await?;

    for app in results {
        system_application_array_write_lock.insert(app.clone().name, app);
    }

    Ok(())
}

#[allow(unused_assignments)]
pub async fn resolve_client_applications(config: &AppConfig) -> Result<(), ErrorArrayItem> {
    let mut application_list: Vec<String> = Vec::new();

    let dir_read: fs::ReadDir = match fs::read_dir("/opt/artisan/bin") {
        Ok(data) => data,
        Err(err) => {
            log!(
                LogLevel::Error,
                "Failed to read bins from /opt/artisan/bin: {}",
                err
            );
            return Err(ErrorArrayItem::from(err));
        }
    };

    for entry in dir_read {
        match entry {
            Ok(maybe_file) => {
                if let Ok(filetype) = maybe_file.file_type() {
                    if filetype.is_file() {
                        match maybe_file.file_name().into_string() {
                            Ok(name) => {
                                application_list.push(name);
                            }
                            Err(err) => {
                                log!(
                                    LogLevel::Error,
                                    "Skipping file, has a stupid file name: {:?}",
                                    err
                                );
                            }
                        };
                    }
                }
            }
            Err(err) => {
                log!(
                    LogLevel::Error,
                    "Failed to read bins from /opt/artisan/bin: {}",
                    err
                );
                continue;
            }
        }
    }

    // Pasring the git configuration
    let git_credential_file_string = match &config.git {
        Some(config) => config.credentials_file.clone(),
        None => {
            log!(LogLevel::Error, "FAILED TO PASRE CLIENT APPLICATIONS !!!");
            log!(
                LogLevel::Trace,
                "Unable to validate what files to run, missing git credentials file"
            );
            return Err(ErrorArrayItem::new(
                Errors::GeneralError,
                "Failed to parse what applications to run",
            ));
        }
    };

    let git_credential_file: PathType = PathType::Content(git_credential_file_string);
    let git_credentials_array = match GitCredentials::new_vec(Some(&git_credential_file)).await {
        Ok(data) => data,
        Err(err) => {
            log!(LogLevel::Error, "{}", err);
            return Err(err);
        }
    };

    let mut git_project_hashes: Vec<Stringy> = Vec::new();

    for project in git_credentials_array {
        git_project_hashes.push(project.generate_id());
    }

    // filtering out system applications and files that dont match the git config file given to the manager
    let client_applications_names = application_list
        .iter_mut()
        .filter(|data: &&mut String| !SYSTEMAPPLICATIONS.contains(&data.as_str()))
        .filter(|data| {
            let stripped_name = Stringy::from(data.replace("ais_", ""));
            git_project_hashes.contains(&stripped_name)
        });

    // assemble the Struct from the array
    let mut tasks: Vec<task::JoinHandle<Result<ClientApplication, ()>>> = Vec::new();

    for name in client_applications_names {
        let name = name.clone();
        tasks.push(task::spawn(async move {
            let application_path = PathType::Content(format!("/opt/artisan/bin/{}", name));
            let application_state_path = PathType::Content(format!("/tmp/.{}.state", name));
            let application_env_path = PathType::Content(format!("/etc/{}/.env", name));

            // TODO This is where the enviornment file will be sourced
            let env: Option<Enviornment> = if application_env_path.exists() {
                let enviornment: Enviornment =
                    match fs::read_to_string(application_env_path).map_err(ErrorArrayItem::from) {
                        Ok(data) => {
                            let raw_env_file = data.as_bytes();
                            if let Ok(data) = Enviornment::parse(&raw_env_file).await {
                                data
                            } else {
                                log!(LogLevel::Error, "Failed to parse env");
                                return Err(());
                            }
                        }
                        Err(err) => {
                            log!(LogLevel::Error, "Failed to parse env: {}", err.err_mesg);
                            return Err(());
                        }
                    };

                Some(enviornment)
            } else {
                log!(LogLevel::Warn, "No enviornment file for: {}", name);
                None
            };

            let state: AppState = match StatePersistence::load_state(&application_state_path).await
            {
                Ok(state) => state,
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "Couldn't load system app state data: {}",
                        err
                    );
                    return Err(());
                }
            };

            let client_application = ClientApplication {
                name: state.clone().name.into(),
                path: application_path.clone(),
                exists: application_path.exists(),
                config: ApplicationConfig::new(state, env, None),
            };

            Ok(client_application)
        }));
    }

    let mut results: Vec<ClientApplication> = Vec::new();
    for task in tasks {
        match task.await {
            Ok(client) => match client {
                Ok(data) => results.push(data),
                Err(_) => continue,
            },
            Err(err) => {
                log!(
                    LogLevel::Error,
                    "Join error assembling system applications: {}",
                    err
                );
            }
        }
    }

    let mut client_application_array_write_lock: tokio::sync::RwLockWriteGuard<
        '_,
        std::collections::HashMap<Stringy, ClientApplication>,
    > = CLIENT_APPLICATION_ARRAY.try_write().await?;

    for app in results {
        client_application_array_write_lock.insert(app.clone().name, app);
    }

    Ok(())
}
