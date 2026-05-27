//! XDR codec for NFS data structures (RFC 1813).
//!
//! Handles encoding/decoding of:
//! - Primitive types: uint32, uint64, int32, int64, bool, opaque<>, string<>
//! - NFS-specific types: file handles (nfs_fh3), file attributes (fattr3)
//! - Fixed and variable-length arrays

// TODO: Implement XDR codec
// - Encode/decode primitives (big-endian)
// - Encode/decode variable-length opaque and string
// - Encode/decode NFS file handles (64-byte opaque)
// - Encode/decode fattr3, post_op_attr, etc.
