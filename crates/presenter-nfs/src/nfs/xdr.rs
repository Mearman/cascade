//! XDR codec for NFS data structures (RFC 1813).
//!
//! Encodes and decodes NFSv3 wire format:
//! - Primitive types: uint32, uint64, int32, bool, opaque<>, string<>
//! - NFS-specific types: file handles (nfs_fh3), file attributes (fattr3)
//! - Fixed and variable-length arrays

use std::io::{self, Read, Write};

// ── NFS constants ──

/// NFS file handle size (64 bytes).
pub const NFS3_FHSIZE: usize = 64;

/// Maximum file name length.
pub const NFS3_MAXNAMLEN: usize = 255;

/// Maximum path length.
pub const NFS3_MAXPATHLEN: usize = 4096;

// ── NFS status codes ──

pub const NFS3_OK: u32 = 0;
pub const NFS3ERR_PERM: u32 = 1;
pub const NFS3ERR_NOENT: u32 = 2;
pub const NFS3ERR_IO: u32 = 5;
pub const NFS3ERR_ACCES: u32 = 13;
pub const NFS3ERR_EXIST: u32 = 17;
pub const NFS3ERR_NOTDIR: u32 = 20;
pub const NFS3ERR_ISDIR: u32 = 21;
pub const NFS3ERR_INVAL: u32 = 22;
pub const NFS3ERR_NOSPC: u32 = 28;
pub const NFS3ERR_ROFS: u32 = 30;
pub const NFS3ERR_NAMETOOLONG: u32 = 63;
pub const NFS3ERR_NOTEMPTY: u32 = 66;
pub const NFS3ERR_STALE: u32 = 70;

// ── File types ──

pub const NF3REG: u32 = 1; // regular file
pub const NF3DIR: u32 = 2; // directory
pub const NF3BLK: u32 = 3; // block special
pub const NF3CHR: u32 = 4; // character special
pub const NF3LNK: u32 = 5; // symbolic link
pub const NF3SOCK: u32 = 6; // AF_UNIX socket
pub const NF3FIFO: u32 = 7; // named pipe

// ── Procedure numbers ──

pub const NFS3PROC_NULL: u32 = 0;
pub const NFS3PROC_GETATTR: u32 = 1;
pub const NFS3PROC_SETATTR: u32 = 2;
pub const NFS3PROC_LOOKUP: u32 = 3;
pub const NFS3PROC_ACCESS: u32 = 4;
pub const NFS3PROC_READLINK: u32 = 5;
pub const NFS3PROC_READ: u32 = 6;
pub const NFS3PROC_WRITE: u32 = 7;
pub const NFS3PROC_CREATE: u32 = 8;
pub const NFS3PROC_MKDIR: u32 = 9;
pub const NFS3PROC_SYMLINK: u32 = 10;
pub const NFS3PROC_MKNOD: u32 = 11;
pub const NFS3PROC_REMOVE: u32 = 12;
pub const NFS3PROC_RMDIR: u32 = 13;
pub const NFS3PROC_RENAME: u32 = 14;
pub const NFS3PROC_LINK: u32 = 15;
pub const NFS3PROC_READDIR: u32 = 16;
pub const NFS3PROC_READDIRPLUS: u32 = 17;
pub const NFS3PROC_FSSTAT: u32 = 18;
pub const NFS3PROC_FSINFO: u32 = 19;
pub const NFS3PROC_PATHCONF: u32 = 20;
pub const NFS3PROC_COMMIT: u32 = 21;

// ── Mount protocol constants ──

pub const MOUNTPROC_NULL: u32 = 0;
pub const MOUNTPROC_MNT: u32 = 1;
pub const MOUNTPROC_DUMP: u32 = 2;
pub const MOUNTPROC_UMNT: u32 = 3;
pub const MOUNTPROC_UMNTALL: u32 = 4;
pub const MOUNTPROC_EXPORT: u32 = 5;

