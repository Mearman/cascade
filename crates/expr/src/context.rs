//! Evaluation context structs — the data an expression is evaluated against.

use chrono::{DateTime, Datelike, Timelike, Utc};

/// Full evaluation context for an expression.
#[derive(Debug, Clone)]
pub struct EvalContext {
    pub file: FileContext,
    pub device: DeviceContext,
    pub disk: DiskContext,
    pub network: NetworkContext,
    pub power: PowerContext,
    pub time: TimeContext,
    pub peer: PeerContext,
}

/// File-level context.
#[derive(Debug, Clone)]
pub struct FileContext {
    pub size: u64,
    pub mime: String,
    pub ext: String,
    pub name: String,
    pub modified: DateTime<Utc>,
    pub owner: String,
    pub shared: bool,
    pub starred: bool,
    pub dirty: bool,
    pub cached: bool,
    pub pinned: bool,
}

impl FileContext {
    pub fn age(&self) -> chrono::Duration {
        Utc::now() - self.modified
    }

    pub fn year(&self) -> i32 {
        self.modified.year()
    }
}

/// Device context.
#[derive(Debug, Clone)]
pub struct DeviceContext {
    pub id: String,
    pub name: String,
    pub tags: Vec<String>,
    pub arch: String,
    pub os: String,
}

/// Disk context.
#[derive(Debug, Clone)]
pub struct DiskContext {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl DiskContext {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes - self.free_bytes
    }
}

/// Network context.
#[derive(Debug, Clone)]
pub struct NetworkContext {
    pub if_type: NetworkType,
    pub metered: bool,
    pub bandwidth_bps: Option<u64>,
}

/// Network interface type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkType {
    Wifi,
    Ethernet,
    Cellular,
    Unknown,
}

impl std::fmt::Display for NetworkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wifi => write!(f, "wifi"),
            Self::Ethernet => write!(f, "ethernet"),
            Self::Cellular => write!(f, "cellular"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Power context.
#[derive(Debug, Clone)]
pub struct PowerContext {
    pub source: PowerSource,
    pub battery_pct: Option<f64>,
}

/// Power source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    AC,
    Battery,
    Unknown,
}

impl std::fmt::Display for PowerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AC => write!(f, "ac"),
            Self::Battery => write!(f, "battery"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Time context.
#[derive(Debug, Clone)]
pub struct TimeContext {
    pub now: DateTime<Utc>,
}

impl TimeContext {
    pub fn hour(&self) -> u32 {
        self.now.hour()
    }

    pub fn is_weekday(&self) -> bool {
        use chrono::Datelike;
        self.now.weekday().number_from_monday() <= 5
    }
}

/// Peer context (P2P).
#[derive(Debug, Clone, Default)]
pub struct PeerContext {
    pub online_count: usize,
    pub peers_with_file: usize,
}

impl Default for EvalContext {
    fn default() -> Self {
        Self {
            file: FileContext {
                size: 0,
                mime: String::new(),
                ext: String::new(),
                name: String::new(),
                modified: Utc::now(),
                owner: String::new(),
                shared: false,
                starred: false,
                dirty: false,
                cached: false,
                pinned: false,
            },
            device: DeviceContext {
                id: String::new(),
                name: String::new(),
                tags: Vec::new(),
                arch: String::new(),
                os: String::new(),
            },
            disk: DiskContext {
                total_bytes: 0,
                free_bytes: 0,
            },
            network: NetworkContext {
                if_type: NetworkType::Unknown,
                metered: false,
                bandwidth_bps: None,
            },
            power: PowerContext {
                source: PowerSource::Unknown,
                battery_pct: None,
            },
            time: TimeContext { now: Utc::now() },
            peer: PeerContext::default(),
        }
    }
}
