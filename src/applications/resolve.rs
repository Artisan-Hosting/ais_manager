use artisan_middleware::config::AppConfig;
use artisan_middleware::dusa_collection_utils::errors::ErrorArrayItem;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::{log::LogLevel, types::PathType};
use artisan_middleware::enviornment::definitions::Enviornment_V1;
use artisan_middleware::git_actions::GitCredentials;
use artisan_middleware::state_persistence::{AppState, StatePersistence};
use serde::{Deserialize, Serialize};
use std::{fmt, fs};
use tokio::task;

// pub static SYSTEMAPPLICATIONS: [&'static str; 4] = ["gitmon", "ids", "self", "messenger"];
pub static SYSTEMAPPLICATIONS: [&'static str; 2] = ["gitmon", "self"];

#[derive(Debug, Serialize, Deserialize)]
pub enum Applications {
    System(Vec<SystemApplication>),
    Client(Vec<ClientApplication>),
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemApplication {
    pub name: String,
    pub path: PathType,
    pub exists: bool,
    pub state: Option<AppState>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClientApplication {
    pub name: String,
    pub path: PathType,
    pub exists: bool,
    pub state: Option<AppState>,
    pub environ: Option<Enviornment_V1>,
}

impl fmt::Display for ClientApplication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ClientApplication {{\n  name: {},\n  path: {},\n  exists: {},\n  state: {},\n  environ: {}\n}}",
            self.name,
            self.path,
            self.exists,
            self.state.as_ref().map_or("None".to_string(), |s| s.to_string()),
            self.environ
                .as_ref()
                .map_or("None".to_string(), |e| e.to_string())
        )
    }
}

pub struct _ClientEnviornment {
    uid: u32,
    gid: u32,
}

#[allow(unused_assignments)]
pub async fn resolve_system_applications() -> Vec<SystemApplication> {
    let system_application_names: Vec<String> = SYSTEMAPPLICATIONS
        .iter()
        .map(|app_name| {
            if *app_name == "self" {
                "ais_manager".to_string()
            } else {
                format!("ais_{}", app_name)
            }
        })
        .collect();

    // assemble the Struct from the array
    let mut tasks: Vec<task::JoinHandle<SystemApplication>> = Vec::new();

    for name in system_application_names {
        let name = name.clone();
        tasks.push(task::spawn(async move {
            let application_path = PathType::Content(format!("/opt/artisan/bin/{}", name));
            let application_state_path = PathType::Content(format!("/tmp/.{}.state", name));

            let state: Option<AppState> = if application_state_path.exists() {
                match StatePersistence::load_state(&application_state_path).await {
                    Ok(state) => Some(state),
                    Err(err) => {
                        log!(LogLevel::Error, "{}", err);
                        None
                    }
                }
            } else {
                None
            };

            if name != "ais_manager" {
                SystemApplication {
                    name,
                    path: application_path.clone(),
                    exists: application_path.exists(),
                    state,
                }
            } else {
                SystemApplication {
                    name,
                    path: PathType::Content(format!("/opt/artisan/bin/ais_manager")),
                    exists: true,
                    state,
                }
            }
        }));
    }

    let mut results: Vec<SystemApplication> = Vec::new();
    for task in tasks {
        match task.await {
            Ok(system) => results.push(system),
            Err(err) => {
                log!(
                    LogLevel::Error,
                    "Join error assembling system applications: {}",
                    err
                );
            }
        }
    }

    return results;
}

#[allow(unused_assignments)]
pub async fn resolve_client_applications(config: &AppConfig) -> Vec<ClientApplication> {
    let mut application_list = Vec::new();

    let dir_read: fs::ReadDir = match fs::read_dir("/opt/artisan/bin") {
        Ok(data) => data,
        Err(err) => {
            log!(
                LogLevel::Error,
                "Failed to read bins from /opt/artisan/bin: {}",
                err
            );
            return Vec::new();
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
            return Vec::new();
        }
    };

    let git_credential_file: PathType = PathType::Content(git_credential_file_string);
    let git_credentials_array = match GitCredentials::new_vec(Some(&git_credential_file)).await {
        Ok(data) => data,
        Err(err) => {
            log!(LogLevel::Error, "{}", err);
            return Vec::new();
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
    let mut tasks: Vec<task::JoinHandle<ClientApplication>> = Vec::new();

    for name in client_applications_names {
        let name = name.clone();
        tasks.push(task::spawn(async move {
            let application_path = PathType::Content(format!("/opt/artisan/bin/{}", name));
            let application_state_path = PathType::Content(format!("/tmp/.{}.state", name));
            let application_env_path = PathType::Content(format!("/etc/{}/.env", name));

            // TODO This is where the enviornment file will be sourced
            let env: Option<Enviornment_V1> = if application_state_path.exists() {
                let encrypted_content: Option<Vec<u8>> =
                    match fs::read_to_string(application_env_path).map_err(ErrorArrayItem::from) {
                        Ok(data) => Some(data.as_bytes().to_vec()),
                        Err(err) => {
                            log!(LogLevel::Error, "{}", err);
                            None
                        }
                    };

                if let Some(data) = encrypted_content {
                    match Enviornment_V1::parse_from(&data).await {
                        Ok(data) => Some(data),
                        Err(err) => {
                            log!(LogLevel::Error, "{}", err);
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                log!(LogLevel::Warn, "No enviornment file for: {}", name);
                None
            };

            let state: Option<AppState> = if application_state_path.exists() {
                match StatePersistence::load_state(&application_state_path).await {
                    Ok(state) => Some(state),
                    Err(err) => {
                        log!(LogLevel::Error, "{}", err);
                        None
                    }
                }
            } else {
                None
            };

            ClientApplication {
                name,
                path: application_path.clone(),
                exists: application_path.exists(),
                state,
                environ: env,
            }
        }));
    }

    let mut results: Vec<ClientApplication> = Vec::new();
    for task in tasks {
        match task.await {
            Ok(system) => results.push(system),
            Err(err) => {
                log!(
                    LogLevel::Error,
                    "Join error assembling system applications: {}",
                    err
                );
            }
        }
    }

    return results;
}
