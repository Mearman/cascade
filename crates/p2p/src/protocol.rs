//! BEP (Block Exchange Protocol) message types and XDR codec.
//!
//! Messages are length-prefixed XDR: a 4-byte big-endian length followed by
//! the XDR-encoded message body. The message type is the first uint32 in the
//! body, allowing the decoder to dispatch to the correct deserialiser.

use std::io;

use anyhow::Result;

// ── Message type constants ──

const MSG_CLUSTER_CONFIG: u32 = 0;
const MSG_INDEX: u32 = 1;
const MSG_INDEX_UPDATE: u32 = 2;
const MSG_REQUEST: u32 = 3;
const MSG_RESPONSE: u32 = 4;
const MSG_PING: u32 = 5;
const MSG_CLOSE: u32 = 6;

// ── XDR primitives ──

fn encode_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_be_bytes());
}

fn encode_u64(buf: &mut Vec<u8>, val: u64) {
    buf.extend_from_slice(&val.to_be_bytes());
}

fn encode_i64(buf: &mut Vec<u8>, val: i64) {
    buf.extend_from_slice(&val.to_be_bytes());
}

fn encode_opaque(buf: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    let len = u32::try_from(data.len())
        .map_err(|_| anyhow::anyhow!("opaque data length {} exceeds u32", data.len()))?;
    encode_u32(buf, len);
    buf.extend_from_slice(data);
    let pad = (4 - (data.len() % 4)) % 4;
    buf.extend(std::iter::repeat_n(0u8, pad));
    Ok(())
}

fn encode_string(buf: &mut Vec<u8>, s: &str) -> Result<()> {
    encode_opaque(buf, s.as_bytes())
}

fn decode_u32(data: &[u8]) -> io::Result<(u32, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<4>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "need 4 bytes for uint32"))?;
    Ok((u32::from_be_bytes(*bytes), rest))
}

fn decode_u64(data: &[u8]) -> io::Result<(u64, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<8>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "need 8 bytes for uint64"))?;
    Ok((u64::from_be_bytes(*bytes), rest))
}

fn decode_i64(data: &[u8]) -> io::Result<(i64, &[u8])> {
    let (bytes, rest) = data
        .split_first_chunk::<8>()
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "need 8 bytes for int64"))?;
    Ok((i64::from_be_bytes(*bytes), rest))
}

fn decode_opaque(data: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let (len, rest) = decode_u32(data)?;
    let len = usize::try_from(len).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if rest.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "opaque data truncated",
        ));
    }
    let (opaque_data, remainder) = rest.split_at(len);
    let pad = (4 - (len % 4)) % 4;
    let remainder = remainder.get(pad..).unwrap_or(&[]);
    Ok((opaque_data, remainder))
}

fn decode_string(data: &[u8]) -> io::Result<(String, &[u8])> {
    let (bytes, rest) = decode_opaque(data)?;
    let s = String::from_utf8(bytes.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((s, rest))
}

// ── Protocol types ──

/// A folder shared between peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// Unique folder identifier.
    pub id: String,
    /// Human-readable label.
    pub label: String,
}

/// A version vector — one `(device_short_id, counter)` entry per device
/// that has ever modified the file. An empty vector means the row has
/// never been written.
///
/// Ordering rules (Syncthing-compatible):
/// - A *dominates* B when every counter in B is less than or equal to
///   the corresponding counter in A, and A has at least one entry that
///   is strictly greater than the matching entry in B (or present in A
///   and absent in B with a non-zero counter).
/// - Equal vectors (`a == b`) do not dominate one another.
/// - When neither dominates the other, the two versions are concurrent
///   — a conflict, in which case the caller must decide how to resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Version {
    /// Sorted ascending by `device_short_id` for a stable wire encoding
    /// and deterministic comparisons.
    pub counters: Vec<(u64, u64)>,
}

impl Version {
    /// Increment this device's counter, inserting a new entry at
    /// counter 1 if the device is not yet present.
    pub fn bump(&mut self, device_short_id: u64) {
        if let Some(entry) = self
            .counters
            .iter_mut()
            .find(|(id, _)| *id == device_short_id)
        {
            entry.1 += 1;
        } else {
            self.counters.push((device_short_id, 1));
            self.counters.sort_by_key(|(id, _)| *id);
        }
    }

