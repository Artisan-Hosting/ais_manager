use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::dusa_collection_utils::logger::LogLevel;
use artisan_middleware::{
    config::AppConfig,
    dusa_collection_utils::errors::{ErrorArrayItem, Errors},
    identity::Identifier,
    network::resolve_url,
    portal::{ManagerData, PortalMessage},
    state_persistence::AppState,
};
use colored::Colorize;
use simple_comms::{
    network::send_receive::{send_empty_ok, send_message},
    protocol::{flags::Flags, proto::Proto},
};
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr},
};
use tokio::net::TcpStream;

use super::{
    control::{PortalState, PORTAL_CONTROLS},
    manager::get_manager_data,
};

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct PortalAddr {
    addr: IpAddr,
    port: u32,
}

impl fmt::Display for PortalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addr_colored = self.addr.to_string().blue(); // IP address in cyan
        let port_colored = self.port.to_string().purple(); // Port in yellow

        write!(f, "{}:{}", addr_colored, port_colored)
    }
}

async fn establish_portal_connection(address: &PortalAddr) -> Result<TcpStream, ErrorArrayItem> {
    TcpStream::connect(format!("{}:{}", address.addr, address.port))
        .await
        .map_err(ErrorArrayItem::from)
}

async fn portal_discovery(stream: &mut TcpStream) -> Result<(), ErrorArrayItem> {
    let discovery = PortalMessage::Discover;
    match send_message::<TcpStream, PortalMessage, PortalMessage>(
        stream,
        Flags::NONE,
        discovery,
        Proto::TCP,
        false,
    )
    .await
    {
        Ok(response) => match response {
            Ok(message) => match message.get_payload().await {
                PortalMessage::IdRequest => handle_identity_request(stream).await,
                _ => Err(ErrorArrayItem::new(
                    Errors::AuthenticationError,
                    "Unexpected response to Discover".to_string(),
                )),
            },
            Err(status) => Err(ErrorArrayItem::new(
                Errors::ConnectionError,
                format!("Error during Discover: {}", status),
            )),
        },
        Err(err) => {
            let mut a_err = ErrorArrayItem::from(err);
            let msg = a_err.err_mesg.clone();
            a_err.err_mesg = format!("Response from server: {}", msg).into();
            Err(a_err)
        }
    }
}

async fn handle_identity_request(stream: &mut TcpStream) -> Result<(), ErrorArrayItem> {
    let id: Option<Identifier> = load_identifier().await;
    let identity: PortalMessage = PortalMessage::IdResponse(id.clone());

    match send_message::<TcpStream, PortalMessage, PortalMessage>(
        stream,
        Flags::ENCRYPTED | Flags::COMPRESSED,
        identity,
        Proto::TCP,
        true,
    )
    .await
    {
        Ok(response) => match response {
            Ok(message) => handle_identity_response(stream, message.get_payload().await).await,
            Err(status) => Err(ErrorArrayItem::new(
                Errors::ConnectionError,
                format!("Error during identity exchange: {}", status),
            )),
        },
        Err(err) => Err(ErrorArrayItem::from(err)),
    }
}

async fn handle_identity_response(
    stream: &mut TcpStream,
    message: PortalMessage,
) -> Result<(), ErrorArrayItem> {
    match message {
        PortalMessage::IdResponse(identifier) => {
            if let Some(identifier) = identifier {
                // * we are done communicating end the conn and finish up id work
                let _ = send_empty_ok(stream, Proto::TCP).await;
                if identifier.verify().await {
                    identifier.display_id();
                    identifier.save_to_file()?;
                    PortalState::set_identity(Some(identifier), PORTAL_CONTROLS.clone()).await?;
                    Ok(())
                } else {
                    Err(ErrorArrayItem::new(
                        Errors::AuthenticationError,
                        "Identifier verification failed".to_string(),
                    ))
                }
            } else {
                let identifier: Identifier = Identifier::load_from_file()?;
                if identifier.verify().await {
                    identifier.display_id();
                    identifier.save_to_file()?;
                    PortalState::set_identity(Some(identifier), PORTAL_CONTROLS.clone()).await?;
                    Ok(())
                } else {
                    Err(ErrorArrayItem::new(
                        Errors::AuthenticationError,
                        "Identifier verification failed".to_string(),
                    ))
                }
            }
        }
        PortalMessage::Error(err) => Err(ErrorArrayItem::new(Errors::ConnectionError, err)),
        _ => Err(ErrorArrayItem::new(
            Errors::ConnectionError,
            "Unexpected message payload".to_string(),
        )),
    }
}

