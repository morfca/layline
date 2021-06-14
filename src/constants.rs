use tokio::time::Duration;

pub const DEFAULT_MAX_SESSIONS: usize = 100;
pub const DEFAULT_SESSION_TIMEOUT: u32 = 900;
pub const ID_SIZE_BINARY: usize = 16;
pub const ID_SIZE_ASCII_HEX: usize = 32;
pub const BODY_SIZE_LIMIT: usize = (1<<20) * 1;
pub const ONE_SECOND: Duration = Duration::from_secs(1);
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
pub const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(30);