    /// `true` if `self` dominates `other`.
    ///
    /// `self` dominates `other` when every counter in `other` is less
    /// than or equal to the corresponding counter in `self`, and at
    /// least one entry in `self` is strictly greater than the matching
    /// entry in `other` (treating absent entries as zero). Equal
    /// vectors are not considered to dominate — use `==` for equality.
    #[must_use]
    pub fn dominates(&self, other: &Self) -> bool {
        let mut at_least_one_greater = false;
        for (other_id, other_ctr) in &other.counters {
            let self_ctr = self
                .counters
                .iter()
                .find(|(id, _)| id == other_id)
                .map_or(0, |(_, c)| *c);
            if self_ctr < *other_ctr {
                return false;
            }
            if self_ctr > *other_ctr {
                at_least_one_greater = true;
            }
        }
        // Any non-zero counter present in self but absent in other
        // implies self has additional history beyond other.
        for (self_id, self_ctr) in &self.counters {
            if *self_ctr > 0 && !other.counters.iter().any(|(id, _)| id == self_id) {
                at_least_one_greater = true;
            }
        }
        at_least_one_greater
    }

    /// Merge `other` into `self`, taking the maximum of each device's
    /// counter. Entries present only in `other` are inserted.
    pub fn merge(&mut self, other: &Self) {
        for (other_id, other_ctr) in &other.counters {
            if let Some(entry) = self.counters.iter_mut().find(|(id, _)| id == other_id) {
                entry.1 = entry.1.max(*other_ctr);
            } else {
                self.counters.push((*other_id, *other_ctr));
            }
        }
        self.counters.sort_by_key(|(id, _)| *id);
    }
}

/// Description of a file's blocks as announced in Index messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// File name (relative to folder root).
    pub name: String,
    /// File type: 0 = file, 1 = directory.
    pub file_type: u32,
    /// Total file size in bytes.
    pub size: u64,
    /// Last modification time (Unix timestamp seconds).
    pub modified: i64,
    /// Per-row monotonic sequence number assigned by the sending peer's
    /// folder index. The receiver records the maximum sequence it has
    /// seen from each peer so that on reconnect only entries with a
    /// sequence greater than the last-seen value need to be sent — the
    /// delta-sync optimisation described in BEP.
    ///
    /// Sequence space is per-INDEX (i.e. per backend instance) on the
    /// sender, not strictly per-DEVICE. Since each device runs exactly
    /// one [`FolderIndex`] in the current implementation, the two are
    /// equivalent here, but a future multi-folder-per-device design
    /// would need a per-(device, folder) tracking key.
    pub sequence: u64,
    /// Block size used for this file.
    pub block_size: u32,
    /// Tombstone flag. When `true`, the row records a delete event:
    /// the peer should mark its local copy deleted (subject to the
    /// version-vector comparison on `version`).
    pub deleted: bool,
    /// When `true`, the row's content is mid-write or otherwise in an
    /// inconsistent state on the sender. Receivers must NOT request
    /// blocks for this entry and must not upsert its content; the row
    /// is silently skipped at debug-log level.
    ///
    /// Currently only respected on receive — local producers do not
    /// emit `invalid: true` yet because the backend has no
    /// mid-write state for an `IndexEntry`. The wire field is in
    /// place so producers can be added later without a protocol bump.
    pub invalid: bool,
    /// When `true`, the sending device exists and knows about the file
    /// but cannot share its content (typically a permission-denied
    /// error reading the local row). Receivers must not request blocks
    /// for this entry and must not upsert its content.
    ///
    /// Currently only respected on receive — local producers do not
    /// emit `no_permissions: true` yet because the backend has no
    /// per-row permission-check infrastructure. The wire field is in
    /// place so producers can be added later without a protocol bump.
    pub no_permissions: bool,
    /// Per-file version vector. Used to detect concurrent edits that
    /// happened on disconnected peers — a strict generalisation of the
    /// previous `modified`-only LWW comparison.
    pub version: Version,
    /// SHA-256 hashes of each block, in order.
    pub block_hashes: Vec<[u8; 32]>,
}

