use std::{net::SocketAddr, sync::Arc, time::Duration};

use artisan_middleware::{
    aggregator::{AppMessage, Command, CommandResponse, CommandType},
    control::ToggleControl,
    systemd::SystemdService,
};
use dusa_collection_utils::{errors::ErrorArrayItem, log};
use dusa_collection_utils::{log::LogLevel, stringy::Stringy};
use simple_comms::{network::send_receive::{send_data, send_empty_err}, protocol::{flags::Flags, header::EOL, io_helpers::read_until, message::ProtocolMessage, proto::Proto}};
use tokio::{net::TcpStream, sync::Notify};

use crate::AppStatusStore;

pub async fn process_tcp(
    mut connection: (TcpStream, SocketAddr),
    reload: Arc<Notify>,
    execution: Arc<ToggleControl>,
    app_status_store: AppStatusStore,
) -> Result<(), ErrorArrayItem> {
    let proto: Proto = Proto::TCP;

    let mut buffer = read_until(&mut connection.0, EOL.as_bytes().to_vec()).await?;
    if let Some(pos) = buffer
        .windows(EOL.len())
        .rposition(|window| window == EOL.as_bytes())
    {
        buffer.truncate(pos);
    }

    let recieved_message: ProtocolMessage<AppMessage> =
        ProtocolMessage::<AppMessage>::from_bytes(&buffer).await?;

    let recieved_payload = recieved_message.get_payload().await;
    let _recieved_header = recieved_message.get_header().await;

    match recieved_payload {
        AppMessage::Command(command) => {
            match command_processor(command, execution, reload, app_status_store).await {
                Ok(data) => {
                    let message: ProtocolMessage<AppMessage> = ProtocolMessage::new(
                        Flags::OPTIMIZED,
                        data,
                    )?;
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
    execution: Arc<ToggleControl>,
    reload: Arc<Notify>,
    app_status_store: AppStatusStore,
) -> Result<AppMessage, ErrorArrayItem> {
    if let Err(err) = execution.wait_with_timeout(Duration::from_secs(5)).await {
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
            // attempts to link the app the the service on the system
            match SystemdService::new(&app_id) {
                Ok(service) => match service.start() {
                    Ok(_) => {
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id,
                            command_type: CommandType::Start,
                            success: true,
                            message: None,
                        }))
                    }
                    Err(err) => {
                        log!(LogLevel::Error, "Failed to start {}, {}", app_id, err);
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id,
                            command_type: CommandType::Start,
                            success: false,
                            message: Some(err.to_string()),
                        }));
                    }
                },
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "linking requested app {}, to systemd service failed: {}",
                        app_id,
                        err
                    );
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id: "".into(),
                        command_type: CommandType::Start,
                        success: false,
                        message: Some(format!(
                            "linking requested app {}, to systemd service failed: {}",
                            app_id, err
                        )),
                    }));
                }
            }
        }
        artisan_middleware::aggregator::CommandType::Stop => {
            // attempts to link the app the the service on the system
            match SystemdService::new(&app_id) {
                Ok(service) => match service.start() {
                    Ok(_) => {
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id,
                            command_type: CommandType::Start,
                            success: true,
                            message: None,
                        }))
                    }
                    Err(err) => {
                        log!(LogLevel::Error, "Failed to stop {}, {}", app_id, err);
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id,
                            command_type: CommandType::Start,
                            success: false,
                            message: Some(err.to_string()),
                        }));
                    }
                },
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "linking requested app {}, to systemd service failed: {}",
                        app_id,
                        err
                    );
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id: "".into(),
                        command_type: CommandType::Start,
                        success: false,
                        message: Some(format!(
                            "linking requested app {}, to systemd service failed: {}",
                            app_id, err
                        )),
                    }));
                }
            }
        }
        artisan_middleware::aggregator::CommandType::Restart => {
            // Check if the request is a self restart first
            if app_id == "Aggregator".into() {
                reload.notify_one();
                return Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: true,
                    message: None,
                }));
            }

            match SystemdService::new(&app_id) {
                Ok(service) => match service.restart() {
                    Ok(_) => {
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id: app_id.clone(),
                            command_type: CommandType::Restart,
                            success: true,
                            message: None,
                        }))
                    }
                    Err(err) => {
                        log!(LogLevel::Error, "Failed to restart {}, {}", app_id, err);
                        return Ok(AppMessage::Response(CommandResponse {
                            app_id: app_id.clone(),
                            command_type: CommandType::Restart,
                            success: false,
                            message: Some(format!("Failed to restart {}, {}", app_id, err)),
                        }));
                    }
                },
                Err(err) => {
                    log!(
                        LogLevel::Error,
                        "linking requested app {}, to systemd service failed: {}",
                        app_id,
                        err
                    );
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id: "".into(),
                        command_type: CommandType::Start,
                        success: false,
                        message: Some(format!(
                            "linking requested app {}, to systemd service failed: {}",
                            app_id, err
                        )),
                    }));
                }
            }
        }
        artisan_middleware::aggregator::CommandType::Status => {
            let store_clone = app_status_store.clone();

            let store_lock = store_clone
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

            return Ok(AppMessage::Response(CommandResponse {
                app_id: app_id.clone(),
                command_type: CommandType::Status,
                success: false,
                message: Some(format!("The app: {}, wasn't in our store", app_id)),
            }));
        }
        artisan_middleware::aggregator::CommandType::AllStatus => {
            let store_clone = app_status_store.clone();

            let store_lock = store_clone
                .try_read_with_timeout(Some(Duration::from_secs(1)))
                .await?;

            let mut status_vec = Vec::new();

            for (id, status) in store_lock.iter() {
                log!(LogLevel::Info, "Sending status of: {}", id);
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
            return Ok(response_data);
        }
        _ => {
            return Ok(AppMessage::Response(CommandResponse {
                app_id,
                command_type: CommandType::Status,
                success: false,
                message: Some("Request not implemented".into()),
            }))
        }
    }
}
