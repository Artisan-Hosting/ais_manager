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
use std::sync::Arc;
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr},
};
use tokio::net::TcpStream;

use super::control::{GlobalState, GLOBAL_STATE};
use super::manager::get_manager_data;

#[allow(dead_code)]
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortalAddr {
    pub addr: IpAddr,
    pub port: u32,
}

impl fmt::Display for PortalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addr_colored = self.addr.to_string().blue(); // IP address in cyan
        let port_colored = self.port.to_string().purple(); // Port in yellow

        write!(f, "{}:{}", addr_colored, port_colored)
    }
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
                let a: Result<(), ErrorArrayItem> = if identifier.verify().await {
                    identifier.display_id();
                    identifier.save_to_file()?;
                    Ok(())
                } else {
                    Err(ErrorArrayItem::new(
                        Errors::AuthenticationError,
                        "Identifier verification failed".to_string(),
                    ))
                };
                a
            } else {
                Ok(())
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

/// Populates the global state with portal intances that we've found.
async fn get_portal_addr(config: &AppConfig) -> Result<(), ErrorArrayItem> {
    let global_state: &Arc<GlobalState> = match GLOBAL_STATE.get() {
        Some(gs) => gs,
        None => {
            return Err(ErrorArrayItem::new(
                Errors::AppState,
                "Failed to get the app state from the global state",
            ));
        }
    };

    let portal_port: u32 = 9801;

    let portal_addrs: Option<Vec<IpAddr>> = if config.environment == "development" {
        resolve_url(
            "portal.arhst.net",
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 122, 169))),
        )
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?
    } else {
        resolve_url(
            "portal.arhst.net",
            Some(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1))),
        )
        .await
        .map_err(|err| ErrorArrayItem::new(Errors::Network, err.to_string()))?
    };

    if let Some(addrs) = portal_addrs {
        for ip in addrs {
            let portal: PortalAddr = PortalAddr {
                addr: ip,
                port: portal_port,
            };
            global_state.portal_state.insert(portal).await?;
        }

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
    let message: PortalMessage = PortalMessage::RegisterRequest(data);

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
                //PortalState::portal_linked(PORTAL_CONTROLS.clone()).await?;
                // This should reset any out of time settings
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
    let global_state: &Arc<GlobalState> = match GLOBAL_STATE.get() {
        Some(gs) => gs,
        None => {
            return Err(ErrorArrayItem::new(
                Errors::AppState,
                "Failed to get the app state from the global state",
            ));
        }
    };


    
    get_portal_addr(&state.config).await?;

    for portal in global_state.portal_state.get_portals().await? {
        let mut stream: TcpStream = match portal.connect().await {
            Ok(s) => s,
            Err(err) => {
                log!(LogLevel::Error, "Failed to connect to portal @ {} -> {}", portal.get_address(), err);
                continue;
            }
        };

        if let Err(err) = portal_discovery(&mut stream).await {
            log!(LogLevel::Error, "Failed to exchange identities with portal @ {} -> {}", portal.get_address(), err);
        } else {
            log!(LogLevel::Debug, "Discovered @ {} !", portal.get_address());
        }

        // * Due to a poor protocol implemtation we need to re initialize the connection here
        let mut stream: TcpStream = match portal.connect().await {
            Ok(s) => s,
            Err(err) => {
                log!(LogLevel::Error, "Failed to connect to portal @ {} -> {}", portal.get_address(), err);
                continue;
            }
        };

        match get_manager_data(state).await {
            Ok(data) => {
            if let Err(err) = portal_registration(&mut stream, data).await {
                log!(LogLevel::Error, "Failed to register with portal @ {} -> {}", portal.get_address(), err);
            } else {
                log!(LogLevel::Debug, "Registered with portal @ {} !", portal.get_address());
                global_state.portal_state.set_time(portal.get_address(), true).await?;
            }
            },
            Err(err) => {
                return Err(err);
            },
        }
    }
    
    Ok(())
}
