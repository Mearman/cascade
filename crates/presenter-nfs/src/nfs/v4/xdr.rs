//! XDR encoding/decoding for `NFSv4` wire types (RFC 5661 subset).
//!
//! `NFSv4` uses a different XDR layout from `NFSv3`:
//! - File handles are variable-length opaque (`opaque<>`), not fixed 64 bytes
//! - State IDs are 16-byte fixed opaque
//! - COMPOUND replaces individual procedure calls
//! - Attributes use a bitmap-based approach (fattr4)

use std::io;

// ── `NFSv4` status codes (RFC 5661 §18) ──

pub const NFS4_OK: u32 = 0;
pub const NFS4ERR_PERM: u32 = 1;
pub const NFS4ERR_NOENT: u32 = 2;
pub const NFS4ERR_IO: u32 = 5;
pub const NFS4ERR_ACCES: u32 = 13;
pub const NFS4ERR_EXIST: u32 = 17;
pub const NFS4ERR_INVAL: u32 = 22;
pub const NFS4ERR_ISDIR: u32 = 21;
pub const NFS4ERR_NOTDIR: u32 = 20;
pub const NFS4ERR_NOSPC: u32 = 28;
pub const NFS4ERR_ROFS: u32 = 30;
pub const NFS4ERR_NAMETOOLONG: u32 = 63;
pub const NFS4ERR_NOTEMPTY: u32 = 66;
pub const NFS4ERR_STALE: u32 = 70;
pub const NFS4ERR_BADHANDLE: u32 = 10001;
pub const NFS4ERR_NOTSUPP: u32 = 10004;
pub const NFS4ERR_BAD_STATEID: u32 = 10026;
pub const NFS4ERR_GRACE: u32 = 10035;

// ── `NFSv4` procedure numbers ──

/// `NFSv4` COMPOUND is procedure 3 (the only procedure in v4.0 besides NULL).
pub const NFSPROC4_NULL: u32 = 0;
pub const NFSPROC4_COMPOUND: u32 = 1;

// ── `NFSv4` operation numbers ──

pub const OP_ACCESS: u32 = 3;
pub const OP_CLOSE: u32 = 4;
pub const OP_COMMIT: u32 = 5;
pub const OP_CREATE: u32 = 6;
pub const OP_DELEGPURGE: u32 = 7;
pub const OP_DELEGRETURN: u32 = 8;
pub const OP_GETATTR: u32 = 9;
pub const OP_GETFH: u32 = 10;
pub const OP_LINK: u32 = 11;
pub const OP_LOCK: u32 = 12;
pub const OP_LOCKT: u32 = 13;
pub const OP_LOCKU: u32 = 14;
pub const OP_LOOKUP: u32 = 15;
pub const OP_LOOKUPP: u32 = 16;
pub const OP_NVERIFY: u32 = 17;
pub const OP_OPEN: u32 = 18;
pub const OP_OPENATTR: u32 = 19;
pub const OP_OPEN_CONFIRM: u32 = 20;
pub const OP_OPEN_DOWNGRADE: u32 = 21;
pub const OP_PUTFH: u32 = 22;
pub const OP_PUTPUBFH: u32 = 23;
pub const OP_PUTROOTFH: u32 = 24;
pub const OP_READ: u32 = 25;
pub const OP_READDIR: u32 = 26;
pub const OP_READLINK: u32 = 27;
pub const OP_REMOVE: u32 = 28;
pub const OP_RENAME: u32 = 29;
pub const OP_RENEW: u32 = 30;
pub const OP_RESTOREFH: u32 = 31;
pub const OP_SAVEFH: u32 = 32;
pub const OP_SECINFO: u32 = 33;
pub const OP_SETATTR: u32 = 34;
pub const OP_SETCLIENTID: u32 = 35;
pub const OP_SETCLIENTID_CONFIRM: u32 = 36;
pub const OP_VERIFY: u32 = 37;
pub const OP_WRITE: u32 = 38;

// ── `NFSv4` file types ──

