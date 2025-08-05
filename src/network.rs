use artisan_middleware::dusa_collection_utils::core::errors::Errors;
use artisan_middleware::dusa_collection_utils::{core::errors::ErrorArrayItem, log};
use artisan_middleware::{
    aggregator::{AppMessage, Command, CommandResponse, CommandType},
    dusa_collection_utils::{core::logger::LogLevel, core::types::stringy::Stringy},
    portal::ManagerData,
    state_persistence::AppState,
};
use simple_comms::{
    network::send_receive::{send_data, send_empty_err},
    protocol::{
        flags::Flags, header::EOL, io_helpers::read_until, message::ProtocolMessage, proto::Proto,
    },
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpStream;

use crate::system::control::{GlobalState, GLOBAL_STATE};
use crate::{
    applications::{
        child::APP_STATUS_ARRAY,
        start_stop::{reload_application, start_application, stop_application},
    },
    system::manager::get_manager_data,
};

pub async fn process_tcp(mut connection: (TcpStream, SocketAddr)) -> Result<(), ErrorArrayItem> {
    let proto: Proto = Proto::TCP;

    let mut buffer = read_until(&mut connection.0, EOL.to_vec()).await?;
    if let Some(pos) = buffer.windows(EOL.len()).rposition(|window| window == EOL) {
        buffer.truncate(pos);
    }

    let recieved_message: ProtocolMessage<AppMessage> =
        ProtocolMessage::<AppMessage>::from_bytes(&buffer).await?;

    let recieved_payload = recieved_message.get_payload().await;

    // TODO Security ?
    // let recieved_header = recieved_message.get_header().await;
    // let flags = Flags::from_bits_truncate(recieved_header.flags);
    // if !flags.contains(Flags::ENCRYPTED | Flags::SIGNATURE) {
    //     log!(LogLevel::Trace, "Asking client to resend, they sent a insecure command");
    //     let mut message = ProtocolMessage::new(Flags::NONE, ())?;
    //     message.header.reserved = Flags::OPTIMIZED.bits();
    //     message.header.status = simple_comms::protocol::status::ProtocolStatus::SIDEGRADE.bits();
    //     let message_bytes: Vec<u8> = message.format().await?;
    //     send_data(&mut connection.0, message_bytes, proto).await?;
    // }

    match recieved_payload {
        AppMessage::Command(command) => match command_processor(command).await {
            Ok(data) => {
                let message: ProtocolMessage<AppMessage> =
                    ProtocolMessage::new(Flags::ENCRYPTED | Flags::COMPRESSED, data)?;
                let message_bytes: Vec<u8> = message.format().await?;
                send_data(&mut connection.0, message_bytes, proto).await?;
            }
            Err(err) => return Err(err),
        },

        _ => {
            // * illegal in this context
            send_empty_err(&mut connection.0, proto).await?;
            return Ok(());
        }
    }

    Ok(())
}

async fn command_processor(command: Command) -> Result<AppMessage, ErrorArrayItem> {
    let global_state: &Arc<GlobalState> = match GLOBAL_STATE.get() {
        Some(gs) => gs,
        None => {
            return Err(ErrorArrayItem::new(
                Errors::AppState,
                "Failed to get the app state from the global state",
            ));
        }
    };

    let mut app_state: AppState = global_state.get_state_clone().await?;

    if let Err(err) = global_state
        .locks
        .wait_for_network_control_with_timeout(Duration::from_secs(1))
        .await
    {
        log!(LogLevel::Error, "{}", err);
        return Ok(AppMessage::Response(CommandResponse {
            app_id: "".into(),
            command_type: command.command_type,
            success: false,
            message: Some("Server not accepting requests".to_owned()),
        }));
    }

    let app_id: Stringy = command.app_id;
    match command.command_type {
        artisan_middleware::aggregator::CommandType::Start => {
            match start_application(&app_id).await {
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
                global_state.signals.signal_shutdown();
                return Ok(AppMessage::Response(CommandResponse {
                    app_id,
                    command_type: CommandType::Restart,
                    success: true,
                    message: Some("triggered manager shutdown !".to_owned()),
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
                global_state.signals.signal_reload();
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
                        let mut app = app.clone();
                        app.timestamp = 0;

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
            let manager_data: ManagerData = get_manager_data(&mut app_state).await?;
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