pub async fn load_identifier() -> Option<Identifier> {
    match Identifier::load_from_file() {
        Ok(data) => Some(data),
        Err(_) => {
            log!(LogLevel::Warn, "System has no identity!");
            None
        }
    }
}

async fn get_portal_addr(config: &AppConfig) -> Result<(), ErrorArrayItem> {
    let portal_port: u32 = 9801;

    let portal_addrs: Option<Vec<IpAddr>> = if config.environment == "development" {
        resolve_url(
            "portal.arhst.net",
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 122, 169))),
        )
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?
    } else {
        resolve_url("portal.arhst.net", Some(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1))))
            .await
            .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?
    };

    if let Some(addrs) = portal_addrs {
        let mut portals: Vec<PortalAddr> = Vec::new();
        for ip in addrs {
            let portal: PortalAddr = PortalAddr {
                addr: ip,
                port: portal_port,
            };
            portals.push(portal);
        }

        PortalState::set_portal_addrs(PORTAL_CONTROLS.clone(), portals).await?;
        return Ok(());
    }

    return Err(ErrorArrayItem::new(
        Errors::Network,
        "Failed to locate the portal".to_owned(),
    ));
}

async fn portal_registration(
    mut stream: &mut TcpStream,
    data: ManagerData,
) -> Result<(), ErrorArrayItem> {
    let message = PortalMessage::RegisterRequest(data);

    match send_message::<TcpStream, PortalMessage, PortalMessage>(
        &mut stream,
        Flags::ENCRYPTED | Flags::COMPRESSED,
        message,
        Proto::TCP,
        false,
    )
    .await?
    {
        Ok(response) => match response.get_payload().await {
            PortalMessage::RegisterResponse(_res) => {
                PortalState::portal_linked(PORTAL_CONTROLS.clone()).await?;
                return Ok(());
            }
            PortalMessage::Error(err) => {
                return Err(ErrorArrayItem::new(
                    Errors::Network,
                    format!("Server responded : {}", err),
                ));
            }
            _ => {
                return Err(ErrorArrayItem::new(
                    Errors::Network,
                    "Recieved illagal response".to_owned(),
                ));
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

#[rustfmt::skip]
pub async fn connect_with_portal(state:&mut AppState ) -> Result<(), ErrorArrayItem> {
    if PortalState::is_portal_linked(PORTAL_CONTROLS.clone()).await? {
        let addrs = PortalState::portal_addrs(PORTAL_CONTROLS.clone()).await?;
        for addr in addrs {
            log!(LogLevel::Debug, "Already linked to portal: {}", addr);
        }        
        return Ok(());
    }
    
    get_portal_addr(&state.config).await?;

    for portal_conn in PortalState::portal_addrs(PORTAL_CONTROLS.clone()).await? {
        let mut stream: TcpStream = match establish_portal_connection(&portal_conn).await {
            Ok(s) => s,
            Err(err) => {
                log!(LogLevel::Error, "Failed to connect to portal @ {} -> {}", portal_conn, err);
                continue;
            }
        };

        if let Err(err) = portal_discovery(&mut stream).await {
            log!(LogLevel::Error, "Failed to exchange identities with portal @ {} -> {}", portal_conn, err);
        } else {
            log!(LogLevel::Debug, "Discovered @ {} !", portal_conn);
        }

        let mut stream: TcpStream = match establish_portal_connection(&portal_conn).await {
            Ok(s) => s,
            Err(err) => {
                log!(LogLevel::Error, "Failed to connect to portal @ {} -> {}", portal_conn, err);
                continue;
            }
        };

        match get_manager_data(state).await {
            Ok(data) => {
            if let Err(err) = portal_registration(&mut stream, data).await {
                log!(LogLevel::Error, "Failed to register with portal @ {} -> {}", portal_conn, err);
            } else {
                log!(LogLevel::Debug, "Registered with portal @ {} !", portal_conn);
                PortalState::portal_linked(PORTAL_CONTROLS.clone()).await?;
            }
            },
            Err(err) => {
                return Err(err);
            },
        }
    }
    
    Ok(())
}