pub const NF4REG: u32 = 1; // regular file
pub const NF4DIR: u32 = 2; // directory
pub const NF4BLK: u32 = 3; // block special
pub const NF4CHR: u32 = 4; // character special
pub const NF4LNK: u32 = 5; // symbolic link
pub const NF4SOCK: u32 = 6; // socket
pub const NF4FIFO: u32 = 7; // named pipe
pub const NF4ATTRDIR: u32 = 8; // attribute directory
pub const NF4NAMEDATTR: u32 = 9; // named attribute

// ── Attribute bitmap IDs (FATTR4) ──

pub const FATTR4_TYPE: u32 = 1;
pub const FATTR4_SIZE: u32 = 4;
pub const FATTR4_FILEID: u32 = 8;
pub const FATTR4_MODE: u32 = 33;
pub const FATTR4_NUMLINKS: u32 = 11;
pub const FATTR4_OWNER: u32 = 9;
pub const FATTR4_OWNER_GROUP: u32 = 10;
pub const FATTR4_SPACE_USED: u32 = 45;
pub const FATTR4_TIME_ACCESS: u32 = 47;
pub const FATTR4_TIME_MODIFY: u32 = 53;
pub const FATTR4_TIME_CREATE: u32 = 52;
pub const FATTR4_FSID: u32 = 34;
pub const FATTR4_MAXREAD: u32 = 14;
pub const FATTR4_MAXWRITE: u32 = 15;
pub const FATTR4_FS_LAYOUT_TYPES: u32 = 62;
pub const FATTR4_CHANGE: u32 = 5;
pub const FATTR4_SUPPORTED_ATTRS: u32 = 0;

// ── ACCESS flags ──

pub const ACCESS4_READ: u32 = 0x0000_0001;
pub const ACCESS4_LOOKUP: u32 = 0x0000_0002;
pub const ACCESS4_MODIFY: u32 = 0x0000_0004;
pub const ACCESS4_EXTEND: u32 = 0x0000_0008;
pub const ACCESS4_DELETE: u32 = 0x0000_0010;
pub const ACCESS4_EXECUTE: u32 = 0x0000_0020;

// ── OPEN flags ──

pub const OPEN4_SHARE_ACCESS_READ: u32 = 1;
pub const OPEN4_SHARE_ACCESS_WRITE: u32 = 2;
pub const OPEN4_SHARE_ACCESS_BOTH: u32 = 3;

pub const OPEN4_CREATE: u32 = 1;
pub const OPEN4_NOCREATE: u32 = 0;

// ── CREATE types ──

pub const NF4REG_CREATE: u32 = 0; // UNCHECKED4
pub const NF4REG_GUARDED: u32 = 1; // GUARDED4
pub const NF4REG_EXCLUSIVE: u32 = 2; // EXCLUSIVE4

// ── WRITE stable_how4 (RFC 7530 §16.36) ──

/// Data and metadata committed to stable storage before the WRITE reply.
pub const FILE_SYNC4: u32 = 2;

// ── `NFSv4` RPC constants ──

/// `NFSv4` program number (same as v3).
pub const NFS4_PROGRAM: u32 = 100_003;
/// `NFSv4` program version.
pub const NFS_V4: u32 = 4;

/// State ID size in bytes.
pub const STATEID_SIZE: usize = 16;

// ── `NFSv4` file handle ──

/// `NFSv4` file handle — variable-length opaque.
#[derive(Debug, Clone)]
pub struct NfsFh4 {
    pub data: Vec<u8>,
}

impl NfsFh4 {
    /// Create a file handle from a path string.
    #[must_use]
    pub fn from_path(path: &str) -> Self {
        Self {
            data: path.as_bytes().to_vec(),
        }
    }

    /// Extract the path string from the file handle.
    #[must_use]
    pub fn to_path(&self) -> Option<String> {
        String::from_utf8(self.data.clone()).ok()
    }

    /// Create the root file handle.
    #[must_use]
    pub fn root() -> Self {
        Self::from_path("/")
    }

