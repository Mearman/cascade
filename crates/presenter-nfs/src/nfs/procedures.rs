//! NFS procedure handlers stub.
//!
//! Required procedures:
//! - GETATTR — return file/directory metadata
//! - LOOKUP — resolve a name in a directory
//! - READDIR / READDIR3 — list directory contents
//! - READ — read file data
//! - FSSTAT — return filesystem statistics
//!
//! Write procedures (Phase 3+):
//! - CREATE, WRITE, REMOVE, RENAME, MKDIR, RMDIR, SETATTR, COMMIT

// TODO: Implement procedure handlers
// Each handler receives parsed XDR parameters and returns XDR-encoded results.
// Handlers delegate to the engine's VFS via the Backend trait.
