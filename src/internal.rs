use std::{sync::Arc, time::Duration};

use artisan_middleware::{
    aggregator::{AppMessage, CommandResponse, CommandType},
    communication_proto::{read_until, send_data, Flags, Proto, ProtocolMessage, EOL},
    control::ToggleControl,
    timestamp::current_timestamp,
};
use dusa_collection_utils::log;
use dusa_collection_utils::{errors::ErrorArrayItem, log::LogLevel};
use tokio::net::{unix::SocketAddr, UnixStream};

use crate::{deregister_app, register_app, AppStatusStore, AppUpdateTimeStore};

pub async fn process_unix(
    mut connection: (UnixStream, SocketAddr),
    execution: Arc<ToggleControl>,
    app_status_store: AppStatusStore,
    app_update_time_store: AppUpdateTimeStore,
) -> Result<(), ErrorArrayItem> {
    let proto: Proto = Proto::TCP;

    let mut buffer: Vec<u8> = read_until(&mut connection.0, EOL.as_bytes().to_vec())
        .await
        .map_err(ErrorArrayItem::from)?;

    // Truncate the EOL from the buffer
    if let Some(pos) = buffer
        .windows(EOL.len())
        .rposition(|window| window == EOL.as_bytes())
    {
        buffer.truncate(pos);
    }

    match command_processor(
        buffer,
        execution.clone(),
        app_status_store,
        app_update_time_store,
    )
    .await
    {
        Ok(data) => {
            let message: ProtocolMessage<AppMessage> = ProtocolMessage::new(Flags::NONE, data)?;
            let message_bytes: Vec<u8> = message.format().await?;
            send_data(&mut connection.0, message_bytes, proto).await?;
            Ok(())
        }
        Err(err) => return Err(err),
    }
}

async fn command_processor(
    buffer: Vec<u8>,
    execution: Arc<ToggleControl>,
    app_status_store: AppStatusStore,
    app_update_time_store: AppUpdateTimeStore,
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
    
    log!(LogLevel::Trace, "{:?}", buffer);

    match ProtocolMessage::<AppMessage>::from_bytes(&buffer).await {
        Ok(recieved) => {
            let payload = recieved.get_payload().await;

            match payload {
                AppMessage::Register(app) => {
                    let app_status_store = app_status_store.clone();
                    let app_update_time_store = app_update_time_store.clone();

                    if let Err(err) = register_app(
                        &app,
                        app_status_store.clone(),
                        app_update_time_store.clone(),
                    )
                    .await
                    {
                        log!(LogLevel::Error, "Error registering app: {}", err);
                        let response = AppMessage::Response(CommandResponse {
                            app_id: app.app_id,
                            command_type: CommandType::Custom("Register".into()),
                            success: false,
                            message: Some(format!("Error registering app: {}", err)),
                        });
                        return Ok(response);
                    };

                    return Ok(AppMessage::Response(CommandResponse {
                        app_id: app.app_id,
                        command_type: CommandType::Custom("Register".into()),
                        success: true,
                        message: None,
                    }));
                }
                AppMessage::Deregister(app) => {
                    let store_clone = app_status_store.clone();
                    if let Err(err) = deregister_app(&app, store_clone).await {
                        log!(LogLevel::Error, "Error De-registering app: {}", err);
                        let response = AppMessage::Response(CommandResponse {
                            app_id: app.app_id,
                            command_type: CommandType::Custom("Register".into()),
                            success: false,
                            message: Some(format!("Error De-registering app: {}", err)),
                        });
                        return Ok(response);
                    }

                    return Ok(AppMessage::Response(CommandResponse {
                        app_id: app.app_id,
                        command_type: CommandType::Custom("Deregister".into()),
                        success: true,
                        message: None,
                    }));
                }
                AppMessage::Update(app) => {
                    let mut app_status_store_write_lock = app_status_store
                        .try_write_with_timeout(Some(Duration::from_secs(2)))
                        .await
                        .map_err(|mut err| {
                            err.err_mesg.push_str("status store");
                            err
                        })?;

                    let mut app_update_time_store_write_lock = app_update_time_store
                        .try_write_with_timeout(Some(Duration::from_secs(2)))
                        .await
                        .map_err(|mut err| {
                            err.err_mesg.push_str("time store");
                            err
                        })?;

                    match app_status_store_write_lock.contains_key(&app.clone().app_id) {
                        true => {
                            let stored_app = app_status_store_write_lock
                                .get_mut(&app.clone().app_id)
                                .unwrap();
                            stored_app.status = app.status;

                            app_update_time_store_write_lock
                                .insert(stored_app.app_id.clone(), current_timestamp());

                            stored_app.error = app.error;

                            if app.metrics.is_some() {
                                stored_app.metrics = app.metrics;
                            }

                            drop(app_status_store_write_lock);
                            drop(app_update_time_store_write_lock);

                            return Ok(AppMessage::Response(CommandResponse {
                                app_id: app.app_id,
                                command_type: CommandType::Custom("Register".into()),
                                success: true,
                                message: None,
                            }));
                        }
                        false => {
                            drop(app_status_store_write_lock);
                            drop(app_update_time_store_write_lock);
                            // We tried to update an app that didnt exist
                            return Ok(AppMessage::Response(CommandResponse {
                                app_id: app.app_id,
                                command_type: CommandType::Custom("Register".into()),
                                success: false,
                                message: Some(format!(
                                    "Tried to update an app that wasn't registered"
                                )),
                            }));
                        }
                    }
                }
                _ => {
                    return Ok(AppMessage::Response(CommandResponse {
                        app_id: "".into(),
                        command_type: CommandType::Custom("Unknown".into()),
                        success: false,
                        message: Some(format!("Illegal request recieved")),
                    }))
                }
            }
        }
        Err(err) => {
            log!(LogLevel::Error, "failed to parse: {}", err);
            return Err(ErrorArrayItem::from(err));
        }
    }
}
