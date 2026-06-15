//! Conflict-copy path construction and device-id derivation helpers, split
//! out of `sync.rs` to keep that file under the source-length cap. Declared
//! from there via `mod conflict_path;`, so this is a child module of the
//! parent and the helpers are re-exported into the parent's namespace, keeping
//! `super::<helper>` resolution intact for sibling modules and tests.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{FILE_TYPE_FILE, FileInfo, IndexEntry, Result, Version};

/// Build the conflict-copy path for `original`.
///
/// The conflict suffix is inserted before the file extension, so
/// `dir/report.txt` becomes `dir/report.conflict-<id>-<ts>.txt`. A leading
/// dot is treated as part of the stem rather than an extension separator, so
/// `.gitignore` becomes `.gitignore.conflict-<id>-<ts>` with no trailing
/// extension.
///
/// `device_identifier` is the friendly device name when one is configured,
/// otherwise the first eight characters of the device id. Callers are
/// responsible for sanitising it via `sanitise_for_path` before passing it in.
pub(super) fn conflict_copy_path(
    original: &str,
    device_identifier: &str,
    timestamp: i64,
) -> String {
    let (parent, filename) = match original.rsplit_once('/') {
        Some((p, f)) => (Some(p), f),
        None => (None, original),
    };
    let (stem, ext) = split_filename(filename);
    let suffixed = if ext.is_empty() {
        format!("{stem}.conflict-{device_identifier}-{timestamp}")
    } else {
        format!("{stem}.conflict-{device_identifier}-{timestamp}.{ext}")
    };
    match parent {
        Some(p) => format!("{p}/{suffixed}"),
        None => suffixed,
    }
}

/// Sanitise a string for use as a filename component in a conflict-copy
/// path. Replaces any character that is unsafe or noisy in a filename
/// with a single `-` and lowercases the result. Forward slash,
/// backslash, dot, and whitespace are always replaced; a handful of
/// shell-significant characters and any remaining control character
/// are also normalised.
///
/// Replacement is one-for-one — runs of replaced characters become runs
/// of dashes — so `..` becomes `--` and `home/server` becomes
/// `home-server`. Collapsing would alias distinct inputs (`a..b` vs
/// `a-b`) which is undesirable when the identifier is meant to be
/// distinguishing.
///
/// An empty input produces an empty output — the caller is expected to
/// fall back to the short device id when this happens.
pub(super) fn sanitise_for_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let replaced = match ch {
            // Filesystem path separators and the extension separator.
            '/' | '\\' | '.'
            // Whitespace — keeps filenames terminal-friendly.
            | ' ' | '\t' | '\n' | '\r'
            // Shell metacharacters and control bytes get normalised too;
            // these would otherwise need quoting at every use site.
            | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '-',
            // Any other control character is replaced as well so the
            // result is safe to embed in shell output and filenames.
            other if other.is_control() => '-',
            other => other,
        };
        out.push(replaced);
    }
    out.to_lowercase()
}

/// Split a filename into `(stem, extension)` on the LAST `.`. A leading
/// dot is treated as part of the stem (hidden-file convention), not as
/// an extension separator. An empty extension means there is no
/// extension to preserve.
pub(super) fn split_filename(filename: &str) -> (&str, &str) {
    // Skip the leading dot for the purposes of finding the extension
    // separator — `.gitignore` is a stem, not a stem + ext. `split_at`
    // panics on an out-of-bounds index; the `min(filename.len())` guard
    // makes the bound trivially in range and avoids the workspace's
    // `indexing_slicing` lint.
    let search_start = usize::from(filename.starts_with('.'));
    let (_, search_slice) = filename.split_at(search_start.min(filename.len()));
    search_slice.rfind('.').map_or((filename, ""), |rel_idx| {
        let abs_idx = search_start + rel_idx;
        let (stem, dot_ext) = filename.split_at(abs_idx);
        // Strip the leading '.' from the extension half. `dot_ext` is
        // non-empty (it starts with the `.` we just located via
        // `rfind`).
        let (_, ext) = dot_ext.split_at(1);
        (stem, ext)
    })
}

pub(super) fn entry_to_file_info(entry: &IndexEntry) -> Result<FileInfo> {
    let block_size = cascade_p2p::block::block_size_for_file(entry.size);
    let mut hashes = Vec::with_capacity(entry.block_hashes.len() / 32);
    for chunk in entry.block_hashes.chunks(32) {
        let mut h = [0u8; 32];
        if chunk.len() != 32 {
            anyhow::bail!("malformed block_hashes column: trailing partial hash");
        }
        h.copy_from_slice(chunk);
        hashes.push(h);
    }
    Ok(FileInfo {
        name: entry.path.clone(),
        file_type: FILE_TYPE_FILE,
        size: entry.size,
        modified: entry.modified,
        // Sequence space is per-INDEX (one FolderIndex per backend instance)
        // and the per-row `row_version` is monotonic across upserts and
        // tombstones, so it is exactly the per-device sequence number BEP
        // expects. See `FileInfo::sequence` for the per-index/per-device
        // equivalence note.
        sequence: u64::try_from(entry.row_version).unwrap_or(0),
        block_size,
        deleted: entry.deleted,
        // The backend has no mid-write or permission-denied state for
        // an `IndexEntry` today, so locally-produced rows always emit
        // these flags as false. The receive path respects the wire
        // fields when peers set them.
        invalid: false,
        no_permissions: false,
        version: Version {
            counters: entry.version.clone(),
        },
        block_hashes: hashes,
    })
}

/// Derive this device's 64-bit short id from its persistent device id.
///
/// `DeviceIdentity::device_id` is a 52-character base32 SHA-256 of the
/// TLS certificate. Hashing again and folding to 8 bytes gives a
/// stable per-device u64 to use as the version vector entry key.
pub(super) fn derive_device_short_id(device_id: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(device_id.as_bytes());
    // SHA-256 is always 32 bytes; take the first 8 via `chunks_exact`
    // so the slice access satisfies the workspace's `indexing_slicing`
    // lint without requiring an `#[allow]` escape.
    let (head, _) = digest.as_slice().split_at(8);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(head);
    u64::from_be_bytes(buf)
}

/// Return the first 8 characters of `device_id` for use as a short,
/// human-readable identifier in conflict-copy paths. `DeviceIdentity::device_id`
/// is a base32-encoded SHA-256 (52 chars), so 8 chars is plenty to
/// distinguish devices in practice without overflowing path budgets.
pub(super) fn local_short_device_id(device_id: &str) -> String {
    let take = device_id.len().min(8);
    let (head, _) = device_id.split_at(take);
    head.to_string()
}

/// Current wall-clock time as seconds since the Unix epoch. Used to
/// stamp conflict-copy filenames so concurrent edits at the same path
/// produce distinct sibling paths.
pub(super) fn unix_timestamp_seconds() -> i64 {
    let now = SystemTime::now();
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // Saturating cast — wall-clock seconds within i64 range for ~292B
    // years; the only way to hit the ceiling is a malformed clock, in
    // which case the saturating value is still a valid (if odd) sibling
    // path stamp.
    i64::try_from(secs).unwrap_or(i64::MAX)
}