    /// Check if this is the root file handle.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.data == b"/"
    }
}

impl PartialEq for NfsFh4 {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}
impl Eq for NfsFh4 {}

/// `NFSv4` state ID — 16-byte fixed opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateId {
    pub data: [u8; STATEID_SIZE],
}

impl StateId {
    /// Create an all-zeros state ID (representing no open state).
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            data: [0u8; STATEID_SIZE],
        }
    }

    /// Create a state ID with a simple sequential counter.
    #[must_use]
    pub fn from_counter(seq: u32) -> Self {
        let mut data = [0u8; STATEID_SIZE];
        data[0..4].copy_from_slice(&seq.to_be_bytes());
        Self { data }
    }
}

/// `NFSv4` file attributes (fattr4) — bitmap-based.
#[derive(Debug, Clone)]
pub struct Fattr4 {
    pub ftype: u32,
    pub size: u64,
    pub fileid: u64,
    pub mode: u32,
    pub numlinks: u32,
    pub owner: String,
    pub owner_group: String,
    pub space_used: u64,
    pub time_access: NfsTime4,
    pub time_modify: NfsTime4,
    pub time_create: NfsTime4,
    pub fsid: u64,
    pub change: u64,
}

impl Default for Fattr4 {
    fn default() -> Self {
        Self {
            ftype: NF4REG,
            size: 0,
            fileid: 0,
            mode: 0o644,
            numlinks: 1,
            owner: "nobody".to_string(),
            owner_group: "nogroup".to_string(),
            space_used: 0,
            time_access: NfsTime4::epoch(),
            time_modify: NfsTime4::epoch(),
            time_create: NfsTime4::epoch(),
            fsid: 0,
            change: 0,
        }
    }
}

/// `NFSv4` time (seconds + nanoseconds as a pair of uint64 + uint32).
#[derive(Debug, Clone, Copy)]
pub struct NfsTime4 {
    pub seconds: u64,
    pub nseconds: u32,
}

impl NfsTime4 {
    #[must_use]
    pub const fn epoch() -> Self {
        Self {
            seconds: 0,
            nseconds: 0,
        }
    }
}

// ── XDR encoding for `NFSv4` ──

/// Encode a uint32 (big-endian).
pub fn encode_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Encode a uint64 (big-endian).
pub fn encode_u64(buf: &mut Vec<u8>, val: u64) {
    buf.extend_from_slice(&val.to_be_bytes());
}

/// Encode a bool as uint32.
pub fn encode_bool(buf: &mut Vec<u8>, val: bool) {
    encode_u32(buf, u32::from(val));
}

/// Encode variable-length opaque with 4-byte padding.
pub fn encode_opaque(buf: &mut Vec<u8>, data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    encode_u32(buf, len);
    buf.extend_from_slice(data);
    let pad = (4 - (data.len() % 4)) % 4;
    buf.extend(std::iter::repeat_n(0u8, pad));
}

/// Encode a string as variable-length opaque.
pub fn encode_string(buf: &mut Vec<u8>, s: &str) {
    encode_opaque(buf, s.as_bytes());
}

/// Encode an `NFSv4` file handle (variable-length opaque).
pub fn encode_fh4(buf: &mut Vec<u8>, fh: &NfsFh4) {
    encode_opaque(buf, &fh.data);
}

/// Encode a state ID (16-byte fixed opaque).
pub fn encode_stateid(buf: &mut Vec<u8>, sid: &StateId) {
    buf.extend_from_slice(&sid.data);
}