pub const MNTPROC_OK: u32 = 0;
pub const MNT3ERR_NOENT: u32 = 1;
pub const MNT3ERR_ACCES: u32 = 13;
pub const MNT3ERR_NOTDIR: u32 = 20;

/// NFS RPC program number.
pub const NFS_PROGRAM: u32 = 100003;
/// NFS RPC program version.
pub const NFS_V3: u32 = 3;

/// Mount RPC program number.
pub const MOUNT_PROGRAM: u32 = 100005;
/// Mount RPC program version.
pub const MOUNT_V3: u32 = 3;

// ── RPC constants ──

pub const RPC_MSG_CALL: u32 = 0;
pub const RPC_MSG_REPLY: u32 = 1;
pub const RPC_REPLY_ACCEPTED: u32 = 0;
pub const RPC_REPLY_DENIED: u32 = 1;
pub const RPC_ACCEPT_SUCCESS: u32 = 0;
pub const RPC_AUTH_NONE: u32 = 0;

// ── XDR types ──

/// NFS file handle — fixed 64-byte opaque.
#[derive(Debug, Clone)]
pub struct NfsFh3 {
    pub data: [u8; NFS3_FHSIZE],
}

impl NfsFh3 {
    /// Create a file handle from an ItemId string, padding with zeros.
    pub fn from_item_id(id: &str) -> Self {
        let mut data = [0u8; NFS3_FHSIZE];
        let bytes = id.as_bytes();
        let len = bytes.len().min(NFS3_FHSIZE);
        data[..len].copy_from_slice(&bytes[..len]);
        Self { data }
    }

    /// Extract the ItemId string from the file handle (up to first null byte).
    pub fn to_item_id(&self) -> Option<String> {
        let end = self.data.iter().position(|&b| b == 0).unwrap_or(NFS3_FHSIZE);
        if end == 0 {
            return None;
        }
        String::from_utf8(self.data[..end].to_vec()).ok()
    }
}

/// File attributes (fattr3).
#[derive(Debug, Clone)]
pub struct Fattr3 {
    pub ftype: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub used: u64,
    pub rdev: Specdata3,
    pub fsid: u64,
    pub fileid: u64,
    pub atime: NfsTime,
    pub mtime: NfsTime,
    pub ctime: NfsTime,
}

impl Default for Fattr3 {
    fn default() -> Self {
        Self {
            ftype: NF3REG,
            mode: 0o644,
            nlink: 1,
            uid: 501,
            gid: 20,
            size: 0,
            used: 0,
            rdev: Specdata3::default(),
            fsid: 0,
            fileid: 0,
            atime: NfsTime::epoch(),
            mtime: NfsTime::epoch(),
            ctime: NfsTime::epoch(),
        }
    }
}

/// Special device data.
#[derive(Debug, Clone)]
pub struct Specdata3 {
    pub specdata1: u32,
    pub specdata2: u32,
}

impl Default for Specdata3 {
    fn default() -> Self {
        Self {
            specdata1: 0,
            specdata2: 0,
        }
    }
}

/// NFS time (seconds + nanoseconds).
#[derive(Debug, Clone)]
pub struct NfsTime {
    pub seconds: u32,
    pub nseconds: u32,
}

impl NfsTime {
    pub fn epoch() -> Self {
        Self {
            seconds: 0,
            nseconds: 0,
        }
    }

    pub fn from_epoch(secs: i64) -> Self {
        Self {
            seconds: secs as u32,
            nseconds: 0,
        }
    }
}

/// Post-operation attributes — either present or not.
#[derive(Debug, Clone)]
pub struct PostOpAttr {
    pub attributes_follow: bool,
    pub attributes: Option<Fattr3>,
}

impl PostOpAttr {
    pub fn some(attr: Fattr3) -> Self {
        Self {
            attributes_follow: true,
            attributes: Some(attr),
        }
    }

    pub fn none() -> Self {
        Self {
            attributes_follow: false,
            attributes: None,
        }
    }
}