/// BEP message types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BepMessage {
    /// Exchange folder configuration on connect.
    ClusterConfig { folders: Vec<Folder> },
    /// Announce files and blocks.
    Index {
        folder: String,
        files: Vec<FileInfo>,
    },
    /// Incremental update when files change.
    IndexUpdate {
        folder: String,
        files: Vec<FileInfo>,
    },
    /// Request a specific block from a peer.
    Request {
        /// Monotonic per-peer correlation id chosen by the requester.
        /// The peer must echo this id in its [`BepMessage::Response`] so
        /// the requester can route the payload to the right waiter,
        /// allowing many concurrent requests on one connection.
        request_id: u64,
        folder: String,
        name: String,
        block_offset: u64,
        block_size: u32,
        block_hash: [u8; 32],
    },
    /// Send block data.
    Response {
        /// Echoes the `request_id` of the [`BepMessage::Request`] this
        /// response satisfies.
        request_id: u64,
        data: Vec<u8>,
    },
    /// Keepalive.
    Ping,
    /// Graceful connection teardown.
    Close { reason: String },
}

impl BepMessage {
    const fn msg_type(&self) -> u32 {
        match self {
            Self::ClusterConfig { .. } => MSG_CLUSTER_CONFIG,
            Self::Index { .. } => MSG_INDEX,
            Self::IndexUpdate { .. } => MSG_INDEX_UPDATE,
            Self::Request { .. } => MSG_REQUEST,
            Self::Response { .. } => MSG_RESPONSE,
            Self::Ping => MSG_PING,
            Self::Close { .. } => MSG_CLOSE,
        }
    }
}

// ── Encoding ──

/// Encode a BEP message into a length-prefixed XDR frame.
///
/// Wire format: `[4-byte length][4-byte msg type][XDR body...]`
pub fn encode_message(msg: &BepMessage) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    encode_u32(&mut body, msg.msg_type());

    match msg {
        BepMessage::ClusterConfig { folders } => {
            encode_u32(
                &mut body,
                u32::try_from(folders.len()).map_err(|_| anyhow::anyhow!("too many folders"))?,
            );
            for folder in folders {
                encode_string(&mut body, &folder.id)?;
                encode_string(&mut body, &folder.label)?;
            }
        }
        BepMessage::Index { folder, files } | BepMessage::IndexUpdate { folder, files } => {
            encode_string(&mut body, folder)?;
            encode_u32(
                &mut body,
                u32::try_from(files.len()).map_err(|_| anyhow::anyhow!("too many files"))?,
            );
            encode_file_infos(&mut body, files)?;
        }
        BepMessage::Request {
            request_id,
            folder,
            name,
            block_offset,
            block_size,
            block_hash,
        } => {
            encode_u64(&mut body, *request_id);
            encode_string(&mut body, folder)?;
            encode_string(&mut body, name)?;
            encode_u64(&mut body, *block_offset);
            encode_u32(&mut body, *block_size);
            encode_opaque(&mut body, block_hash)?;
        }
        BepMessage::Response { request_id, data } => {
            encode_u64(&mut body, *request_id);
            encode_opaque(&mut body, data)?;
        }
        BepMessage::Ping => {}
        BepMessage::Close { reason } => {
            encode_string(&mut body, reason)?;
        }
    }

    let body_len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("frame body too large for u32 length prefix"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    encode_u32(&mut frame, body_len);
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn encode_file_infos(buf: &mut Vec<u8>, files: &[FileInfo]) -> Result<()> {
    for fi in files {
        encode_string(buf, &fi.name)?;
        encode_u32(buf, fi.file_type);
        encode_u64(buf, fi.size);
        encode_i64(buf, fi.modified);
        encode_u64(buf, fi.sequence);
        encode_u32(buf, fi.block_size);
        encode_u32(buf, u32::from(fi.deleted));
        encode_u32(buf, u32::from(fi.invalid));
        encode_u32(buf, u32::from(fi.no_permissions));
        encode_version(buf, &fi.version)?;
        encode_u32(
            buf,
            u32::try_from(fi.block_hashes.len())
                .map_err(|_| anyhow::anyhow!("too many block hashes"))?,
        );
        for hash in &fi.block_hashes {
            encode_opaque(buf, hash)?;
        }
    }
    Ok(())
}

fn encode_version(buf: &mut Vec<u8>, version: &Version) -> Result<()> {
    encode_u32(
        buf,
        u32::try_from(version.counters.len())
            .map_err(|_| anyhow::anyhow!("version vector too long"))?,
    );
    for (id, ctr) in &version.counters {
        encode_u64(buf, *id);
        encode_u64(buf, *ctr);
    }
    Ok(())
}

// ── Decoding ──

/// Decode a BEP message from a length-prefixed XDR frame.
///
/// Expects the full frame including the 4-byte length prefix.
pub fn decode_message(frame: &[u8]) -> Result<BepMessage> {
    let (body_len_u32, body) =
        decode_u32(frame).map_err(|e| anyhow::anyhow!("invalid frame length: {e}"))?;
    let body_len = usize::try_from(body_len_u32)
        .map_err(|_| anyhow::anyhow!("frame length too large for this platform"))?;
    if body.len() < body_len {
        anyhow::bail!(
            "frame body truncated: expected {body_len} bytes, got {}",
            body.len()
        );
    }
    let body = body
        .get(..body_len)
        .ok_or_else(|| anyhow::anyhow!("frame body slice out of bounds"))?;

    let (msg_type, rest) =
        decode_u32(body).map_err(|e| anyhow::anyhow!("invalid message type: {e}"))?;

    match msg_type {
        MSG_CLUSTER_CONFIG => decode_cluster_config(rest),
        MSG_INDEX => decode_index(rest),
        MSG_INDEX_UPDATE => decode_index_update(rest),
        MSG_REQUEST => decode_request(rest),
        MSG_RESPONSE => decode_response(rest),
        MSG_PING => Ok(BepMessage::Ping),
        MSG_CLOSE => decode_close(rest),
        _ => anyhow::bail!("unknown message type: {msg_type}"),
    }
}

fn decode_cluster_config(data: &[u8]) -> Result<BepMessage> {
    let (count, mut data) = decode_u32(data)?;
    let mut folders = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (id, rest) = decode_string(data)?;
        let (label, rest) = decode_string(rest)?;
        folders.push(Folder { id, label });
        data = rest;
    }
    Ok(BepMessage::ClusterConfig { folders })
}