/// Encode an attribute bitmap. `NFSv4` uses a two-word bitmap where
/// each bit indicates an attribute ID. We build the two u32 words
/// from the set of requested/present attribute IDs.
pub fn encode_attr_bitmap(buf: &mut Vec<u8>, attrs: &[u32]) {
    // Find the maximum attribute ID to determine how many words we need.
    let max_id = attrs.iter().copied().max().unwrap_or(0);
    let word_count = usize::try_from(max_id / 32).unwrap_or(0) + 1;
    let word_count = word_count.min(2); // Cap at 2 words for simplicity.

    let mut words = vec![0u32; word_count];
    for &id in attrs {
        let word_idx = usize::try_from(id / 32).unwrap_or(0);
        let bit = id % 32;
        if word_idx < word_count
            && let Some(word) = words.get_mut(word_idx)
        {
            *word |= 1u32 << bit;
        }
    }

    encode_u32(buf, u32::try_from(word_count).unwrap_or(0));
    for &word in &words {
        encode_u32(buf, word);
    }
}

/// Encode a full fattr4 attribute set behind a bitmap.
/// Encodes only the attributes indicated in the bitmap.
pub fn encode_fattr4(buf: &mut Vec<u8>, bitmap: &[u32], attr: &Fattr4) {
    // Encode the bitmap.
    encode_attr_bitmap(buf, bitmap);

    // Encode each requested attribute value in ID order.
    // The order must match the canonical attribute ID order.
    let mut sorted: Vec<u32> = bitmap.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    // Build a quick lookup set.
    let _requested: std::collections::HashSet<u32> = sorted.iter().copied().collect();

    // Encode values in the order they appear in the bitmap.
    // Each attribute's XDR encoding depends on its type.
    for &id in &sorted {
        match id {
            FATTR4_SUPPORTED_ATTRS => {
                // Return the set of attributes we support.
                let supported = [
                    FATTR4_TYPE,
                    FATTR4_SIZE,
                    FATTR4_FILEID,
                    FATTR4_MODE,
                    FATTR4_NUMLINKS,
                    FATTR4_CHANGE,
                    FATTR4_FSID,
                ];
                encode_attr_bitmap(buf, &supported);
            }
            FATTR4_TYPE => encode_u32(buf, attr.ftype),
            FATTR4_SIZE => encode_u64(buf, attr.size),
            FATTR4_FILEID => encode_u64(buf, attr.fileid),
            FATTR4_MODE => encode_u32(buf, attr.mode),
            FATTR4_NUMLINKS => encode_u32(buf, attr.numlinks),
            FATTR4_OWNER => encode_string(buf, &attr.owner),
            FATTR4_OWNER_GROUP => encode_string(buf, &attr.owner_group),
            FATTR4_SPACE_USED => encode_u64(buf, attr.space_used),
            FATTR4_TIME_ACCESS => encode_nfstime4(buf, &attr.time_access),
            FATTR4_TIME_MODIFY => encode_nfstime4(buf, &attr.time_modify),
            FATTR4_TIME_CREATE => encode_nfstime4(buf, &attr.time_create),
            FATTR4_FSID => {
                // fsid is a struct of two uint64.
                encode_u64(buf, attr.fsid);
                encode_u64(buf, 0);
            }
            FATTR4_CHANGE => encode_u64(buf, attr.change),
            FATTR4_MAXREAD | FATTR4_MAXWRITE => encode_u64(buf, 1_048_576),
            _ => {
                // Unknown attribute — skip. The bitmap claimed it but
                // we don't have a value. This is acceptable for attributes
                // we don't actually support.
            }
        }
    }
}

/// Encode an nfstime4 (uint64 seconds + uint32 nseconds).
pub fn encode_nfstime4(buf: &mut Vec<u8>, t: &NfsTime4) {
    encode_u64(buf, t.seconds);
    encode_u32(buf, t.nseconds);
}

// ── XDR decoding for `NFSv4` ──

/// Decode a uint32.
pub fn decode_u32(data: &[u8]) -> io::Result<(u32, &[u8])> {
    if data.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "need 4 bytes for u32",
        ));
    }
    let (word, rest) = data.split_at(4);
    let val = u32::from_be_bytes(
        word.try_into()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    );
    Ok((val, rest))
}

/// Decode a uint64.
pub fn decode_u64(data: &[u8]) -> io::Result<(u64, &[u8])> {
    if data.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "need 8 bytes for u64",
        ));
    }
    let (word, rest) = data.split_at(8);
    let val = u64::from_be_bytes(
        word.try_into()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    );
    Ok((val, rest))
}

