//! `NFSv4` protocol support (RFC 5661 subset).
//!
//! Implements a minimal `NFSv4` server sufficient for macOS 26+ mount
//! compatibility. Provides COMPOUND procedure handling with the following
//! operations:
//!
//! - `PUTROOTFH`, `PUTFH`, `GETFH` — file handle manipulation
//! - `LOOKUP` — pathname resolution
//! - `GETATTR`, `ACCESS` — attribute retrieval
//! - `READDIR` — directory listing
//! - `OPEN`, `CLOSE` — file state management (minimal)
//! - `READ`, `WRITE` — data transfer
//! - `CREATE`, `REMOVE`, `RENAME` — namespace operations
//! - `SAVEFH`, `RESTOREFH` — file handle stack
//!
//! No delegations, callbacks, or lock manager support.

pub mod compound;
pub mod state;
pub mod xdr;
