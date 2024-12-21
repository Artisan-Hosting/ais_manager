use std::net::IpAddr;

use artisan_middleware::{
    communication_proto::{send_empty_ok, send_message, Flags, Proto}, config::AppConfig, identity::Identifier, network::{get_external_ip, get_local_ip, resolve_url}, portal::PortalMessage
};
use dusa_collection_utils::{errors::{ErrorArrayItem, Errors}, stringy::Stringy};
use dusa_collection_utils::log;
use dusa_collection_utils::log::LogLevel;
use tokio::{io::AsyncWriteExt, net::TcpStream};

pub async fn query_portal(portal_address: Stringy) -> Result<(), ErrorArrayItem> {
    
    let mut stream = TcpStream::connect(portal_address.to_string()).await?;

    let discovery = PortalMessage::Discover;

    match send_message::<TcpStream, PortalMessage, PortalMessage>(
        &mut stream,
        Flags::NONE,
        discovery,
        Proto::TCP,
        false,
    )
    .await
    {
        Ok(response) => match response {
            Ok(message) => match message.get_payload().await {
                PortalMessage::IdRequest => {
                    let id: Option<Identifier> = match Identifier::load_from_file() {
                        Ok(data) => Some(data),
                        Err(err) => {
                            log!(LogLevel::Error, "failed to load identity: {}", err);
                            None
                        }
                    };
                    let identity = PortalMessage::IdResponse(id);
                    match send_message::<TcpStream, PortalMessage, PortalMessage>(
                        &mut stream,
                        Flags::NONE,
                        identity,
                        Proto::TCP,
                        false,
                    )
                    .await
                    {
                        Ok(response) => match response {
                            Ok(message) => match message.get_payload().await {
                                PortalMessage::IdResponse(identifier) => {
                                    if let Some(identifier) = identifier {
                                        // * we are done communicating end the conn and finish up id work
                                        let _ = send_empty_ok(&mut stream, Proto::TCP).await;
                                        if identifier.verify().await {
                                            identifier.display_id();
                                            identifier.save_to_file()?;
                                            return Ok(());
                                        }
                                    } else {
                                        let identifier: Identifier = Identifier::load_from_file()?;
                                        if identifier.verify().await {
                                            identifier.display_id();
                                            identifier.save_to_file()?;
                                            return Ok(());
                                        }
                                    }
                                }
                                _ => {
                                    log!(LogLevel::Error, "Recieved illegal message");
                                    let _ = stream.shutdown().await;
                                }
                            },
                            Err(status) => {
                                log!(
                                    LogLevel::Error,
                                    "Id request failed, response from server: {}",
                                    status
                                );
                            }
                        },
                        Err(err) => {
                            log!(LogLevel::Error, "Identity exchange error response: {}", err)
                        }
                    }
                }
                _ => {
                    log!(LogLevel::Error, "Recieved illegal message");
                    let _ = stream.shutdown().await?;
                }
            },
            Err(status) => {
                log!(
                    LogLevel::Error,
                    "Id excange failed, response from server: {}",
                    status
                );
            }
        },
        Err(err) => return Err(ErrorArrayItem::from(err)),
    };

    return Err(ErrorArrayItem::new(
        Errors::AuthenticationError,
        "Failed to discovery and validate potral and identify".to_owned(),
    ));
}

pub async fn get_portal_addr(config: &AppConfig) -> Result<String, ErrorArrayItem> {
    let portal_port: i32 = 9801;

    if config.environment == "development" {
        return Ok(format!("192.168.122.169:{}", portal_port));
    }


    let portal_addrs: Option<Vec<IpAddr>> = resolve_url("portal.arhst.net", None)
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?;

    if let Some(mut addrs) = portal_addrs {
        if addrs.len() > 0 {
            let portal_addr: IpAddr = addrs.pop().unwrap();
            return Ok(format!("{}:{}", portal_addr, portal_port));
        }
    }

    return Err(ErrorArrayItem::new(
        Errors::Network,
        "Failed to locate the portal".to_owned(),
    ));
}

pub async fn register_with_portal(
    portal_address: String,
    client: Identifier,
    config: &AppConfig,
) -> Result<(), ErrorArrayItem> {
    let mut stream = TcpStream::connect(portal_address).await?;

    let ip: IpAddr = match config.environment == "development" {
        true => IpAddr::from(get_local_ip()),
        false => IpAddr::from(get_external_ip().await?),
    };

    let message = PortalMessage::RegisterRequest(client, ip);

    match send_message::<TcpStream, PortalMessage, PortalMessage>(
        &mut stream,
        Flags::ENCRYPTED | Flags::ENCODED | Flags::COMPRESSED,
        message,
        Proto::TCP,
        false,
    )
    .await?
    {
        Ok(response) => match response.get_payload().await {
            PortalMessage::RegisterResponse(_res) => {
                return Ok(())
            },
            PortalMessage::Error(err) => {
                return Err(ErrorArrayItem::new(
                    Errors::Network,
                    format!("Server responded : {}", err),
                ));
            }
            _ => {
                return Err(ErrorArrayItem::new(Errors::Network, "Recieved illagal response".to_owned()));
            }
        },
        Err(status) => {
            return Err(ErrorArrayItem::new(
                Errors::Network,
                format!("Server responded : {}", status),
            ));
        }
    }
}