/// Decode a bool.
pub fn decode_bool(data: &[u8]) -> io::Result<(bool, &[u8])> {
    let (val, rest) = decode_u32(data)?;
    Ok((val != 0, rest))
}

/// Decode variable-length opaque.
pub fn decode_opaque(data: &[u8]) -> io::Result<(&[u8], &[u8])> {
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
    #[allow(clippy::indexing_slicing)]
    let remainder = if remainder.len() >= pad {
        &remainder[pad..]
    } else {
        &[]
    };
    Ok((opaque_data, remainder))
}

/// Decode a string.
pub fn decode_string(data: &[u8]) -> io::Result<(String, &[u8])> {
    let (bytes, rest) = decode_opaque(data)?;
    let s = String::from_utf8(bytes.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((s, rest))
}

/// Decode an `NFSv4` file handle (variable-length opaque).
pub fn decode_fh4(data: &[u8]) -> io::Result<(NfsFh4, &[u8])> {
    let (bytes, rest) = decode_opaque(data)?;
    Ok((
        NfsFh4 {
            data: bytes.to_vec(),
        },
        rest,
    ))
}

/// Decode a state ID (16-byte fixed opaque).
pub fn decode_stateid(data: &[u8]) -> io::Result<(StateId, &[u8])> {
    if data.len() < STATEID_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stateid truncated",
        ));
    }
    let (sid_bytes, rest) = data.split_at(STATEID_SIZE);
    let mut sid = StateId {
        data: [0u8; STATEID_SIZE],
    };
    sid.data.copy_from_slice(sid_bytes);
    Ok((sid, rest))
}

/// Decode an attribute bitmap.
pub fn decode_attr_bitmap(data: &[u8]) -> io::Result<(Vec<u32>, &[u8])> {
    let (count, mut rest) = decode_u32(data)?;
    let count =
        usize::try_from(count).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut attrs = Vec::new();
    for word_index in 0..count {
        let (word, remaining) = decode_u32(rest)?;
        rest = remaining;
        // Each word covers attribute ids [word_index*32, word_index*32 + 32).
        // The global attribute id is the word offset plus the local bit, so an
        // attribute such as FATTR4_TIME_MODIFY (53) in word 1 bit 21 decodes
        // back to 53, not 21.
        let base = u32::try_from(word_index)
            .unwrap_or(u32::MAX)
            .saturating_mul(32);
        for bit in 0..32u32 {
            if word & (1u32 << bit) != 0 {
                attrs.push(base.saturating_add(bit));
            }
        }
    }
    Ok((attrs, rest))
}