// ── XDR encoding ──

/// Encode a uint32 in XDR format (big-endian).
pub fn encode_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Encode a uint64 in XDR format (big-endian).
pub fn encode_u64(buf: &mut Vec<u8>, val: u64) {
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Encode a bool in XDR format.
pub fn encode_bool(buf: &mut Vec<u8>, val: bool) {
    encode_u32(buf, if val { 1 } else { 0 });
}

/// Encode a variable-length opaque in XDR format (length + padded data).
pub fn encode_opaque(buf: &mut Vec<u8>, data: &[u8]) {
    encode_u32(buf, data.len() as u32);
    buf.extend_from_slice(data);
    // Pad to 4-byte boundary.
    let pad = (4 - (data.len() % 4)) % 4;
    buf.extend(std::iter::repeat(0u8).take(pad));
}

/// Encode a variable-length string in XDR format.
pub fn encode_string(buf: &mut Vec<u8>, s: &str) {
    encode_opaque(buf, s.as_bytes());
}

/// Encode a fixed opaque (like a file handle).
pub fn encode_fixed_opaque(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(data);
    let pad = (4 - (data.len() % 4)) % 4;
    buf.extend(std::iter::repeat(0u8).take(pad));
}

/// Encode an NFS file handle.
pub fn encode_fh(buf: &mut Vec<u8>, fh: &NfsFh3) {
    encode_fixed_opaque(buf, &fh.data);
}

/// Encode fattr3.
pub fn encode_fattr3(buf: &mut Vec<u8>, attr: &Fattr3) {
    encode_u32(buf, attr.ftype);
    encode_u32(buf, attr.mode);
    encode_u32(buf, attr.nlink);
    encode_u32(buf, attr.uid);
    encode_u32(buf, attr.gid);
    encode_u64(buf, attr.size);
    encode_u64(buf, attr.used);
    encode_u32(buf, attr.rdev.specdata1);
    encode_u32(buf, attr.rdev.specdata2);
    encode_u64(buf, attr.fsid);
    encode_u64(buf, attr.fileid);
    encode_u32(buf, attr.atime.seconds);
    encode_u32(buf, attr.atime.nseconds);
    encode_u32(buf, attr.mtime.seconds);
    encode_u32(buf, attr.mtime.nseconds);
    encode_u32(buf, attr.ctime.seconds);
    encode_u32(buf, attr.ctime.nseconds);
}

/// Encode post-op-attr.
pub fn encode_post_op_attr(buf: &mut Vec<u8>, poa: &PostOpAttr) {
    encode_bool(buf, poa.attributes_follow);
    if let Some(ref attr) = poa.attributes {
        encode_fattr3(buf, attr);
    }
}

// ── XDR decoding ──

/// Decode a uint32 from XDR data.
pub fn decode_u32(data: &[u8]) -> io::Result<(u32, &[u8])> {
    if data.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "need 4 bytes for u32"));
    }
    let val = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    Ok((val, &data[4..]))
}

/// Decode a uint64 from XDR data.
pub fn decode_u64(data: &[u8]) -> io::Result<(u64, &[u8])> {
    if data.len() < 8 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "need 8 bytes for u64"));
    }
    let val = u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    Ok((val, &data[8..]))
}

/// Decode a bool from XDR data.
pub fn decode_bool(data: &[u8]) -> io::Result<(bool, &[u8])> {
    let (val, rest) = decode_u32(data)?;
    Ok((val != 0, rest))
}

/// Decode a variable-length opaque from XDR data.
pub fn decode_opaque(data: &[u8]) -> io::Result<(&[u8], &[u8])> {
    let (len, rest) = decode_u32(data)?;
    let len = len as usize;
    if rest.len() < len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "opaque data truncated"));
    }
    let (opaque_data, remainder) = rest.split_at(len);
    // Skip padding.
    let pad = (4 - (len % 4)) % 4;
    let remainder = if remainder.len() >= pad {
        &remainder[pad..]
    } else {
        &[]
    };
    Ok((opaque_data, remainder))
}

