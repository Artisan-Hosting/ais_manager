use artisan_middleware::dusa_collection_utils::errors::{ErrorArrayItem, Errors};
use aya::programs::Program;
use aya::{include_bytes_aligned, programs::KProbe, Bpf};
use bytemuck::Zeroable; // Only derive Zeroable.
use std::convert::TryInto;
use std::sync::{Arc, RwLock};

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Zeroable)]
#[repr(C)]
pub struct TrafficStats {
    rx_bytes: u64,
    tx_bytes: u64,
}

unsafe impl aya::Pod for TrafficStats {}

#[allow(dead_code)]
pub struct BandwidthTracker {
    bpf: Arc<RwLock<Bpf>>,
}

impl BandwidthTracker {
    pub async fn new() -> Result<Self, ErrorArrayItem> {
        let bpf_data = include_bytes_aligned!("../ebpf/prog.bpf.o");
        let mut bpf = Bpf::load(bpf_data)
            .map_err(|err| ErrorArrayItem::new(Errors::GeneralError, err.to_string()))?;

        for probe in [
            "bpf_tcp_sendmsg",
            "bpf_tcp_recvmsg",
            "bpf_udp_sendmsg",
            "bpf_udp_recvmsg",
        ] {
            let bpf: Result<&mut Program, ErrorArrayItem> =
                if let Some(bpf) = bpf.program_mut(probe) {
                    Ok(bpf)
                } else {
                    Err(ErrorArrayItem::new(
                        Errors::GeneralError,
                        "Error getting bpf application",
                    ))
                };

            let program: &mut KProbe =
                bpf?.try_into()
                    .map_err(|err: aya::programs::ProgramError| {
                        ErrorArrayItem::new(Errors::GeneralError, err.to_string())
                    })?;

            program.load().map_err(|err: aya::programs::ProgramError| {
                ErrorArrayItem::new(Errors::GeneralError, err.to_string())
            })?;

            program
                .attach(probe, 0)
                .map_err(|err: aya::programs::ProgramError| {
                    ErrorArrayItem::new(Errors::GeneralError, err.to_string())
                })?;
        }

        Ok(Self {
            bpf: Arc::new(RwLock::new(bpf)),
        })
    }

    pub async fn view_bandwidth(&self, pid: i32) -> Result<(u64, u64), ErrorArrayItem> {
        let mut bpf = self.bpf.try_write().map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Can't lock bpf handle: {}", err.to_string()),
            )
        })?;

        // Instead of unwrap(), we use ok_or_else to report a helpful error if the map isn’t found.
        let map_data = bpf.take_map("pid_traffic_map").ok_or_else(|| {
            ErrorArrayItem::new(Errors::GeneralError, "failed to find pid_traffic_map")
        })?;

        // Convert the raw map handle into a typed HashMap.
        let map: aya::maps::HashMap<_, u32, TrafficStats> = aya::maps::HashMap::try_from(map_data)
            .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        let stats = map
            .get(&(pid as u32), 0)
            .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        Ok((stats.rx_bytes, stats.tx_bytes))
    }

    pub async fn track_pid(&self, pid: u32) -> Result<(), ErrorArrayItem> {
        let mut bpf = self.bpf.try_write().map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Can't lock bpf handle: {}", err.to_string()),
            )
        })?;

        let map_data = bpf.take_map("pid_traffic_map").ok_or_else(|| {
            ErrorArrayItem::new(Errors::GeneralError, "failed to find pid_traffic_map")
        })?;

        let mut map: aya::maps::HashMap<_, u32, TrafficStats> =
            aya::maps::HashMap::try_from(map_data)
                .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        let initial = TrafficStats {
            rx_bytes: 0,
            tx_bytes: 0,
        };

        match map.insert(pid, initial, 0) {
            Ok(_) => Ok(()),
            Err(err) => Err(ErrorArrayItem::new(Errors::GeneralError, err.to_string())),
        }
    }
}
