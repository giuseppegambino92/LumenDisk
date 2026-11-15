use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::transaction_tracker::TransactionTracker;
use crate::vfs::NFSFileSystem;

/// LumenDisk patch: loopback access policy.
#[derive(Debug, Default)]
pub struct Security {
    /// AUTH_UNIX uids allowed to call anything but NULL (empty = no restriction).
    pub allowed_uids: Vec<u32>,
    /// Accept a single MOUNT MNT call; later MNT calls are denied.
    pub single_mount: bool,
    pub mounted: std::sync::atomic::AtomicBool,
    /// Count of rejected calls / connections (for tests and diagnostics).
    pub rejected_calls: std::sync::atomic::AtomicU64,
    pub rejected_connections: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub struct RPCContext {
    pub local_port: u16,
    pub client_addr: String,
    pub auth: crate::rpc::auth_unix,
    pub vfs: Arc<dyn NFSFileSystem + Send + Sync>,
    pub mount_signal: Option<mpsc::Sender<bool>>,
    pub export_name: Arc<String>,
    pub transaction_tracker: Arc<TransactionTracker>,
    pub security: Option<Arc<Security>>,
}

impl fmt::Debug for RPCContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("RPCContext")
            .field("local_port", &self.local_port)
            .field("client_addr", &self.client_addr)
            .field("auth", &self.auth)
            .finish()
    }
}
