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

/// Boolean flags for a file entry, packed into a single byte.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileFlags(u8);

impl FileFlags {
    const SHARED: u8 = 1 << 0;
    const STARRED: u8 = 1 << 1;
    const DIRTY: u8 = 1 << 2;
    const CACHED: u8 = 1 << 3;
    const PINNED: u8 = 1 << 4;

    /// Set the `cached` flag to the given value.
    #[must_use]
    pub const fn with_cached(self, value: bool) -> Self {
        if value {
            Self(self.0 | Self::CACHED)
        } else {
            Self(self.0 & !Self::CACHED)
        }
    }

    /// Set the `pinned` flag to the given value.
    #[must_use]
    pub const fn with_pinned(self, value: bool) -> Self {
        if value {
            Self(self.0 | Self::PINNED)
        } else {
            Self(self.0 & !Self::PINNED)
        }
    }

    /// Set the `shared` flag to the given value.
    #[must_use]
    pub const fn with_shared(self, value: bool) -> Self {
        if value {
            Self(self.0 | Self::SHARED)
        } else {
            Self(self.0 & !Self::SHARED)
        }
    }

    /// Set the `starred` flag to the given value.
    #[must_use]
    pub const fn with_starred(self, value: bool) -> Self {
        if value {
            Self(self.0 | Self::STARRED)
        } else {
            Self(self.0 & !Self::STARRED)
        }
    }

    /// Set the `dirty` flag to the given value.
    #[must_use]
    pub const fn with_dirty(self, value: bool) -> Self {
        if value {
            Self(self.0 | Self::DIRTY)
        } else {
            Self(self.0 & !Self::DIRTY)
        }
    }

    /// Returns whether the `shared` flag is set.
    #[must_use]
    pub const fn shared(self) -> bool {
        self.0 & Self::SHARED != 0
    }

    /// Returns whether the `starred` flag is set.
    #[must_use]
    pub const fn starred(self) -> bool {
        self.0 & Self::STARRED != 0
    }

    /// Returns whether the `dirty` flag is set.
    #[must_use]
    pub const fn dirty(self) -> bool {
        self.0 & Self::DIRTY != 0
    }

    /// Returns whether the `cached` flag is set.
    #[must_use]
    pub const fn cached(self) -> bool {
        self.0 & Self::CACHED != 0
    }

    /// Returns whether the `pinned` flag is set.
    #[must_use]
    pub const fn pinned(self) -> bool {
        self.0 & Self::PINNED != 0
    }
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
    pub flags: FileFlags,
}

impl FileContext {
    /// Returns whether this file is shared.
    #[must_use]
    pub const fn shared(&self) -> bool {
        self.flags.shared()
    }

    /// Returns whether this file is starred.
    #[must_use]
    pub const fn starred(&self) -> bool {
        self.flags.starred()
    }

    /// Returns whether this file is dirty (locally modified).
    #[must_use]
    pub const fn dirty(&self) -> bool {
        self.flags.dirty()
    }

    /// Returns whether this file is cached locally.
    #[must_use]
    pub const fn cached(&self) -> bool {
        self.flags.cached()
    }

    /// Returns whether this file is pinned.
    #[must_use]
    pub const fn pinned(&self) -> bool {
        self.flags.pinned()
    }

    #[must_use]
    pub fn age(&self) -> chrono::Duration {
        Utc::now() - self.modified
    }

    #[must_use]
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
#[derive(Debug, Clone, Copy)]
pub struct DiskContext {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl DiskContext {
    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.total_bytes - self.free_bytes
    }
}

/// Network context.
#[derive(Debug, Clone, Copy)]
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
#[derive(Debug, Clone, Copy)]
pub struct PowerContext {
    pub source: PowerSource,
    /// Battery percentage 0–100, or `None` if no battery is present.
    pub battery_pct: Option<u8>,
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
#[derive(Debug, Clone, Copy)]
pub struct TimeContext {
    pub now: DateTime<Utc>,
}

impl TimeContext {
    #[must_use]
    pub fn hour(&self) -> u32 {
        self.now.hour()
    }

    #[must_use]
    pub fn is_weekday(&self) -> bool {
        use chrono::Datelike;
        self.now.weekday().number_from_monday() <= 5
    }
}

/// Peer context (P2P).
#[derive(Debug, Clone, Copy, Default)]
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
                flags: FileFlags::default(),
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
