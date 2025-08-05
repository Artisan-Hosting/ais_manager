use artisan_middleware::aggregator::NetworkUsage;
use artisan_middleware::dusa_collection_utils::core::errors::{ErrorArrayItem, Errors};
use artisan_middleware::dusa_collection_utils::core::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::process_manager::is_pid_active;
use aya::programs::Program;
use aya::{include_bytes_aligned, programs::KProbe, Bpf};
use bytemuck::Zeroable;
use std::collections::HashMap;
// Only derive Zeroable.
use std::convert::TryInto;
use std::path::Path;
use std::sync::RwLock;

use super::control::GLOBAL_STATE;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Zeroable)]
#[repr(C)]
pub struct TrafficStats {
    rx_bytes: u64,
    tx_bytes: u64,
}

unsafe impl aya::Pod for TrafficStats {}

// to avoid adding aya depends to the shared lib
impl TrafficStats {
    pub fn to_network_usage(&self) -> NetworkUsage {
        NetworkUsage {
            rx_bytes: self.rx_bytes,
            tx_bytes: self.tx_bytes,
        }
    }
}

#[allow(dead_code)]
pub struct BandwidthTracker {
    bpf: RwLock<Bpf>,
}

impl BandwidthTracker {
    pub async fn new() -> Result<Self, ErrorArrayItem> {
        if GLOBAL_STATE.initialized() {
            return Err(ErrorArrayItem::new(
                Errors::AppState,
                "Attemping to double initialize ebpf",
            ));
        }

        let bpf_data = include_bytes_aligned!("../ebpf/network.o");
        let mut bpf = Bpf::load(bpf_data)
            .map_err(|err| ErrorArrayItem::new(Errors::GeneralError, err.to_string()))?;

        let probes = [
            ("bpf_tcp_sendmsg", "tcp_sendmsg"),
            ("bpf_tcp_recvmsg", "tcp_cleanup_rbuf"),
            ("bpf_udp_sendmsg", "udp_sendmsg"),
            ("bpf_udp_recvmsg", "udp_recvmsg"),
        ];

        for (prog_name, attach_point) in probes {
            let bpf: Result<&mut Program, ErrorArrayItem> =
                if let Some(bpf) = bpf.program_mut(prog_name) {
                    Ok(bpf)
                } else {
                    Err(ErrorArrayItem::new(
                        Errors::GeneralError,
                        "Error getting bpf application",
                    ))
                };

            let program: &mut KProbe =
                bpf.unwrap()
                    .try_into()
                    .map_err(|err: aya::programs::ProgramError| {
                        ErrorArrayItem::new(Errors::GeneralError, err.to_string())
                    })?;

            program.load().map_err(|e: aya::programs::ProgramError| {
                ErrorArrayItem::new(Errors::GeneralError, e.to_string())
            })?;

            program
                .attach(attach_point, 0)
                .map_err(|e: aya::programs::ProgramError| {
                    ErrorArrayItem::new(Errors::GeneralError, e.to_string())
                })?;

            log!(
                LogLevel::Debug,
                "✅ Successfully attached probe {} to {}",
                prog_name,
                attach_point
            );
        }

        Ok(Self {
            bpf: RwLock::new(bpf),
        })
    }