/// Decode a variable-length string from XDR data.
pub fn decode_string(data: &[u8]) -> io::Result<(String, &[u8])> {
    let (bytes, rest) = decode_opaque(data)?;
    let s = String::from_utf8(bytes.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((s, rest))
}

/// Decode an NFS file handle from XDR data.
pub fn decode_fh(data: &[u8]) -> io::Result<(NfsFh3, &[u8])> {
    if data.len() < NFS3_FHSIZE {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file handle truncated",
        ));
    }
    let mut fh = NfsFh3 {
        data: [0u8; NFS3_FHSIZE],
    };
    fh.data.copy_from_slice(&data[..NFS3_FHSIZE]);
    Ok((fh, &data[NFS3_FHSIZE..]))
}

/// Read a complete RPC message (length-prefixed) from a reader.
pub fn read_rpc_message(reader: &mut dyn Read) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1_048_576 {
        // Cap at 1MB to prevent OOM.
        return Err(io::Error::new(io::ErrorKind::InvalidData, "RPC message too large"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a complete RPC message (length-prefixed) to a writer.
pub fn write_rpc_message(writer: &mut dyn Write, msg: &[u8]) -> io::Result<()> {
    writer.write_all(&(msg.len() as u32).to_be_bytes())?;
    writer.write_all(msg)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_u32() {
        let mut buf = Vec::new();
        encode_u32(&mut buf, 0xDEADBEEF);
        let (val, rest) = decode_u32(&buf).unwrap();
        assert_eq!(val, 0xDEADBEEF);
        assert!(rest.is_empty());
    }

    #[test]
    fn encode_decode_u64() {
        let mut buf = Vec::new();
        encode_u64(&mut buf, 0x0102030405060708);
        let (val, rest) = decode_u64(&buf).unwrap();
        assert_eq!(val, 0x0102030405060708);
        assert!(rest.is_empty());
    }

    #[test]
    fn encode_decode_bool() {
        let mut buf = Vec::new();
        encode_bool(&mut buf, true);
        encode_bool(&mut buf, false);
        let (v1, rest) = decode_bool(&buf).unwrap();
        assert!(v1);
        let (v2, rest) = decode_bool(rest).unwrap();
        assert!(!v2);
        assert!(rest.is_empty());
    }

    #[test]
    fn encode_decode_opaque() {
        let mut buf = Vec::new();
        encode_opaque(&mut buf, b"hello");
        let (data, rest) = decode_opaque(&buf).unwrap();
        assert_eq!(data, b"hello");
        assert!(rest.is_empty());
    }

    #[test]
    fn encode_decode_string() {
        let mut buf = Vec::new();
        encode_string(&mut buf, "Documents/report.txt");
        let (s, rest) = decode_string(&buf).unwrap();
        assert_eq!(s, "Documents/report.txt");
        assert!(rest.is_empty());
    }

    #[test]
    fn file_handle_round_trip() {
        let fh = NfsFh3::from_item_id("gdrive:abc123");
        let id = fh.to_item_id().unwrap();
        assert_eq!(id, "gdrive:abc123");
    }

    #[test]
    fn file_handle_xdr_round_trip() {
        let fh = NfsFh3::from_item_id("gdrive:root");
        let mut buf = Vec::new();
        encode_fh(&mut buf, &fh);
        let (decoded, rest) = decode_fh(&buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded.to_item_id().unwrap(), "gdrive:root");
    }

    #[test]
    fn rpc_message_round_trip() {
        let msg = b"test RPC payload";
        let mut written = Vec::new();
        write_rpc_message(&mut written, msg).unwrap();

        let mut cursor = std::io::Cursor::new(&written);
        let read = read_rpc_message(&mut cursor).unwrap();
        assert_eq!(&read, msg);
    }
}
