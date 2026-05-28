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
    /// Block size used for this file.
    pub block_size: u32,
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
        folder: String,
        name: String,
        block_offset: u64,
        block_size: u32,
        block_hash: [u8; 32],
    },
    /// Send block data.
    Response { data: Vec<u8> },
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
            folder,
            name,
            block_offset,
            block_size,
            block_hash,
        } => {
            encode_string(&mut body, folder)?;
            encode_string(&mut body, name)?;
            encode_u64(&mut body, *block_offset);
            encode_u32(&mut body, *block_size);
            encode_opaque(&mut body, block_hash)?;
        }
        BepMessage::Response { data } => {
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
        encode_u64(
            buf,
            u64::try_from(fi.modified)
                .map_err(|_| anyhow::anyhow!("file modified timestamp is negative"))?,
        );
        encode_u32(buf, fi.block_size);
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
        folder,
        name,
        block_offset,
        block_size,
        block_hash,
    })
}

fn decode_response(data: &[u8]) -> Result<BepMessage> {
    let (raw, _) = decode_opaque(data)?;
    Ok(BepMessage::Response { data: raw.to_vec() })
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
        let (modified, rest) = decode_u64(rest)?;
        let (block_size, rest) = decode_u32(rest)?;
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
            modified: i64::try_from(modified)
                .map_err(|_| anyhow::anyhow!("modified timestamp overflows i64"))?,
            block_size,
            block_hashes,
        });
        data = rest;
    }
    Ok((files, data))
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
                block_size: 128 * 1024,
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
                    block_size: 128 * 1024,
                    block_hashes: vec![[1u8; 32]],
                },
                FileInfo {
                    name: "b.txt".into(),
                    file_type: 0,
                    size: 200000,
                    modified: 200,
                    block_size: 128 * 1024,
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
                block_size: 512 * 1024,
                block_hashes: vec![[0xFF; 32], [0xEE; 32]],
            }],
        });
    }

    #[test]
    fn encode_decode_request() {
        round_trip(BepMessage::Request {
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
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        });
    }

    #[test]
    fn encode_decode_response_empty() {
        round_trip(BepMessage::Response { data: vec![] });
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
}
