use artisan_middleware::dusa_collection_utils::core::errors::ErrorArrayItem;
use artisan_middleware::dusa_collection_utils::core::logger::LogLevel;
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::process_manager::SupervisedProcess;
use nix::unistd::Pid;

/// This will verify, The child
/// returned will have no monitors or metric trackin enabled. The caller need to
/// set up threading to enable resource monitoring
pub async fn reclaim_child(pid: u32) -> Result<SupervisedProcess, ErrorArrayItem> {
    let proper_pid = Pid::from_raw(pid as i32);
    log!(LogLevel::Debug, "Reclaiming pid: {}", pid);
    SupervisedProcess::new(proper_pid)
}
