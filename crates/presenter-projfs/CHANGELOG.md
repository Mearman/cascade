# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.38](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.37...cascade-presenter-projfs-v0.1.38) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.37
- release v0.1.36
- release v0.1.35
- release v0.1.34
- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.37](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.36...cascade-presenter-projfs-v0.1.37) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.36
- release v0.1.35
- release v0.1.34
- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.36](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.35...cascade-presenter-projfs-v0.1.36) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.35
- release v0.1.34
- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.35](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.34...cascade-presenter-projfs-v0.1.35) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.34
- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.34](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.33...cascade-presenter-projfs-v0.1.34) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.33
- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.33](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.32...cascade-presenter-projfs-v0.1.33) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.32
- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.32](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.31...cascade-presenter-projfs-v0.1.32) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.31
- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.31](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.30...cascade-presenter-projfs-v0.1.31) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.30
- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.30](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.29...cascade-presenter-projfs-v0.1.30) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.29
- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.29](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.28...cascade-presenter-projfs-v0.1.29) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map poisoned-mutex sentinels to ERROR_INTERNAL_ERROR
- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.28
- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.28](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.27...cascade-presenter-projfs-v0.1.28) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* map PrjAllocateAlignedBuffer failure to ERROR_NOT_ENOUGH_MEMORY
- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- *(presenter-projfs)* use HResultCode for the browse-only no-provider sentinel
- release v0.1.27
- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.27](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.26...cascade-presenter-projfs-v0.1.27) - 2026-06-01

### Added

- *(presenter-projfs)* map io::ErrorKind to typed HRESULT via hresult_for_io_error
- *(presenter-projfs)* add HResultCode newtype with FACILITY_WIN32 packing
- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(followups)* address review findings from the all-followups round
- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- *(presenter-projfs)* propagate HResultCode through ProviderReadOutcome::Failed
- release v0.1.26
- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.26](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.25...cascade-presenter-projfs-v0.1.26) - 2026-06-01

### Added

- *(presenter-projfs)* cancel in-flight reads via CancelCommand
- *(presenter-projfs)* wire Notification events to typed enum and trace logs
- *(presenter-projfs)* implement GetFileData via aligned buffer + PrjWriteFileData
- *(presenter-projfs)* add ContentProvider trait and presenter wiring
- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* address Windows clippy and recommended review findings
- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.25
- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.25](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.24...cascade-presenter-projfs-v0.1.25) - 2026-06-01

### Added

- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.24
- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.24](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.23...cascade-presenter-projfs-v0.1.24) - 2026-06-01

### Added

- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.23
- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.23](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.22...cascade-presenter-projfs-v0.1.23) - 2026-06-01

### Added

- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- release v0.1.22
- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure

## [0.1.22](https://github.com/Mearman/cascade/compare/cascade-presenter-projfs-v0.1.21...cascade-presenter-projfs-v0.1.22) - 2026-06-01

### Added

- *(presenter-projfs)* serve directory browse callbacks from items map
- *(presenter-projfs)* expose root_id, enumeration state, and browse helpers
- *(presenter-projfs)* scaffold the crate with VfsPresenter stubs

### Fixed

- *(presenter-projfs)* satisfy workspace pedantic lints on Windows clippy
- *(presenter-projfs)* trim unread fields from CallbackContext wrapper
- *(presenter-projfs)* build against windows 0.59 on x86_64-pc-windows-msvc
- *(presenter-projfs)* add Drop impl + document async-context callback contract

### Other

- render cross-crate type references as code instead of broken links
- *(presenter-projfs)* note browse callbacks live, narrow follow-up list
- *(presenter-projfs)* describe scaffold scope and follow-up callbacks
- *(presenter-projfs)* smoke test in-memory upsert and non-Windows start failure