/// Consume the encoded `fattr4` attribute-values blob that follows a SETATTR
/// bitmap, returning the requested `FATTR4_SIZE` (when set) and the slice
/// positioned immediately after the last value.
///
/// SETATTR encodes one value per set bit, in ascending attribute-number order,
/// with no per-attribute framing (this codebase's `encode_fattr4` does not wrap
/// the values in an `opaque<>`). To keep the surrounding COMPOUND framed, every
/// set attribute's value must be consumed by its exact wire length — not just
/// the one Cascade chooses to act on. The size value is read at its correct
/// offset within the blob (after any lower-numbered attributes), and the cursor
/// advances past the whole blob regardless of which attributes were set.
///
/// # Errors
///
/// Returns an error if the blob is truncated, or if it carries an attribute
/// whose wire length is not known here — consuming an unknown-length attribute
/// would desync the rest of the request, so the operation fails loudly instead.
pub fn decode_setattr_values<'a>(
    bitmap: &[u32],
    values: &'a [u8],
) -> io::Result<(Option<u64>, &'a [u8])> {
    let mut ids: Vec<u32> = bitmap.to_vec();
    ids.sort_unstable();
    ids.dedup();

    let mut size: Option<u64> = None;
    let mut rest = values;
    for id in ids {
        match id {
            FATTR4_TYPE | FATTR4_MODE | FATTR4_NUMLINKS => {
                let (_v, r) = decode_u32(rest)?;
                rest = r;
            }
            FATTR4_SIZE => {
                let (v, r) = decode_u64(rest)?;
                size = Some(v);
                rest = r;
            }
            FATTR4_CHANGE | FATTR4_FILEID | FATTR4_SPACE_USED | FATTR4_MAXREAD
            | FATTR4_MAXWRITE => {
                let (_v, r) = decode_u64(rest)?;
                rest = r;
            }
            FATTR4_FSID => {
                // fsid4 is two uint64 (major, minor).
                let (_major, r) = decode_u64(rest)?;
                let (_minor, r) = decode_u64(r)?;
                rest = r;
            }
            FATTR4_OWNER | FATTR4_OWNER_GROUP => {
                let (_v, r) = decode_opaque(rest)?;
                rest = r;
            }
            FATTR4_TIME_ACCESS | FATTR4_TIME_MODIFY | FATTR4_TIME_CREATE => {
                // nfstime4 is uint64 seconds + uint32 nseconds.
                let (_secs, r) = decode_u64(rest)?;
                let (_nsecs, r) = decode_u32(r)?;
                rest = r;
            }
            FATTR4_SUPPORTED_ATTRS => {
                // A bitmap value; consume count + words.
                let (_attrs, r) = decode_attr_bitmap(rest)?;
                rest = r;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("SETATTR carries attribute {other} of unknown wire length"),
                ));
            }
        }
    }
    Ok((size, rest))
}

/// Build a set of default attributes for a path.
#[must_use]
pub fn make_fattr4(path: &str, is_dir: bool) -> Fattr4 {
    make_fattr4_with_size(path, is_dir, 0)
}

/// Build `fattr4` for a path with a known content size.
#[must_use]
pub fn make_fattr4_with_size(path: &str, is_dir: bool, size: u64) -> Fattr4 {
    Fattr4 {
        ftype: if is_dir { NF4DIR } else { NF4REG },
        size,
        fileid: id_hash(path),
        mode: if is_dir { 0o755 } else { 0o644 },
        numlinks: if is_dir { 2 } else { 1 },
        owner: "nobody".to_string(),
        owner_group: "nogroup".to_string(),
        space_used: size,
        time_access: NfsTime4::epoch(),
        time_modify: NfsTime4::epoch(),
        time_create: NfsTime4::epoch(),
        fsid: 0,
        change: id_hash(path),
    }
}