fn decode_index(data: &[u8]) -> Result<BepMessage> {
    let (folder, rest) = decode_string(data)?;
    let (files, _) = decode_file_infos(rest)?;
    Ok(BepMessage::Index { folder, files })
}

fn decode_index_update(data: &[u8]) -> Result<BepMessage> {
    let (folder, rest) = decode_string(data)?;
    let (files, _) = decode_file_infos(rest)?;
    Ok(BepMessage::IndexUpdate { folder, files })
}

fn decode_request(data: &[u8]) -> Result<BepMessage> {
    let (request_id, data) = decode_u64(data)?;
    let (folder, data) = decode_string(data)?;
    let (name, data) = decode_string(data)?;
    let (block_offset, data) = decode_u64(data)?;
    let (block_size, data) = decode_u32(data)?;
    let (hash_bytes, _) = decode_opaque(data)?;
    if hash_bytes.len() != 32 {
        anyhow::bail!("block hash must be 32 bytes, got {}", hash_bytes.len());
    }
    let mut block_hash = [0u8; 32];
    block_hash.copy_from_slice(hash_bytes);
    Ok(BepMessage::Request {
        request_id,
        folder,
        name,
        block_offset,
        block_size,
        block_hash,
    })
}

fn decode_response(data: &[u8]) -> Result<BepMessage> {
    let (request_id, data) = decode_u64(data)?;
    let (raw, _) = decode_opaque(data)?;
    Ok(BepMessage::Response {
        request_id,
        data: raw.to_vec(),
    })
}

fn decode_close(data: &[u8]) -> Result<BepMessage> {
    let (reason, _) = decode_string(data)?;
    Ok(BepMessage::Close { reason })
}

