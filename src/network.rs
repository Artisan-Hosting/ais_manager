use std::{net::SocketAddr, sync::Arc, time::Duration};
use gethostname::gethostname;
use artisan_middleware::{
    aggregator::{AppMessage, Command, CommandResponse, CommandType}, config::AppConfig, dusa_collection_utils::{errors::Errors, functions::current_timestamp}, git_actions::GitCredentials, identity, portal::ManagerData, state_persistence::AppState
};
use artisan_middleware::dusa_collection_utils::{errors::ErrorArrayItem, log, types::PathType};
use artisan_middleware::dusa_collection_utils::{log::LogLevel, stringy::Stringy};
use simple_comms::{
    network::{send_receive::{send_data, send_empty_err}, utils::get_local_ip},
    protocol::{
        flags::Flags, header::EOL, io_helpers::read_until, message::ProtocolMessage, proto::Proto,
    },
};
use tokio::net::TcpStream;

use crate::{
    applications::{
        child::{APP_STATUS_ARRAY, CLIENT_APPLICATION_ARRAY, SYSTEM_APPLICATION_ARRAY},
        start_stop::{reload_application, start_application, stop_application},
    },
    system::{control::Controls, manager::get_manager_data, portal::load_identifier},
};

pub async fn process_tcp(
    mut connection: (TcpStream, SocketAddr),
    application_controls: Arc<Controls>,
    state: &mut AppState,
    state_path: &PathType,
    config: &AppConfig,
) -> Result<(), ErrorArrayItem> {
    let proto: Proto = Proto::TCP;

    let mut buffer = read_until(&mut connection.0, EOL.to_vec()).await?;
    if let Some(pos) = buffer.windows(EOL.len()).rposition(|window| window == EOL) {
        buffer.truncate(pos);
    }

    let recieved_message: ProtocolMessage<AppMessage> =
        ProtocolMessage::<AppMessage>::from_bytes(&buffer).await?;

    let recieved_payload = recieved_message.get_payload().await;
    let _recieved_header = recieved_message.get_header().await;

    match recieved_payload {
        AppMessage::Command(command) => {
            match command_processor(command, application_controls, state, state_path, config).await
            {
                Ok(data) => {
                    let message: ProtocolMessage<AppMessage> =
                        // ProtocolMessage::new(Flags::COMPRESSED | Flags::ENCRYPTED, data)?;
                        ProtocolMessage::new(Flags::NONE, data)?;
                    let message_bytes: Vec<u8> = message.format().await?;
                    send_data(&mut connection.0, message_bytes, proto).await?;
                }
                Err(err) => return Err(err),
            }
        }

        _ => {
            // * illegal in this context
            send_empty_err(&mut connection.0, proto).await?;
            return Ok(());
        }
    }

    Ok(())
}

async fn command_processor(
    command: Command,
    application_controls: Arc<Controls>,
    state: &mut AppState,
    state_path: &PathType,
    config: &AppConfig,
) -> Result<AppMessage, ErrorArrayItem> {
    if let Err(err) = application_controls
        .wait_for_network_control_with_timeout(Duration::from_secs(5))
        .await
    {
        log!(LogLevel::Error, "{}", err);
        return Ok(AppMessage::Response(CommandResponse {
            app_id: "".into(),
            command_type: CommandType::Custom("Unknown".into()),
            success: false,
            message: Some("Server not accepting requests".to_owned()),
        }));
    }

    let app_id: Stringy = command.app_id;
    match command.command_type {
        artisan_middleware::aggregator::CommandType::Start => {
            match start_application(&app_id, state, state_path, config).await {
                Ok(_) => {
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id,
                        command_type: CommandType::Start,
                        success: true,
                        message: None,
                    }))
                }
                Err(err) => {
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id,
                        command_type: CommandType::Start,
                        success: false,
                        message: Some(err.to_string()),
                    }));
                }
            }
        }
        artisan_middleware::aggregator::CommandType::Stop => {
            if app_id == "ais_manager".into() {
                application_controls.signal_reload();
                return Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: true,
                    message: Some("triggered reload !".to_owned()),
                }));
            }

            match stop_application(&app_id).await {
                Ok(_) => {
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id,
                        command_type: CommandType::Stop,
                        success: true,
                        message: None,
                    }))
                }
                Err(err) => {
                    log!(LogLevel::Error, "Failed to stop {}, {}", app_id, err);
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id,
                        command_type: CommandType::Stop,
                        success: false,
                        message: Some(err.to_string()),
                    }));
                }
            }
        }
        artisan_middleware::aggregator::CommandType::Restart => {
            // Check if the request is a self restart first
            if app_id == "ais_manager".into() {
                application_controls.signal_reload();
                return Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: true,
                    message: None,
                }));
            }

            match reload_application(&app_id).await {
                Ok(_) => {
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id,
                        command_type: CommandType::Restart,
                        success: true,
                        message: None,
                    }))
                }
                Err(err) => {
                    log!(LogLevel::Error, "Failed to stop {}, {}", app_id, err);
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id,
                        command_type: CommandType::Restart,
                        success: false,
                        message: Some(err.to_string()),
                    }));
                }
            }
        }
        artisan_middleware::aggregator::CommandType::Status => {
            let store_lock = APP_STATUS_ARRAY
                .try_read_with_timeout(Some(Duration::from_secs(5)))
                .await?;

            if store_lock.contains_key(&app_id) {
                match store_lock.get(&app_id) {
                    Some(app) => {
                        let response_data = AppMessage::Response(CommandResponse {
                            app_id,
                            command_type: CommandType::Status,
                            success: true,
                            message: app.to_json(),
                        });
                        return Ok(response_data);
                    }
                    None => {
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id: app_id.clone(),
                            command_type: CommandType::Status,
                            success: false,
                            message: Some(format!("The app: {}, wasn't in our store", app_id)),
                        }))
                    }
                }
            }

            drop(store_lock);

            return Ok(AppMessage::Response(CommandResponse {
                app_id: app_id.clone(),
                command_type: CommandType::Status,
                success: false,
                message: Some(format!("The app: {}, wasn't in our store", app_id)),
            }));
        }
        artisan_middleware::aggregator::CommandType::AllStatus => {
            let store_lock = APP_STATUS_ARRAY
                .try_read_with_timeout(Some(Duration::from_secs(2)))
                .await?;

            let mut status_vec = Vec::new();

            for (id, status) in store_lock.iter() {
                log!(LogLevel::Debug, "Sending status of: {}", id);
                status_vec.push(status.clone().to_json().unwrap());
            }

            status_vec.shrink_to_fit();
            let mut data = String::new();

            for status in status_vec {
                data.push_str(&format!("{},", status));
            }

            let response_data = AppMessage::Response(CommandResponse {
                app_id,
                command_type: CommandType::AllStatus,
                success: true,
                message: Some(format!("[{}]", data).replace(",]", "]")),
            });

            drop(store_lock);

            return Ok(response_data);
        }

        artisan_middleware::aggregator::CommandType::Info => {
            let manager_data: ManagerData = get_manager_data(state).await?;
            return Ok(AppMessage::ManagerInfo(manager_data));
        }

        _ => {
            return Ok(AppMessage::Response(CommandResponse {
                app_id,
                command_type: CommandType::Custom("command not found".to_string()),
                success: false,
                message: Some("Request not implemented".into()),
            }))
        }
    }
}
