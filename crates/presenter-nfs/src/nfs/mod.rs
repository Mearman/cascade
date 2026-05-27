pub mod mount;
pub mod procedures;
pub mod server;
pub mod xdr;

// Phase 1: NFSv3 server implementation stubs.
// Full implementation will cover:
// - Mount protocol (RFC 1814): MOUNT, DUMP, UNMOUNT
// - NFS procedures (RFC 1813): GETATTR, LOOKUP, READDIR, READ, FSSTAT,
//   CREATE, WRITE, REMOVE, RENAME, MKDIR, RMDIR, SETATTR, COMMIT
// - XDR codec for all NFS data structures
