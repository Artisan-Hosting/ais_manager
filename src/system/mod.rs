// getting state and config data for this application
pub mod config;

// locks and controlls for networking, application array, and portal registration
pub mod control;

// save, load, and manipulating state data
pub mod state;

// portal logic
pub mod portal;

// manager data function
pub mod manager;

// the git monitor's repo list, read and written on behalf of the portal
pub mod git_repos;

// Noise_NK static keys for both simple_comms channels
pub mod noise;

// The TCP ports this manager dials and binds
pub mod ports;

// The wire envelope multiplexed over the manager's tunnel to the portal
pub mod tunnel_wire;
