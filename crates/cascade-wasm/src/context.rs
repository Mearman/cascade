//! Deserialise a caller-supplied JSON blob into an [`EvalContext`].
//!
//! All fields are optional; absent values fall back to their zero / unknown
//! defaults so callers only need to supply the fields that matter for the
//! expression being evaluated.

use cascade_expr::context::{
    DeviceContext, DiskContext, EvalContext, FileContext, FileFlags, NetworkContext, NetworkType,
    PeerContext, PowerContext, PowerSource, TimeContext,
};
use chrono::Utc;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct FileInput {
    size: Option<u64>,
    mime: Option<String>,
    ext: Option<String>,
    name: Option<String>,
    cached: Option<bool>,
    pinned: Option<bool>,
    shared: Option<bool>,
    starred: Option<bool>,
    dirty: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct DeviceInput {
    id: Option<String>,
    name: Option<String>,
    arch: Option<String>,
    os: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DiskInput {
    total_bytes: Option<u64>,
    free_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct NetworkInput {
    #[serde(rename = "type")]
    if_type: Option<String>,
    metered: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PowerInput {
    source: Option<String>,
    battery_pct: Option<u8>,
}

#[derive(Debug, Default, Deserialize)]
struct PeerInput {
    online_count: Option<usize>,
    peers_with_file: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct ContextInput {
    file: Option<FileInput>,
    device: Option<DeviceInput>,
    disk: Option<DiskInput>,
    network: Option<NetworkInput>,
    power: Option<PowerInput>,
    peer: Option<PeerInput>,
}

/// Build an [`EvalContext`] from a JSON string.
///
/// # Errors
///
/// Returns an error if `json` is not valid JSON or does not match the expected
/// structure.
pub fn from_json(json: &str) -> anyhow::Result<EvalContext> {
    let input: ContextInput = serde_json::from_str(json)?;

    let file_in = input.file.unwrap_or_default();
    let flags = FileFlags::default()
        .with_cached(file_in.cached.unwrap_or(false))
        .with_pinned(file_in.pinned.unwrap_or(false))
        .with_shared(file_in.shared.unwrap_or(false))
        .with_starred(file_in.starred.unwrap_or(false))
        .with_dirty(file_in.dirty.unwrap_or(false));
    let file = FileContext {
        size: file_in.size.unwrap_or(0),
        mime: file_in.mime.unwrap_or_default(),
        ext: file_in.ext.unwrap_or_default(),
        name: file_in.name.unwrap_or_default(),
        modified: chrono::DateTime::<Utc>::default(),
        owner: String::new(),
        flags,
    };

    let device_in = input.device.unwrap_or_default();
    let device = DeviceContext {
        id: device_in.id.unwrap_or_default(),
        name: device_in.name.unwrap_or_default(),
        tags: Vec::new(),
        arch: device_in.arch.unwrap_or_default(),
        os: device_in.os.unwrap_or_default(),
    };

    let disk_in = input.disk.unwrap_or_default();
    let disk = DiskContext {
        total_bytes: disk_in.total_bytes.unwrap_or(0),
        free_bytes: disk_in.free_bytes.unwrap_or(0),
    };

    let network_in = input.network.unwrap_or_default();
    let if_type = match network_in.if_type.as_deref() {
        Some("wifi") => NetworkType::Wifi,
        Some("ethernet") => NetworkType::Ethernet,
        Some("cellular") => NetworkType::Cellular,
        _ => NetworkType::Unknown,
    };
    let network = NetworkContext {
        if_type,
        metered: network_in.metered.unwrap_or(false),
        bandwidth_bps: None,
    };

    let power_in = input.power.unwrap_or_default();
    let source = match power_in.source.as_deref() {
        Some("ac") => PowerSource::AC,
        Some("battery") => PowerSource::Battery,
        _ => PowerSource::Unknown,
    };
    let power = PowerContext {
        source,
        battery_pct: power_in.battery_pct,
    };

    let peer_in = input.peer.unwrap_or_default();
    let peer = PeerContext {
        online_count: peer_in.online_count.unwrap_or(0),
        peers_with_file: peer_in.peers_with_file.unwrap_or(0),
    };

    Ok(EvalContext {
        file,
        device,
        disk,
        network,
        power,
        time: TimeContext { now: Utc::now() },
        peer,
    })
}