    #[allow(dead_code)]
    pub async fn view_bandwidth(&self, pid: u32) -> Result<(String, String), ErrorArrayItem> {
        let bpf = self.bpf.try_read().map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Can't lock bpf handle: {}", err.to_string()),
            )
        })?;

        let map_data = bpf.map("pid_traffic_map").ok_or_else(|| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("pid_traffic_map not found — was BandwidthTracker initialized properly?"),
            )
        })?;

        let map: aya::maps::HashMap<_, u32, TrafficStats> = aya::maps::HashMap::try_from(map_data)
            .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        let stats = match map.get(&(pid as u32), 0) {
            Ok(stats) => stats,
            Err(_) => {
                log!(
                    LogLevel::Debug,
                    "ℹ️ PID {} not found in map, returning zeroed stats",
                    pid
                );
                return Ok((Self::format_bytes(0), Self::format_bytes(0)));
            }
        };

        Ok((
            Self::format_bytes(stats.rx_bytes),
            Self::format_bytes(stats.tx_bytes),
        ))
    }

    /// Helper function to format byte counts into human-readable strings.
    fn format_bytes(bytes: u64) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;

        let bytes_f64 = bytes as f64;

        if bytes_f64 >= GB {
            format!("{:.2} GB", bytes_f64 / GB)
        } else if bytes_f64 >= MB {
            format!("{:.2} MB", bytes_f64 / MB)
        } else if bytes_f64 >= KB {
            format!("{:.2} KB", bytes_f64 / KB)
        } else {
            format!("{} B", bytes)
        }
    }

    pub async fn track_pid(&self, pid: u32) -> Result<(), ErrorArrayItem> {
        let mut bpf = self.bpf.try_write().map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Can't lock bpf handle: {}", err.to_string()),
            )
        })?;

        let map_data = bpf.map_mut("pid_traffic_map").ok_or_else(|| {
            ErrorArrayItem::new(Errors::GeneralError, "failed to find pid_traffic_map")
        })?;

        let mut map: aya::maps::HashMap<_, u32, TrafficStats> =
            aya::maps::HashMap::try_from(map_data)
                .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        // First, check if the PID is already tracked
        if map.get(&pid, 0).is_ok() {
            log!(
                LogLevel::Debug,
                "ℹ️ PID {} is already being tracked — skipping insertion.",
                pid
            );
            return Ok(());
        }

        let initial = TrafficStats {
            rx_bytes: 0,
            tx_bytes: 0,
        };

        map.insert(pid, initial, 0)
            .map_err(|err| ErrorArrayItem::new(Errors::GeneralError, err.to_string()))?;

        log!(
            LogLevel::Debug,
            "✅ Started tracking PID {} in BPF map",
            pid
        );

        Ok(())
    }

    pub async fn aggregate_bandwidth_by_service(
        &self,
    ) -> Result<HashMap<String, TrafficStats>, ErrorArrayItem> {
        let mut service_pid_map: HashMap<u32, String> = HashMap::new();
        let artisan_slice = Path::new("/sys/fs/cgroup/artisan.slice/");

        // Step 1: Build PID -> Service map
        for entry in std::fs::read_dir(artisan_slice)? {
            let path = entry?.path();
            let service_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) if name.ends_with(".service") => {
                    name.trim_end_matches(".service").to_string()
                }
                _ => continue,
            };

            let procs_path = path.join("cgroup.procs");
            if let Ok(content) = std::fs::read_to_string(procs_path) {
                for pid_str in content.lines() {
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        service_pid_map.insert(pid, service_name.clone());
                    }
                }
            }
        }

        // Step 2: Prepare aggregated map
        let mut service_traffic: HashMap<String, TrafficStats> = HashMap::new();

        // Step 3: Read from BPF map and aggregate
        let bpf = self.bpf.try_read().map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Can't lock bpf handle: {}", err.to_string()),
            )
        })?;

        let map_data = bpf.map("pid_traffic_map").ok_or_else(|| {
            ErrorArrayItem::new(Errors::GeneralError, "failed to find pid_traffic_map")
        })?;

        let map: aya::maps::HashMap<_, u32, TrafficStats> = aya::maps::HashMap::try_from(map_data)
            .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        let mut iter = map.iter();

        while let Some(Ok((pid, stats))) = iter.next() {
            if let Some(service_name) = service_pid_map.get(&pid) {
                let entry = service_traffic
                    .entry(service_name.clone())
                    .or_insert(TrafficStats {
                        rx_bytes: 0,
                        tx_bytes: 0,
                    });
                entry.rx_bytes += stats.rx_bytes;
                entry.tx_bytes += stats.tx_bytes;
            }
        }

        Ok(service_traffic)
    }

    pub async fn cleanup_dead_pids(&self) -> Result<(), ErrorArrayItem> {
        let mut bpf = self.bpf.try_write().map_err(|err| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                format!("Can't lock bpf handle: {}", err),
            )
        })?;

        let map_data = bpf.map_mut("pid_traffic_map").ok_or_else(|| {
            ErrorArrayItem::new(
                Errors::GeneralError,
                "pid_traffic_map not found — was BandwidthTracker initialized properly?"
                    .to_string(),
            )
        })?;

        let mut map: aya::maps::HashMap<_, u32, TrafficStats> =
            aya::maps::HashMap::try_from(map_data)
                .map_err(|e| ErrorArrayItem::new(Errors::GeneralError, e.to_string()))?;

        // Iterate over all keys
        let mut to_remove = Vec::new();

        let mut iter = map.iter();

        while let Some(Ok((pid, _stats))) = iter.next() {
            if !is_pid_active(pid as i32)? {
                to_remove.push(pid);
            }
        }

        for pid in to_remove {
            map.remove(&pid).map_err(|e| {
                ErrorArrayItem::new(
                    Errors::GeneralError,
                    format!("Failed to remove dead PID {}: {}", pid, e),
                )
            })?;

            log!(
                LogLevel::Debug,
                "🧹 Cleaned up dead PID {} from pid_traffic_map",
                pid
            );
        }

        Ok(())
    }
}

pub fn debug_print_aggregated(service_traffic: HashMap<String, TrafficStats>) {
    for (service, stats) in service_traffic {
        log!(LogLevel::Debug, "Service: {}", service);
        log!(
            LogLevel::Debug,
            "  RX: {}",
            BandwidthTracker::format_bytes(stats.rx_bytes)
        );
        log!(
            LogLevel::Debug,
            "  TX: {}",
            BandwidthTracker::format_bytes(stats.tx_bytes)
        );
    }
}