/// Simple hash of a string to a u64 fileid.
fn id_hash(s: &str) -> u64 {
    let mut hash: u64 = 5381;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(byte));
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fh4_round_trip() {
        let fh = NfsFh4::from_path("/Documents/report.txt");
        let mut buf = Vec::new();
        encode_fh4(&mut buf, &fh);
        let (decoded, rest) = decode_fh4(&buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded.to_path().unwrap(), "/Documents/report.txt");
    }

    #[test]
    fn fh4_root() {
        let fh = NfsFh4::root();
        assert!(fh.is_root());
        assert_eq!(fh.to_path().unwrap(), "/");
    }

    #[test]
    fn stateid_round_trip() {
        let sid = StateId::from_counter(42);
        let mut buf = Vec::new();
        encode_stateid(&mut buf, &sid);
        let (decoded, rest) = decode_stateid(&buf).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded.data[0..4], 42u32.to_be_bytes());
    }

    #[test]
    fn stateid_zero() {
        let sid = StateId::zero();
        assert_eq!(sid.data, [0u8; STATEID_SIZE]);
    }

    #[test]
    fn u32_round_trip() {
        let mut buf = Vec::new();
        encode_u32(&mut buf, 0xDEAD_BEEF);
        let (val, rest) = decode_u32(&buf).unwrap();
        assert_eq!(val, 0xDEAD_BEEF);
        assert!(rest.is_empty());
    }

    #[test]
    fn u64_round_trip() {
        let mut buf = Vec::new();
        encode_u64(&mut buf, 0x0102_0304_0506_0708);
        let (val, rest) = decode_u64(&buf).unwrap();
        assert_eq!(val, 0x0102_0304_0506_0708);
        assert!(rest.is_empty());
    }

    #[test]
    fn string_round_trip() {
        let mut buf = Vec::new();
        encode_string(&mut buf, "Documents/report.txt");
        let (s, rest) = decode_string(&buf).unwrap();
        assert_eq!(s, "Documents/report.txt");
        assert!(rest.is_empty());
    }

    #[test]
    fn bool_round_trip() {
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
    fn attr_bitmap_round_trip() {
        let attrs = vec![FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID];
        let mut buf = Vec::new();
        encode_attr_bitmap(&mut buf, &attrs);
        let (decoded, rest) = decode_attr_bitmap(&buf).unwrap();
        assert!(rest.is_empty());
        assert!(decoded.contains(&FATTR4_TYPE));
        assert!(decoded.contains(&FATTR4_SIZE));
        assert!(decoded.contains(&FATTR4_FILEID));
    }

    #[test]
    fn fattr4_encoding_round_trip() {
        let attr = make_fattr4("/test", false);
        let bitmap = vec![FATTR4_TYPE, FATTR4_SIZE, FATTR4_FILEID];
        let mut buf = Vec::new();
        encode_fattr4(&mut buf, &bitmap, &attr);
        // Verify the buffer is non-empty and starts with a bitmap.
        assert!(!buf.is_empty());
        let (decoded_bitmap, _) = decode_attr_bitmap(&buf).unwrap();
        assert!(decoded_bitmap.contains(&FATTR4_TYPE));
        assert!(decoded_bitmap.contains(&FATTR4_SIZE));
    }

    #[test]
    fn make_fattr4_dir() {
        let attr = make_fattr4("/dir", true);
        assert_eq!(attr.ftype, NF4DIR);
        assert_eq!(attr.mode, 0o755);
        assert_eq!(attr.numlinks, 2);
    }

    #[test]
    fn make_fattr4_file() {
        let attr = make_fattr4("/file.txt", false);
        assert_eq!(attr.ftype, NF4REG);
        assert_eq!(attr.mode, 0o644);
        assert_eq!(attr.numlinks, 1);
    }

    #[test]
    fn setattr_values_size_and_mtime() {
        // bitmap {size(4), time_modify(53)} then size(u64) + nfstime4.
        let mut bitmap_buf = Vec::new();
        encode_attr_bitmap(&mut bitmap_buf, &[FATTR4_SIZE, FATTR4_TIME_MODIFY]);
        let (bitmap, _) = decode_attr_bitmap(&bitmap_buf).unwrap();

        let mut values = Vec::new();
        encode_u64(&mut values, 4); // size
        encode_u64(&mut values, 1_700_000_000); // mtime secs
        encode_u32(&mut values, 0); // mtime nsecs
        // A trailing sentinel uint32 representing the next op.
        encode_u32(&mut values, 0xABCD);

        let (size, rest) = decode_setattr_values(&bitmap, &values).unwrap();
        assert_eq!(size, Some(4));
        // Exactly the trailing sentinel remains.
        let (sentinel, tail) = decode_u32(rest).unwrap();
        assert_eq!(sentinel, 0xABCD);
        assert!(tail.is_empty());
    }

    #[test]
    fn setattr_values_unknown_attr_errors() {
        // Attribute id 99 has no known wire length; must fail rather than desync.
        let bitmap = vec![99];
        let values = [0u8; 8];
        assert!(decode_setattr_values(&bitmap, &values).is_err());
    }

    #[test]
    fn decode_truncated_u32() {
        let result = decode_u32(&[0x00, 0x01]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_truncated_opaque() {
        let mut buf = Vec::new();
        encode_u32(&mut buf, 100); // Claims 100 bytes
        let result = decode_opaque(&buf);
        assert!(result.is_err());
    }
}
