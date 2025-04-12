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

//driver for interacting with ebpf system
pub mod ebpf;

// signalling system for  shutdowns and reloads
pub mod signals;