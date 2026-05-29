pub mod context;
pub mod mount;
pub mod procedures;
pub mod server;
pub mod v4;
pub mod xdr;

// NFSv3 + NFSv4 server implementation.
//
// NFSv3 (RFC 1813): Mount protocol + individual procedures.
//   - Mount protocol (RFC 1814): MOUNT, DUMP, UNMOUNT
//   - NFS procedures: GETATTR, LOOKUP, READDIR, READ, FSSTAT,
//     CREATE, WRITE, REMOVE, RENAME, MKDIR, RMDIR, SETATTR, COMMIT
//   - XDR codec for all NFSv3 data structures
//
// NFSv4 (RFC 5661): COMPOUND procedure with chained operations.
//   - Operations: PUTROOTFH, LOOKUP, GETATTR, READDIR, OPEN, CLOSE,
//     READ, WRITE, CREATE, REMOVE, RENAME, ACCESS, SAVEFH, RESTOREFH
//   - Minimal state management for OPEN/CLOSE
//   - No delegations, callbacks, or lock manager
//
// The server auto-detects v3 vs v4 requests based on the RPC program
// version field and dispatches accordingly.