fn decode_file_infos(data: &[u8]) -> Result<(Vec<FileInfo>, &[u8])> {
    let (count, mut data) = decode_u32(data)?;
    let mut files = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (name, rest) = decode_string(data)?;
        let (file_type, rest) = decode_u32(rest)?;
        let (size, rest) = decode_u64(rest)?;
        let (modified, rest) = decode_i64(rest)?;
        let (sequence, rest) = decode_u64(rest)?;
        let (block_size, rest) = decode_u32(rest)?;
        let (deleted_flag, rest) = decode_u32(rest)?;
        let (invalid_flag, rest) = decode_u32(rest)?;
        let (no_permissions_flag, rest) = decode_u32(rest)?;
        let (version, rest) = decode_version(rest)?;
        let (hash_count, mut rest) = decode_u32(rest)?;
        let mut block_hashes = Vec::with_capacity(hash_count as usize);
        for _ in 0..hash_count {
            let (hash_bytes, remaining) = decode_opaque(rest)?;
            if hash_bytes.len() != 32 {
                anyhow::bail!("block hash must be 32 bytes, got {}", hash_bytes.len());
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(hash_bytes);
            block_hashes.push(hash);
            rest = remaining;
        }
        files.push(FileInfo {
            name,
            file_type,
            size,
            modified,
            sequence,
            block_size,
            deleted: deleted_flag != 0,
            invalid: invalid_flag != 0,
            no_permissions: no_permissions_flag != 0,
            version,
            block_hashes,
        });
        data = rest;
    }
    Ok((files, data))
}

fn decode_version(data: &[u8]) -> Result<(Version, &[u8])> {
    let (count, mut rest) = decode_u32(data)?;
    let mut counters = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (id, after_id) = decode_u64(rest)?;
        let (ctr, after_ctr) = decode_u64(after_id)?;
        counters.push((id, ctr));
        rest = after_ctr;
    }
    Ok((Version { counters }, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: BepMessage) {
        let encoded = encode_message(&msg).unwrap();
        let decoded = decode_message(&encoded).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_decode_cluster_config() {
        round_trip(BepMessage::ClusterConfig {
            folders: vec![
                Folder {
                    id: "folder-1".into(),
                    label: "Documents".into(),
                },
                Folder {
                    id: "folder-2".into(),
                    label: "Photos".into(),
                },
            ],
        });
    }

    #[test]
    fn encode_decode_cluster_config_empty() {
        round_trip(BepMessage::ClusterConfig { folders: vec![] });
    }

    #[test]
    fn encode_decode_index() {
        round_trip(BepMessage::Index {
            folder: "folder-1".into(),
            files: vec![FileInfo {
                name: "test.txt".into(),
                file_type: 0,
                size: 1024,
                modified: 1700000000,
                sequence: 0,
                block_size: 128 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version::default(),
                block_hashes: vec![[0xAB; 32]],
            }],
        });
    }

    #[test]
    fn encode_decode_index_multiple_files() {
        round_trip(BepMessage::Index {
            folder: "docs".into(),
            files: vec![
                FileInfo {
                    name: "a.txt".into(),
                    file_type: 0,
                    size: 500,
                    modified: 100,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version::default(),
                    block_hashes: vec![[1u8; 32]],
                },
                FileInfo {
                    name: "b.txt".into(),
                    file_type: 0,
                    size: 200000,
                    modified: 200,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version::default(),
                    block_hashes: vec![[2u8; 32], [3u8; 32]],
                },
            ],
        });
    }

    #[test]
    fn encode_decode_index_update() {
        round_trip(BepMessage::IndexUpdate {
            folder: "folder-1".into(),
            files: vec![FileInfo {
                name: "updated.bin".into(),
                file_type: 0,
                size: 999999,
                modified: 1700000001,
                sequence: 0,
                block_size: 512 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version::default(),
                block_hashes: vec![[0xFF; 32], [0xEE; 32]],
            }],
        });
    }

    #[test]
    fn encode_decode_index_tombstone_round_trip() {
        round_trip(BepMessage::IndexUpdate {
            folder: "folder-1".into(),
            files: vec![FileInfo {
                name: "gone.txt".into(),
                file_type: 0,
                size: 0,
                modified: 1700000002,
                sequence: 0,
                block_size: 128 * 1024,
                deleted: true,
                invalid: false,
                no_permissions: false,
                version: Version::default(),
                block_hashes: vec![],
            }],
        });
    }

    #[test]
    fn encode_decode_index_negative_modified() {
        round_trip(BepMessage::IndexUpdate {
            folder: "folder-1".into(),
            files: vec![FileInfo {
                name: "ancient.txt".into(),
                file_type: 0,
                size: 42,
                modified: -1_000_000,
                sequence: 0,
                block_size: 128 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version::default(),
                block_hashes: vec![[0x77; 32]],
            }],
        });
    }

    #[test]
    fn encode_decode_index_with_version_vector() {
        round_trip(BepMessage::Index {
            folder: "folder-1".into(),
            files: vec![FileInfo {
                name: "doc.txt".into(),
                file_type: 0,
                size: 99,
                modified: 1_700_000_000,
                sequence: 0,
                block_size: 128 * 1024,
                deleted: false,
                invalid: false,
                no_permissions: false,
                version: Version {
                    counters: vec![(7, 3), (42, 1), (1024, 9)],
                },
                block_hashes: vec![[0x11; 32]],
            }],
        });
    }

    #[test]
    fn encode_decode_request() {
        round_trip(BepMessage::Request {
            request_id: 42,
            folder: "folder-1".into(),
            name: "bigfile.iso".into(),
            block_offset: 524288,
            block_size: 524288,
            block_hash: [0x42; 32],
        });
    }

    #[test]
    fn encode_decode_response() {
        round_trip(BepMessage::Response {
            request_id: 42,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        });
    }

    #[test]
    fn encode_decode_response_empty() {
        round_trip(BepMessage::Response {
            request_id: 1,
            data: vec![],
        });
    }

    #[test]
    fn encode_decode_ping() {
        round_trip(BepMessage::Ping);
    }

    #[test]
    fn encode_decode_close() {
        round_trip(BepMessage::Close {
            reason: "shutdown".into(),
        });
    }

    #[test]
    fn decode_invalid_frame_length() {
        let result = decode_message(&[0, 0]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_unknown_message_type() {
        let mut frame = Vec::new();
        // Body: msg type 99 (unknown), no further data.
        let mut body = Vec::new();
        encode_u32(&mut body, 99);
        let body_len = u32::try_from(body.len()).unwrap_or(0);
        encode_u32(&mut frame, body_len);
        frame.extend_from_slice(&body);

        let result = decode_message(&frame);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown message type")
        );
    }

    #[test]
    fn decode_truncated_body() {
        let mut frame = Vec::new();
        encode_u32(&mut frame, 100); // claim 100 bytes of body
        frame.extend_from_slice(&[0, 0, 0, 1]); // only 4 bytes present

        let result = decode_message(&frame);
        assert!(result.is_err());
    }

    // ── Version vector semantics ──

    #[test]
    fn version_dominates_self_is_false_for_equal() {
        let a = Version {
            counters: vec![(1, 2), (2, 3)],
        };
        let b = a.clone();
        assert!(!a.dominates(&b), "equal vectors do not dominate");
        assert!(!b.dominates(&a));
        assert_eq!(a, b);
    }

    #[test]
    fn version_dominates_strictly_greater() {
        let a = Version {
            counters: vec![(1, 5)],
        };
        let b = Version {
            counters: vec![(1, 2)],
        };
        assert!(a.dominates(&b));
        assert!(!b.dominates(&a));
    }

    #[test]
    fn version_dominates_with_extra_device() {
        // a has an entry b does not — a covers strictly more history.
        let a = Version {
            counters: vec![(1, 1), (2, 4)],
        };
        let b = Version {
            counters: vec![(2, 4)],
        };
        assert!(a.dominates(&b));
        assert!(!b.dominates(&a));
    }

    #[test]
    fn version_dominates_concurrent_returns_false_both_ways() {
        // Device 1 advanced only in a; device 2 advanced only in b.
        let a = Version {
            counters: vec![(1, 1)],
        };
        let b = Version {
            counters: vec![(2, 1)],
        };
        assert!(!a.dominates(&b));
        assert!(!b.dominates(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn version_merge_takes_per_device_max() {
        let mut a = Version {
            counters: vec![(1, 5), (2, 3)],
        };
        let b = Version {
            counters: vec![(1, 2), (3, 9)],
        };
        a.merge(&b);
        assert_eq!(a.counters, vec![(1, 5), (2, 3), (3, 9)]);
    }

    #[test]
    fn version_bump_appends_or_increments() {
        let mut v = Version::default();
        v.bump(42);
        assert_eq!(v.counters, vec![(42, 1)]);
        v.bump(42);
        assert_eq!(v.counters, vec![(42, 2)]);
        v.bump(7);
        // Sorted ascending by device id.
        assert_eq!(v.counters, vec![(7, 1), (42, 2)]);
    }

    #[test]
    fn version_bump_keeps_counters_sorted() {
        let mut v = Version::default();
        v.bump(100);
        v.bump(5);
        v.bump(50);
        let ids: Vec<u64> = v.counters.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![5, 50, 100]);
    }
}
