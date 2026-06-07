//! Test module for `sync.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "sync_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

    use super::*;
    use cascade_p2p::identity::DeviceIdentity;
    use tempfile::tempdir;

    fn make_engine(folder_id: &str) -> (tempfile::TempDir, SyncEngine) {
        let dir = tempdir().unwrap();
        let index = Arc::new(FolderIndex::open(&dir.path().join("idx.db")).unwrap());
        let blocks = Arc::new(BlockStore::new(&dir.path().join("blocks")).unwrap());
        let identity = DeviceIdentity::generate().unwrap();
        let engine = SyncEngine::new(folder_id.to_string(), index, blocks, identity);
        (dir, engine)
    }

    /// Two engines on loopback. A uploads a file, B should see it in
    /// its index after the `IndexUpdate` broadcast.
    #[tokio::test]
    async fn upload_propagates_via_index_update() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        let (_cancel_tx_b, cancel_rx_b) = tokio::sync::watch::channel(false);
        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_b)
            .await
            .unwrap();
        engine_a
            .connect_to(Peer {
                device_id: engine_b.device_id().to_string(),
                address: addr_b,
            })
            .await
            .unwrap();

        // Let the handshake settle.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Upload on A by inserting directly into A's index, then broadcast.
        let entry = IndexEntry {
            path: "hello.txt".to_string(),
            is_dir: false,
            size: 11,
            modified: 1_700_000_000,
            block_hashes: vec![0u8; 32],
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        engine_a.index.upsert(&entry).unwrap();
        engine_a.broadcast_update(&entry).await;

        // Wait for B to receive the IndexUpdate.
        for _ in 0..40 {
            if engine_b.index.get("hello.txt").unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("hello.txt did not appear in B's index");
    }

    /// Block-level fetch round trip. A has a block, B requests it.
    #[tokio::test]
    async fn fetch_block_from_peer() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // Pre-populate A's block store.
        let data = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let hash = BlockHash::from_data(&data);
        engine_a.blocks.store_block(&hash, &data).await.unwrap();

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = engine_a
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        engine_b
            .connect_to(Peer {
                device_id: engine_a.device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        // Let the handshake settle so the peer handle is registered.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let peers = engine_b.peers.lock().await;
            if peers.contains_key(engine_a.device_id()) {
                break;
            }
        }

        let fetched = engine_b
            .fetch_block(
                "anything.bin",
                0,
                u32::try_from(data.len()).unwrap(),
                hash.0,
            )
            .await
            .expect("expected to fetch block from peer");
        assert_eq!(fetched, data);
    }

    /// Two concurrent block requests against the same peer must each
    /// get a distinct `request_id` and must each receive the right
    /// payload — no Response can be misrouted by FIFO order.
    #[tokio::test]
    async fn request_block_uses_distinct_request_ids_concurrently() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // Two distinct blocks on A's store.
        let data_x = vec![0xAAu8; 4096];
        let data_y = vec![0xBBu8; 4096];
        let hash_x = BlockHash::from_data(&data_x);
        let hash_y = BlockHash::from_data(&data_y);
        engine_a.blocks.store_block(&hash_x, &data_x).await.unwrap();
        engine_a.blocks.store_block(&hash_y, &data_y).await.unwrap();

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = engine_a
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        engine_b
            .connect_to(Peer {
                device_id: engine_a.device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        // Wait for the peer handle to be registered.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let peers = engine_b.peers.lock().await;
            if peers.contains_key(engine_a.device_id()) {
                break;
            }
        }

        // Fire both requests concurrently and assert both succeed with
        // the right bytes.
        let size = u32::try_from(data_x.len()).unwrap();
        let engine_b_x = engine_b.clone();
        let engine_b_y = engine_b.clone();
        let fut_x =
            tokio::spawn(async move { engine_b_x.fetch_block("x.bin", 0, size, hash_x.0).await });
        let fut_y =
            tokio::spawn(async move { engine_b_y.fetch_block("y.bin", 0, size, hash_y.0).await });

        let got_x = fut_x.await.unwrap().expect("expected X block");
        let got_y = fut_y.await.unwrap().expect("expected Y block");
        assert_eq!(got_x, data_x);
        assert_eq!(got_y, data_y);

        // The peer's id allocator must have advanced by at least two.
        let peers = engine_b.peers.lock().await;
        let handle = peers.get(engine_a.device_id()).unwrap();
        assert!(
            handle.next_request_id.load(Ordering::Relaxed) >= 2,
            "expected at least two ids consumed",
        );
    }

    #[tokio::test]
    async fn merge_files_skips_when_local_dominates() {
        let (_dir, engine) = make_engine("f");
        // Local row carries a strictly newer vector for device 1.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 2_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 5)],
            })
            .unwrap();

        // Older-by-vector incoming row (`(1, 2)` < `(1, 5)`) must be ignored.
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 1_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 2)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 10);
        assert_eq!(after.version, vec![(1, 5)]);
    }

    #[tokio::test]
    async fn merge_files_takes_dominating_peer() {
        let (_dir, engine) = make_engine("f");
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Incoming dominates: `(1, 1)` is strictly less than `(1, 3)`.
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 3)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 99);
        assert_eq!(after.modified, 2_000_000_000);
        assert_eq!(after.block_hashes, vec![1u8; 32]);
        assert_eq!(after.version, vec![(1, 3)]);
    }

    #[tokio::test]
    async fn merge_files_noop_on_equal_vector() {
        let (_dir, engine) = make_engine("f");
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1), (2, 2)],
            })
            .unwrap();
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99, // would be different content, but vector equals — skip
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 1), (2, 2)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert_eq!(after.size, 10, "equal vectors are no-ops");
    }

    #[test]
    fn conflict_copy_path_preserves_extension() {
        assert_eq!(
            conflict_copy_path("docs/report.txt", "7BHJ62FL", 1_700_000_000),
            "docs/report.conflict-7BHJ62FL-1700000000.txt",
        );
    }

    #[test]
    fn conflict_copy_path_handles_no_extension() {
        assert_eq!(
            conflict_copy_path("README", "7BHJ62FL", 1_700_000_000),
            "README.conflict-7BHJ62FL-1700000000",
        );
    }

    #[test]
    fn conflict_copy_path_handles_dot_prefixed_filename() {
        // A leading dot is a hidden-file marker, not an extension
        // separator — the whole `.gitignore` is the stem, so no
        // extension is preserved.
        assert_eq!(
            conflict_copy_path(".gitignore", "7BHJ62FL", 1_700_000_000),
            ".gitignore.conflict-7BHJ62FL-1700000000",
        );
    }

    #[test]
    fn conflict_copy_path_splits_on_last_dot() {
        // Compound extensions like `.tar.gz` split on the LAST dot:
        // stem = `archive.tar`, ext = `gz`. The conflict marker lands
        // between them. Two-component restoration (`gunzip` then
        // `tar -xf`) still recognises the file shape.
        assert_eq!(
            conflict_copy_path("archive.tar.gz", "7BHJ62FL", 1_700_000_000),
            "archive.tar.conflict-7BHJ62FL-1700000000.gz",
        );
    }

    #[test]
    fn conflict_copy_path_uses_friendly_name() {
        // A sanitised friendly name is passed positionally where the
        // short device id used to live — the format is unchanged, only
        // the source of the identifier differs.
        assert_eq!(
            conflict_copy_path("doc.txt", "work-laptop", 1_700_000_000),
            "doc.conflict-work-laptop-1700000000.txt",
        );
    }

    #[test]
    fn sanitise_for_path_handles_special_chars() {
        // The three cases called out by the design: whitespace, path
        // separators, and dots all replace one-for-one and lowercase.
        assert_eq!(sanitise_for_path("Work Laptop"), "work-laptop");
        assert_eq!(sanitise_for_path("home/server"), "home-server");
        assert_eq!(sanitise_for_path(".."), "--");
    }

    #[test]
    fn sanitise_for_path_lowercases_and_normalises_metacharacters() {
        // Mixed-case alphanumerics and apostrophes survive intact
        // except for the lowercasing pass; shell metacharacters,
        // colons, and backslashes normalise to single dashes
        // one-for-one. Replacement is not collapsed, so `C:\` becomes
        // `c--` (colon + backslash).
        assert_eq!(sanitise_for_path("Joe's MacBook"), "joe's-macbook");
        assert_eq!(sanitise_for_path("C:\\users\\joe"), "c--users-joe");
    }

    #[test]
    fn sanitise_for_path_empty_input_returns_empty() {
        // An empty input must produce an empty output so the caller
        // can detect the case and fall back to the short device id —
        // returning a placeholder here would defeat the fallback.
        assert_eq!(sanitise_for_path(""), "");
    }

    #[tokio::test]
    async fn persist_conflict_copy_uses_friendly_name_when_set() {
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_local_device_name(Some("Work Laptop".to_string()));
        // Seed a local row so `merge_files` has something to displace.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Concurrent incoming write — neither vector dominates.
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        // The displaced local row should be persisted at a sibling
        // path stamped with the sanitised friendly name, not the
        // opaque short device id.
        let conflict_row = engine
            .index
            .list_children("")
            .unwrap()
            .into_iter()
            .find(|e| e.path.starts_with("doc.conflict-work-laptop-"))
            .expect("conflict copy should use the friendly name");
        assert_eq!(
            std::path::Path::new(&conflict_row.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("txt"),
            "conflict copy preserves the original extension",
        );
        assert_eq!(
            conflict_row.size, 10,
            "conflict copy keeps local content size"
        );
    }

    #[tokio::test]
    async fn persist_conflict_copy_falls_back_to_short_id_without_name() {
        let (_dir, engine) = make_engine("f");
        // No friendly name configured — `local_device_name` is `None`
        // by default — so the short device id must identify the
        // displaced side.
        assert!(engine.local_device_name().is_none());
        let short_id = local_short_device_id(engine.device_id());
        let conflict_prefix = format!("doc.conflict-{short_id}-");

        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let conflict_row = engine
            .index
            .list_children("")
            .unwrap()
            .into_iter()
            .find(|e| e.path.starts_with(&conflict_prefix))
            .expect("conflict copy should use the short device id when no friendly name is set");
        assert_eq!(
            std::path::Path::new(&conflict_row.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("txt"),
        );
    }

    #[tokio::test]
    async fn persist_conflict_copy_falls_back_when_friendly_name_sanitises_to_empty() {
        // A friendly name that consists entirely of replaced
        // characters sanitises to a string of dashes — non-empty —
        // and is still preferred over the short device id. The
        // genuine empty-string case (which would otherwise produce a
        // bare `.conflict--<ts>.` path) is the one we must guard.
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_local_device_name(Some(String::new()));
        let short_id = local_short_device_id(engine.device_id());
        let conflict_prefix = format!("doc.conflict-{short_id}-");

        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let conflict_row = engine
            .index
            .list_children("")
            .unwrap()
            .into_iter()
            .find(|e| e.path.starts_with(&conflict_prefix))
            .expect("empty friendly name must fall back to the short device id");
        assert_eq!(
            std::path::Path::new(&conflict_row.path)
                .extension()
                .and_then(std::ffi::OsStr::to_str),
            Some("txt"),
        );
    }

    #[tokio::test]
    async fn seed_peer_names_round_trips_via_peer_name_lookup() {
        let (_dir, engine) = make_engine("f");
        engine
            .seed_peer_names(vec![
                ("AAAAA".to_string(), "home-laptop".to_string()),
                // An empty value is ignored — the absence is preserved.
                ("BBBBB".to_string(), String::new()),
            ])
            .await;
        assert_eq!(
            engine.peer_name("AAAAA").await.as_deref(),
            Some("home-laptop"),
        );
        assert!(engine.peer_name("BBBBB").await.is_none());
        assert!(engine.peer_name("CCCCC").await.is_none());
    }

    #[tokio::test]
    async fn merge_files_concurrent_edit_accepts_incoming() {
        let (_dir, engine) = make_engine("f");
        // Local row bumped by device 1; incoming row bumped by device 2.
        // Neither dominates — concurrent edit on disconnected peers.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        // The incoming row overwrites the original (matching the
        // ordering chosen by merge_files on the concurrent branch);
        // the version vector must merge both counters so a third peer
        // sees the full history. Separately, the conflict-copy path
        // covered by `merge_files_persists_conflict_copy` ensures the
        // displaced local content is preserved at a sibling path.
        assert_eq!(after.size, 99);
        assert!(
            after.version.iter().any(|(id, _)| *id == 1),
            "local device counter must survive the merge"
        );
        assert!(
            after.version.iter().any(|(id, _)| *id == 2),
            "remote device counter must be present after the merge"
        );
    }

    #[tokio::test]
    async fn merge_files_merges_version_vectors_on_conflict() {
        let (_dir, engine) = make_engine("f");
        // Seed local: version = [(1, 1)]
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 100,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Receive incoming with concurrent VV: [(2, 1)] — neither dominates.
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 100,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(2, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        // After merge, the row must contain BOTH counters.
        let row = engine.index.get("doc.txt").unwrap().unwrap();
        assert!(
            row.version.iter().any(|(id, _)| *id == 1),
            "local device counter dropped"
        );
        assert!(
            row.version.iter().any(|(id, _)| *id == 2),
            "remote device counter missing"
        );
    }

    #[tokio::test]
    async fn merge_files_ignores_directory_entries() {
        let (_dir, engine) = make_engine("f");
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "subdir".into(),
                    file_type: FILE_TYPE_DIR,
                    size: 0,
                    modified: 1_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version::default(),
                    block_hashes: vec![],
                }],
            )
            .unwrap();
        assert!(engine.index.get("subdir").unwrap().is_none());
    }

    #[tokio::test]
    async fn merge_files_applies_dominating_tombstone() {
        let (_dir, engine) = make_engine("f");
        // Seed an undeleted local row with version `(1, 1)`.
        engine
            .index
            .upsert(&IndexEntry {
                path: "doc.txt".into(),
                is_dir: false,
                size: 10,
                modified: 1_000_000_000,
                block_hashes: vec![0u8; 32],
                deleted: false,
                row_version: 0,
                version: vec![(1, 1)],
            })
            .unwrap();
        // Incoming tombstone dominates with `(1, 2)`.
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "doc.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 0,
                    modified: 2_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: true,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 2)],
                    },
                    block_hashes: vec![],
                }],
            )
            .unwrap();
        let after = engine.index.get("doc.txt").unwrap().unwrap();
        assert!(after.deleted, "row should be marked deleted");
        assert_eq!(after.version, vec![(1, 2)]);
    }

    #[tokio::test]
    async fn merge_files_creates_tombstone_for_unknown_path() {
        let (_dir, engine) = make_engine("f");
        // No prior upsert for "gone.txt".
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "gone.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 0,
                    modified: 1_700_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    block_hashes: vec![],
                    deleted: true,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(7, 1)],
                    },
                }],
            )
            .unwrap();
        let row = engine
            .index
            .get("gone.txt")
            .unwrap()
            .expect("tombstone row should exist");
        assert!(row.deleted);
        assert_eq!(row.modified, 1_700_000_000);
        assert_eq!(row.version, vec![(7, 1)]);
    }

    #[tokio::test]
    async fn merge_files_skips_unknown_file_type() {
        let (_dir, engine) = make_engine("f");
        engine
            .merge_files(
                "peer-test",
                &[FileInfo {
                    name: "weird".into(),
                    file_type: 99,
                    size: 1,
                    modified: 1_000_000_000,
                    sequence: 0,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version::default(),
                    block_hashes: vec![[0u8; 32]],
                }],
            )
            .unwrap();
        assert!(engine.index.get("weird").unwrap().is_none());
    }

    #[tokio::test]
    async fn broadcast_update_skips_directories() {
        let (_dir, engine) = make_engine("f");
        // No peers connected; broadcast should be a quiet no-op for
        // dir entries (we just confirm no panic). Tombstones are now
        // broadcast normally and are exercised by the integration test.
        let dir = IndexEntry {
            path: "subdir".into(),
            is_dir: true,
            size: 0,
            modified: 0,
            block_hashes: vec![],
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        engine.broadcast_update(&dir).await;
    }

    /// `connect_to` should record the dialled peer in our `PeerBook`.
    #[tokio::test]
    async fn peer_book_records_outbound_connections() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        let (_cancel_tx_b, cancel_rx_b) = tokio::sync::watch::channel(false);
        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_b)
            .await
            .unwrap();
        engine_a
            .connect_to(Peer {
                device_id: engine_b.device_id().to_string(),
                address: addr_b,
            })
            .await
            .unwrap();

        let book = engine_a.peer_book().read().await;
        let recorded = book
            .get(engine_b.device_id())
            .expect("B should be recorded in A's peer book");
        assert_eq!(recorded.addresses, vec![addr_b]);
        assert!(
            recorded.introduced_by.is_empty(),
            "manual contact should record no introducer"
        );
        assert!(
            recorded.last_seen > 0,
            "outbound connect should stamp last_seen with the contact time",
        );
    }

    /// `handle_inbound` should record the accepted peer in our `PeerBook`.
    #[tokio::test]
    async fn peer_book_records_inbound_connections() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        let (_cancel_tx_b, cancel_rx_b) = tokio::sync::watch::channel(false);
        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_b)
            .await
            .unwrap();
        engine_a
            .connect_to(Peer {
                device_id: engine_b.device_id().to_string(),
                address: addr_b,
            })
            .await
            .unwrap();

        // The inbound handler records the peer asynchronously inside the
        // listener task; poll the peer book until A appears (or fail).
        let mut found_last_seen: Option<i64> = None;
        for _ in 0..40 {
            let book = engine_b.peer_book().read().await;
            if let Some(entry) = book.get(engine_a.device_id()) {
                found_last_seen = Some(entry.last_seen);
                break;
            }
            drop(book);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let last_seen = found_last_seen.expect("A should be recorded in B's peer book");
        assert!(
            last_seen > 0,
            "inbound accept should stamp last_seen with the contact time",
        );
    }

    /// `current_gossip_snapshot` must carry the per-peer `last_seen`
    /// stamped on each `KnownPeer`, not a single broadcast-time
    /// timestamp. Build a book with two peers at known timestamps and
    /// confirm both come back through the snapshot.
    #[tokio::test]
    async fn broadcast_gossip_uses_per_peer_last_seen() {
        let (_dir, engine) = make_engine("f");
        {
            let mut book = engine.peer_book.write().await;
            book.add_peer(
                "DEVICE-A".to_string(),
                vec!["127.0.0.1:22000".parse().unwrap()],
            );
            book.mark_seen("DEVICE-A", 1_700_000_000);
            book.add_peer(
                "DEVICE-B".to_string(),
                vec!["127.0.0.1:22001".parse().unwrap()],
            );
            book.mark_seen("DEVICE-B", 1_700_005_000);
        }
        let snapshot = engine.current_gossip_snapshot().await;
        assert_eq!(snapshot.len(), 2);
        let by_id: HashMap<&str, &GossipPeer> =
            snapshot.iter().map(|p| (p.device_id.as_str(), p)).collect();
        assert_eq!(
            by_id.get("DEVICE-A").unwrap().snapshot_unix_seconds,
            1_700_000_000,
            "snapshot must carry the per-peer last_seen, not a global stamp",
        );
        assert_eq!(
            by_id.get("DEVICE-B").unwrap().snapshot_unix_seconds,
            1_700_005_000,
        );
    }

    /// A peer learned via gossip but never directly contacted has a
    /// `last_seen` of `0` and must be broadcast that way — we must not
    /// fabricate a contact time we cannot vouch for.
    #[tokio::test]
    async fn gossip_introduced_peers_broadcast_with_zero_last_seen() {
        let (_dir, engine) = make_engine("f");
        {
            let mut book = engine.peer_book.write().await;
            // Simulate a peer learned solely through gossip — never
            // confirmed reachable by us.
            let message = cascade_p2p::wan::GossipMessage {
                peers: vec![cascade_p2p::wan::GossipPeer {
                    device_id: "DEVICE-C".to_string(),
                    addresses: vec!["127.0.0.1:22002".parse().unwrap()],
                }],
            };
            book.merge_gossip("INTRODUCER", engine.device_id(), &message);
        }
        let snapshot = engine.current_gossip_snapshot().await;
        let entry = snapshot
            .iter()
            .find(|p| p.device_id == "DEVICE-C")
            .expect("gossip-introduced peer should appear in snapshot");
        assert_eq!(
            entry.snapshot_unix_seconds, 0,
            "uncontacted peers must broadcast last_seen = 0",
        );
    }

    /// Peer-as-STUN over a real loopback handshake: A dials B. Only the
    /// accepting side (B) observes a genuine NAT-mapped source — A's
    /// ephemeral outbound port — so only B sends an `ObservedAddress`
    /// frame. The connector (A) must NOT echo the address it dialled
    /// (B's own listening address) back to B, so the listener must never
    /// record its own listening address as a server-reflexive candidate.
    ///
    /// Over loopback the source B observes for A is a `127.0.0.0/8`
    /// address, which is not globally routable, so the scope filter in
    /// `set_observed_external_addr` drops it at ingress. The net effect
    /// is that neither side records a reflexive candidate — which is
    /// exactly correct: a same-host observation conveys no public
    /// reachability. (A routable observation is covered by
    /// `set_observed_external_addr_rejects_non_routable_sources`.)
    #[tokio::test]
    async fn observed_address_flows_only_from_acceptor() {
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        let (_cancel_tx_b, cancel_rx_b) = tokio::sync::watch::channel(false);
        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_b)
            .await
            .unwrap();
        engine_a
            .connect_to(Peer {
                device_id: engine_b.device_id().to_string(),
                address: addr_b,
            })
            .await
            .unwrap();

        // Let the handshake and any ObservedAddress frames settle.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The listener (B) must never record its own listening address as
        // a server-reflexive candidate. Before the fix, the connector
        // echoed the dialled address (exactly `addr_b`) back to B, which
        // folded it in as a bogus reflexive candidate. The connector now
        // sends nothing, so B holds no reflexive candidate at all.
        let b_reflexive = engine_b.observed_external_candidates().await;
        assert!(
            !b_reflexive.iter().any(|c| c.address == addr_b),
            "listener must not record its own listening address {addr_b} as a reflexive candidate, got {b_reflexive:?}",
        );
        assert!(
            b_reflexive.is_empty(),
            "acceptor sends no ObservedAddress on the outbound leg, so B records nothing, got {b_reflexive:?}",
        );

        // A's loopback source is observed by B and echoed back, but the
        // scope filter drops it because loopback is not globally routable.
        let a_reflexive = engine_a.observed_external_candidates().await;
        assert!(
            a_reflexive.is_empty(),
            "loopback observations are not globally routable and must be dropped, got {a_reflexive:?}",
        );
    }

    /// A reflexive candidate sourced from an observed address must appear
    /// in the broadcast gossip frame as a self entry, and a fresh
    /// receiver merging that frame must record the reflexive address in
    /// its peer book.
    #[tokio::test]
    async fn observed_reflexive_address_propagates_through_gossip() {
        let (_dir, engine) = make_engine("f");
        let observed: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        engine.set_observed_external_addr(observed).await;

        // The self entry carrying the reflexive address must appear in
        // the snapshot the broadcaster sends.
        let snapshot = engine.current_gossip_snapshot().await;
        let self_entry = snapshot
            .iter()
            .find(|p| p.device_id == engine.device_id())
            .expect("self entry with reflexive address must be in the gossip snapshot");
        assert!(
            self_entry
                .addresses
                .iter()
                .any(|a| a == &observed.to_string()),
            "the observed reflexive address must be advertised, got {:?}",
            self_entry.addresses,
        );

        // A receiver merging that frame must record the address. The
        // receiver is a different device, so the self-exclusion guard in
        // PeerBook::merge_gossip does not drop the broadcaster's entry.
        let (_dir_rx, receiver) = make_engine("f");
        receiver.merge_gossip(engine.device_id(), snapshot).await;
        let book = receiver.peer_book().read().await;
        let recorded = book
            .get(engine.device_id())
            .expect("receiver should record the broadcaster from the gossip frame");
        assert!(
            recorded.addresses.contains(&observed),
            "receiver must merge the reflexive address into the peer book, got {:?}",
            recorded.addresses,
        );
    }

    /// Recording the same observed address more than once must not
    /// inflate the candidate set — repeated frames from several peers
    /// reporting the same reflexive address collapse to one candidate.
    #[tokio::test]
    async fn set_observed_external_addr_deduplicates() {
        let (_dir, engine) = make_engine("f");
        let observed: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        engine.set_observed_external_addr(observed).await;
        engine.set_observed_external_addr(observed).await;
        assert_eq!(engine.observed_external_candidates().await.len(), 1);
    }

    /// A peer on the same LAN (or the local host) observes a private,
    /// loopback, or link-local source. Folding those into the reflexive
    /// set would advertise an unreachable address to off-LAN peers as a
    /// public mapping, so `set_observed_external_addr` must drop them at
    /// ingress and store nothing.
    #[tokio::test]
    async fn set_observed_external_addr_rejects_non_routable_sources() {
        let (_dir, engine) = make_engine("f");
        for raw in [
            "127.0.0.1:51820",      // IPv4 loopback
            "10.0.0.5:51820",       // RFC1918 10/8
            "172.16.3.4:51820",     // RFC1918 172.16/12
            "192.168.1.20:51820",   // RFC1918 192.168/16
            "169.254.10.10:51820",  // IPv4 link-local
            "0.0.0.0:51820",        // unspecified
            "[::1]:51820",          // IPv6 loopback
            "[fe80::1]:51820",      // IPv6 link-local
            "[fc00::1]:51820",      // IPv6 unique-local (fc00::/8)
            "[fd12:3456::1]:51820", // IPv6 unique-local (fd00::/8)
            "[::]:51820",           // IPv6 unspecified
        ] {
            let observed: SocketAddr = raw.parse().unwrap();
            engine.set_observed_external_addr(observed).await;
            assert!(
                engine.observed_external_candidates().await.is_empty(),
                "non-routable observed source {raw} must not be stored as a reflexive candidate",
            );
        }

        // A genuinely routable observation is still recorded.
        let routable: SocketAddr = "198.51.100.7:51820".parse().unwrap();
        engine.set_observed_external_addr(routable).await;
        let stored = engine.observed_external_candidates().await;
        assert_eq!(
            stored.len(),
            1,
            "a globally-routable observed source must be recorded, got {stored:?}",
        );
    }

    #[tokio::test]
    async fn entry_to_file_info_rejects_partial_hash() {
        let entry = IndexEntry {
            path: "bad.txt".into(),
            is_dir: false,
            size: 1,
            modified: 0,
            block_hashes: vec![0u8; 31], // not a multiple of 32
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        let err = entry_to_file_info(&entry).unwrap_err();
        assert!(err.to_string().contains("partial hash"));
    }

    #[tokio::test]
    async fn merge_files_advances_peer_max_sequence() {
        let (_dir, engine) = make_engine("f");
        engine
            .merge_files(
                "peer-x",
                &[
                    FileInfo {
                        name: "a.txt".into(),
                        file_type: FILE_TYPE_FILE,
                        size: 1,
                        modified: 0,
                        sequence: 7,
                        block_size: 128 * 1024,
                        deleted: false,
                        invalid: false,
                        no_permissions: false,
                        version: Version {
                            counters: vec![(1, 1)],
                        },
                        block_hashes: vec![[0u8; 32]],
                    },
                    FileInfo {
                        name: "b.txt".into(),
                        file_type: FILE_TYPE_FILE,
                        size: 1,
                        modified: 0,
                        sequence: 15,
                        block_size: 128 * 1024,
                        deleted: false,
                        invalid: false,
                        no_permissions: false,
                        version: Version {
                            counters: vec![(1, 1)],
                        },
                        block_hashes: vec![[0u8; 32]],
                    },
                ],
            )
            .unwrap();
        assert_eq!(engine.index.get_peer_max_sequence("peer-x").unwrap(), 15);
    }

    #[tokio::test]
    async fn merge_files_does_not_regress_peer_max_sequence() {
        let (_dir, engine) = make_engine("f");
        // Seed: peer reports a high watermark first.
        engine.index.set_peer_max_sequence("peer-x", 100).unwrap();
        // A later batch with a lower max sequence must NOT overwrite
        // the prior value — frame reordering should never regress
        // the cursor.
        engine
            .merge_files(
                "peer-x",
                &[FileInfo {
                    name: "late.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 1,
                    modified: 0,
                    sequence: 4,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 1)],
                    },
                    block_hashes: vec![[0u8; 32]],
                }],
            )
            .unwrap();
        assert_eq!(
            engine.index.get_peer_max_sequence("peer-x").unwrap(),
            100,
            "out-of-order frames must not regress the cursor",
        );
    }

    #[tokio::test]
    async fn snapshot_since_filters_by_row_version() {
        // Three rows seeded into the index; entries_since(2) yields
        // only the third. Snapshot_since must mirror that.
        let (_dir, engine) = make_engine("f");
        for path in ["one.txt", "two.txt", "three.txt"] {
            engine
                .index
                .upsert(&IndexEntry {
                    path: path.into(),
                    is_dir: false,
                    size: 1,
                    modified: 0,
                    block_hashes: vec![0u8; 32],
                    deleted: false,
                    row_version: 0,
                    version: vec![(1, 1)],
                })
                .unwrap();
        }
        let delta = engine.snapshot_since(2).unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].name, "three.txt");
        assert_eq!(delta[0].sequence, 3);
    }

    #[tokio::test]
    async fn merge_files_skips_invalid_entries() {
        let (_dir, engine) = make_engine("f");
        engine
            .merge_files(
                "peer-x",
                &[FileInfo {
                    name: "midwrite.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 1_000_000_000,
                    sequence: 1,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: true,
                    no_permissions: false,
                    version: Version {
                        counters: vec![(1, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        assert!(
            engine.index.get("midwrite.txt").unwrap().is_none(),
            "invalid entries must not be upserted",
        );
    }

    #[tokio::test]
    async fn merge_files_skips_no_permissions_entries() {
        let (_dir, engine) = make_engine("f");
        engine
            .merge_files(
                "peer-x",
                &[FileInfo {
                    name: "secret.txt".into(),
                    file_type: FILE_TYPE_FILE,
                    size: 99,
                    modified: 1_000_000_000,
                    sequence: 1,
                    block_size: 128 * 1024,
                    deleted: false,
                    invalid: false,
                    no_permissions: true,
                    version: Version {
                        counters: vec![(1, 1)],
                    },
                    block_hashes: vec![[1u8; 32]],
                }],
            )
            .unwrap();
        assert!(
            engine.index.get("secret.txt").unwrap().is_none(),
            "no_permissions entries must not be upserted",
        );
    }

    /// The accept loop must observe the `cancel` watch and exit. Without
    /// this, dropping the `JoinHandle` would detach the task and leave
    /// the loop running forever, pinning the cloned engine (and its
    /// `Arc<FolderIndex>` / `Arc<BlockStore>`) past the backend's
    /// lifetime.
    #[tokio::test]
    async fn start_listener_exits_on_cancel() {
        let (_dir, engine) = make_engine("f");
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (_bound, handle) = engine
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();
        cancel_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener should exit within 2s of cancel")
            .expect("listener task should not panic");
    }

    // ── NAT traversal wiring ──

    /// Synthesise a host candidate for tests that need a concrete
    /// address+port without depending on the machine's real network
    /// interface list.
    fn fake_host_candidate(addr: SocketAddr, local_preference: u16) -> Candidate {
        Candidate::new(addr, CandidateKind::Host, local_preference)
    }

    #[test]
    fn aggregate_candidates_folds_host_set_and_external_addr() {
        // A typical run: two host candidates from the interface walk
        // plus one server-reflexive candidate derived from the STUN
        // mapping. All three should be present in the output, sorted
        // by descending priority.
        let host_a = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let host_b = fake_host_candidate("192.0.2.2:22000".parse().unwrap(), u16::MAX - 1);
        let external: SocketAddr = "203.0.113.5:42000".parse().unwrap();

        let aggregated = aggregate_candidates(vec![host_a, host_b], Some(external), Vec::new());

        assert_eq!(
            aggregated.len(),
            3,
            "host + host + srflx survives the merge"
        );
        // Host candidates outrank server-reflexive by type preference
        // (126 vs 100) — both hosts must come before the srflx.
        assert_eq!(aggregated[0].kind, CandidateKind::Host);
        assert_eq!(aggregated[1].kind, CandidateKind::Host);
        assert_eq!(aggregated[2].kind, CandidateKind::ServerReflexive);
        assert_eq!(aggregated[2].address, external);
    }

    #[test]
    fn aggregate_candidates_sorts_by_descending_priority() {
        // The decision tree on the receiving end picks the highest
        // priority pair first; the gossiped order must reflect that so
        // a naïve scan does not have to re-sort.
        let host_high = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let host_low = fake_host_candidate("192.0.2.2:22000".parse().unwrap(), 0);
        let external: SocketAddr = "203.0.113.5:42000".parse().unwrap();

        let aggregated = aggregate_candidates(
            vec![host_low, host_high], // Deliberately reversed input.
            Some(external),
            Vec::new(),
        );

        // The output must be priority-descending regardless of input
        // order — the highest-preference host first, the lowest host
        // second, and the server-reflexive last.
        assert!(aggregated[0].priority >= aggregated[1].priority);
        assert!(aggregated[1].priority >= aggregated[2].priority);
        assert_eq!(aggregated[0].address.ip().to_string(), "192.0.2.1");
        assert_eq!(aggregated[2].kind, CandidateKind::ServerReflexive);
    }

    #[test]
    fn aggregate_candidates_dedupes_by_address_and_kind() {
        // Two host inputs at the same address+kind collapse to one;
        // a server-reflexive at the same address but different kind
        // survives because the dedupe key is the pair, not the address
        // alone.
        let addr: SocketAddr = "192.0.2.1:22000".parse().unwrap();
        let host_a = fake_host_candidate(addr, u16::MAX);
        let host_a_dup = fake_host_candidate(addr, u16::MAX);

        let aggregated = aggregate_candidates(vec![host_a, host_a_dup], Some(addr), Vec::new());

        assert_eq!(
            aggregated.len(),
            2,
            "duplicate host collapses but the srflx at the same address survives"
        );
        let kinds: Vec<_> = aggregated.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&CandidateKind::Host));
        assert!(kinds.contains(&CandidateKind::ServerReflexive));
    }

    #[test]
    fn aggregate_candidates_handles_missing_external_addr() {
        // When NAT detection has not produced an external mapping yet
        // (or the host is on a public address), the aggregated set
        // contains only the host candidates and nothing else.
        let host = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let aggregated = aggregate_candidates(vec![host], None, Vec::new());
        assert_eq!(aggregated.len(), 1);
        assert_eq!(aggregated[0].kind, CandidateKind::Host);
    }

    #[test]
    fn aggregate_candidates_folds_extras_into_output() {
        // PeerReflexive / Relayed entries supplied via `extras` must
        // appear alongside the host + srflx set, sorted into the
        // priority order. No extras flow in production yet, but the
        // helper must honour them so a future round can wire them up
        // without changing the aggregation contract.
        let host = fake_host_candidate("192.0.2.1:22000".parse().unwrap(), u16::MAX);
        let relay_addr: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let relayed = Candidate::new(relay_addr, CandidateKind::Relayed, 0);

        let aggregated = aggregate_candidates(vec![host], None, vec![relayed]);

        assert_eq!(aggregated.len(), 2);
        assert_eq!(aggregated[0].kind, CandidateKind::Host);
        assert_eq!(aggregated[1].kind, CandidateKind::Relayed);
        assert_eq!(aggregated[1].address, relay_addr);
    }

    #[tokio::test]
    async fn decide_connectivity_chooses_direct_when_both_peers_open() {
        // Two Open peers must end up Direct. Feeding a synthetic host
        // candidate through `aggregate_candidates` proves the decision
        // tree honours the priority sort: the dialler targets that
        // address rather than (say) a relay endpoint.
        let host_addr: SocketAddr = "127.0.0.1:22000".parse().unwrap();
        let candidates = aggregate_candidates(
            vec![fake_host_candidate(host_addr, u16::MAX)],
            None,
            Vec::new(),
        );
        let strategy = decide_connectivity(NatType::Open, NatType::Open, &candidates, &[], &[]);
        match strategy {
            ConnectivityStrategy::Direct { addr } => assert_eq!(addr, host_addr),
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn local_external_addr_round_trips_via_setter() {
        // The background detection task calls `set_local_external_addr`
        // and the connection-time `gather_local_candidates` reads it
        // back. The round-trip is the contract under test here.
        let (_dir, engine) = make_engine("f");
        assert!(
            engine.local_external_addr().await.is_none(),
            "default is None until detection publishes a reading"
        );
        let external: SocketAddr = "203.0.113.5:42000".parse().unwrap();
        engine.set_local_external_addr(Some(external)).await;
        assert_eq!(engine.local_external_addr().await, Some(external));
    }

    #[test]
    fn decide_connectivity_chooses_relay_when_both_symmetric_with_relay() {
        // Symmetric ↔ Symmetric is doomed for direct punch — the table
        // routes through Relay when one is configured. Without a
        // relay, falls back to a best-effort punch (covered by the
        // upstream `cascade_p2p::traversal` tests).
        let relay: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let strategy =
            decide_connectivity(NatType::Symmetric, NatType::Symmetric, &[], &[], &[relay]);
        assert_eq!(
            strategy,
            ConnectivityStrategy::Relay {
                route: RelayRoute::Operated { endpoint: relay }
            }
        );
    }

    #[tokio::test]
    async fn recorded_peer_relay_is_preferred_over_operated_endpoint() {
        // A relay offer recorded on the engine must surface through
        // `peer_relays()` and, fed to `decide_connectivity` alongside an
        // operated endpoint for a Symmetric ↔ Symmetric pair, win.
        let (_dir, engine) = make_engine("f");
        let volunteer_addr: SocketAddr = "203.0.113.9:22000".parse().unwrap();
        engine
            .record_relay_offer("VOLUNTEER".to_owned(), vec![volunteer_addr])
            .await;

        let peer_relays = engine.peer_relays().await;
        assert_eq!(peer_relays.len(), 1);

        let operated: SocketAddr = "198.51.100.7:3478".parse().unwrap();
        let strategy = decide_connectivity(
            NatType::Symmetric,
            NatType::Symmetric,
            &[],
            &peer_relays,
            &[operated],
        );
        assert_eq!(
            strategy,
            ConnectivityStrategy::Relay {
                route: RelayRoute::Peer {
                    device_id: "VOLUNTEER".to_owned(),
                    address: volunteer_addr,
                }
            }
        );
    }

    /// End-to-end: a volunteer with a permissive policy and an `Open` NAT must
    /// actually emit a `RelayOffer` on session setup, and the connecting peer
    /// must record it so its `peer_relays()` becomes non-empty. This drives
    /// real framed TLS sessions over loopback — the gap the unit tests left,
    /// where `record_relay_offer` was only ever called directly.
    #[tokio::test]
    async fn volunteer_emits_relay_offer_on_session_setup() {
        let (_dir_v, volunteer) = make_engine("shared");
        let (_dir_a, requester) = make_engine("shared");

        volunteer.trust(requester.device_id().to_string()).await;
        requester.trust(volunteer.device_id().to_string()).await;

        // The volunteer is Open with the default Auto policy, so it should
        // advertise itself once its listener is bound (the offer addresses are
        // drawn from the bound host candidates, filtered to routable IPs).
        volunteer.set_local_nat_type(NatType::Open).await;

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (addr_v, _v_task) = volunteer
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();
        // Seed a routable external mapping so the offer set is non-empty even
        // though the loopback listener address itself is not globally routable.
        volunteer
            .set_local_external_addr(Some("203.0.113.7:22000".parse().unwrap()))
            .await;

        requester
            .connect_to(Peer {
                device_id: volunteer.device_id().to_string(),
                address: addr_v,
            })
            .await
            .unwrap();

        // Wait for the volunteer's handshake (which emits the RelayOffer) to
        // reach the requester and populate its peer-relay map.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let relays = requester.peer_relays().await;
            if let Some(relay) = relays.first() {
                assert_eq!(relay.device_id, volunteer.device_id());
                assert!(
                    relay
                        .addresses
                        .contains(&"203.0.113.7:22000".parse().unwrap()),
                    "offer must carry the volunteer's routable endpoint",
                );
                return;
            }
        }
        panic!("requester never recorded the volunteer's relay offer");
    }

    /// A node whose policy is `Off` must never advertise, even with an `Open`
    /// NAT and a routable endpoint — the connecting peer's `peer_relays()`
    /// stays empty over a full session.
    #[tokio::test]
    async fn off_policy_volunteer_emits_no_relay_offer_over_session() {
        let (_dir_v, volunteer) = make_engine("shared");
        let volunteer = volunteer.with_relay_volunteer(RelayVolunteer::Off);
        let (_dir_a, requester) = make_engine("shared");

        volunteer.trust(requester.device_id().to_string()).await;
        requester.trust(volunteer.device_id().to_string()).await;
        volunteer.set_local_nat_type(NatType::Open).await;

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (addr_v, _v_task) = volunteer
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();
        volunteer
            .set_local_external_addr(Some("203.0.113.7:22000".parse().unwrap()))
            .await;

        requester
            .connect_to(Peer {
                device_id: volunteer.device_id().to_string(),
                address: addr_v,
            })
            .await
            .unwrap();

        // Give the handshake ample time, then confirm no offer was recorded.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            requester.peer_relays().await.is_empty(),
            "an Off-policy node must not advertise itself as a relay",
        );
    }

    /// Full two-hop relay: A reaches B by tunnelling through volunteer V, with
    /// no direct A↔B connection. Proves bytes traverse A → V → B AND back:
    /// the inner BEP handshake completes (ClusterConfig/Index both ways), an
    /// `IndexUpdate` from A lands in B's index (forward), and A fetches a block
    /// that only B holds (return), so a request/response round trip survives
    /// the bidirectional bridge. The volunteer's bandwidth meter must also see
    /// the relayed bytes — the live path it was previously bypassing.
    #[tokio::test]
    async fn two_hop_relay_carries_bep_session_both_ways() {
        let (_dir_v, volunteer) = make_engine("shared");
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        // Full trust mesh — every device must accept the others' TLS.
        for peer in [engine_a.device_id(), engine_b.device_id()] {
            volunteer.trust(peer.to_string()).await;
        }
        engine_a.trust(volunteer.device_id().to_string()).await;
        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(volunteer.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // A block only B holds, so the return direction (B → A) is exercised
        // by a real Request/Response.
        let only_on_b = b"payload that only the target peer holds".repeat(4);
        let block_hash = BlockHash::from_data(&only_on_b);
        engine_b
            .blocks
            .store_block(&block_hash, &only_on_b)
            .await
            .unwrap();

        // The volunteer accepts inbound sessions; B and A both dial it.
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let (addr_v, _v_task) = volunteer
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx)
            .await
            .unwrap();

        // B connects to V first so the volunteer holds a live session to the
        // target before A asks to be bridged to it.
        engine_b
            .connect_to(Peer {
                device_id: volunteer.device_id().to_string(),
                address: addr_v,
            })
            .await
            .unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if volunteer.has_peer(engine_b.device_id()).await {
                break;
            }
        }
        assert!(
            volunteer.has_peer(engine_b.device_id()).await,
            "volunteer must hold a session to the target before bridging",
        );

        // A asks the volunteer to bridge it to B. No direct A↔B link exists.
        engine_a
            .attempt_peer_relay(
                &Peer {
                    device_id: engine_b.device_id().to_string(),
                    address: "127.0.0.1:1".parse().unwrap(),
                },
                volunteer.device_id(),
                addr_v,
            )
            .await
            .unwrap();

        // Wait for the inner relayed session to register on both ends.
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if engine_a.has_peer(engine_b.device_id()).await
                && engine_b.has_peer(engine_a.device_id()).await
            {
                break;
            }
        }
        assert!(
            engine_a.has_peer(engine_b.device_id()).await,
            "A must hold an inner session to B through the relay",
        );
        assert!(
            engine_b.has_peer(engine_a.device_id()).await,
            "B must hold an inner session to A through the relay",
        );

        // Forward direction: an IndexUpdate from A must reach B's index.
        let entry = IndexEntry {
            path: "relayed.txt".to_string(),
            is_dir: false,
            size: 11,
            modified: 1_700_000_000,
            block_hashes: vec![0u8; 32],
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        engine_a.index.upsert(&entry).unwrap();
        engine_a.broadcast_update(&entry).await;
        let mut forward_ok = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if engine_b.index.get("relayed.txt").unwrap().is_some() {
                forward_ok = true;
                break;
            }
        }
        assert!(
            forward_ok,
            "A's IndexUpdate never reached B through the relay"
        );

        // Return direction: A fetches a block only B holds. This requires a
        // Request A → B and a Response B → A, both crossing the bridge.
        let fetched = engine_a
            .fetch_block(
                "relayed.txt",
                0,
                u32::try_from(only_on_b.len()).unwrap(),
                block_hash.0,
            )
            .await
            .expect("A must fetch B's block back through the relay");
        assert_eq!(fetched, only_on_b);

        // The volunteer must have metered the relayed bytes — proof the live
        // forwarding path, not a parallel helper, accounted the traffic.
        assert!(
            volunteer.relay_capacity().bytes_relayed() > 0,
            "the relay bandwidth meter must see the bridged bytes",
        );
    }

    #[tokio::test]
    async fn volunteers_only_when_policy_allows_and_nat_permits() {
        let (_dir, engine) = make_engine("f");

        // Default policy is Auto, but NAT defaults to Unknown — a node
        // that cannot relay must not advertise itself.
        assert_eq!(engine.relay_volunteer(), RelayVolunteer::Auto);
        assert!(
            !engine.should_volunteer_as_relay().await,
            "Unknown NAT must not volunteer"
        );

        // FullCone permits relaying under Auto.
        engine.set_local_nat_type(NatType::FullCone).await;
        assert!(engine.should_volunteer_as_relay().await);

        // Open permits relaying under Auto.
        engine.set_local_nat_type(NatType::Open).await;
        assert!(engine.should_volunteer_as_relay().await);

        // A restrictive NAT cannot relay even when Open earlier.
        engine.set_local_nat_type(NatType::Symmetric).await;
        assert!(!engine.should_volunteer_as_relay().await);
    }

    #[tokio::test]
    async fn volunteer_policy_off_never_advertises() {
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_relay_volunteer(RelayVolunteer::Off);
        // Even with the most permissive NAT, Off means Off.
        engine.set_local_nat_type(NatType::Open).await;
        assert!(!engine.should_volunteer_as_relay().await);
    }

    #[tokio::test]
    async fn candidates_frame_updates_peer_book() {
        // Receiving a `BepMessage::Candidates` must store the wire
        // candidates on the peer book so the next traversal decision
        // can pair them against the local set.
        let (_dir, engine) = make_engine("f");
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let remote_addr: SocketAddr = "203.0.113.1:22001".parse().unwrap();
        let candidates = vec![Candidate::new(remote_addr, CandidateKind::Host, 1024)];
        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Candidates {
                    candidates: candidates.clone(),
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        let book = engine.peer_book.read().await;
        let stored = book
            .remote_candidates("PEER-A")
            .expect("candidates should be stored under PEER-A");
        assert_eq!(stored, candidates.as_slice());
    }

    #[tokio::test]
    async fn sync_punch_frame_records_agreement_on_peer_book() {
        // Inbound `SyncPunch` must record the peer's nonce and
        // deadline. The matching `run_hole_punch` call reads them back
        // via `PeerBook::current_punch_agreement`.
        let (_dir, engine) = make_engine("f");
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::SyncPunch {
                    nonce: 0xCAFE_BABE,
                    deadline_unix_ms: 1_700_000_000_000,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        let book = engine.peer_book.read().await;
        let agreement = book
            .current_punch_agreement("PEER-A")
            .expect("agreement should be stored under PEER-A");
        assert_eq!(agreement.nonce, 0xCAFE_BABE);
        assert_eq!(agreement.deadline_unix_ms, 1_700_000_000_000);
    }

    #[tokio::test]
    async fn local_nat_type_defaults_to_unknown_until_detection_publishes() {
        // No detection has run yet — the engine must report Unknown.
        // This is the conservative reading the strategy table treats
        // as "route through Relay (or best-effort punch)" rather than
        // a brittle optimistic Direct.
        let (_dir, engine) = make_engine("f");
        assert_eq!(engine.local_nat_type().await, NatType::Unknown);
    }

    #[tokio::test]
    async fn set_local_nat_type_publishes_to_strategy_input() {
        // The background detection task calls `set_local_nat_type` and
        // the connection-time `decide_connectivity` reads it back. The
        // round-trip is the contract under test here.
        let (_dir, engine) = make_engine("f");
        engine.set_local_nat_type(NatType::FullCone).await;
        assert_eq!(engine.local_nat_type().await, NatType::FullCone);
    }

    #[tokio::test]
    async fn ensure_sync_punch_agreement_reuses_fresh_peer_agreement() {
        // When the peer signals first, we honour their nonce instead
        // of allocating a new one — both sides must probe with the
        // same value or `run_hole_punch` will treat the matched probe
        // as a wrong-nonce stray and time out.
        let (_dir, engine) = make_engine("f");
        let peer_agreement = SyncPunchAgreement {
            nonce: 0xDEAD_BEEF,
            deadline_unix_ms: unix_now_ms() + 10_000,
        };
        {
            let mut book = engine.peer_book.write().await;
            book.start_punch_with("PEER-A", peer_agreement);
        }
        let got = engine.ensure_sync_punch_agreement("PEER-A").await.unwrap();
        assert_eq!(got.nonce, 0xDEAD_BEEF);
    }

    #[tokio::test]
    async fn ensure_sync_punch_agreement_replaces_expired() {
        // An expired agreement is treated as absent: a fresh nonce
        // and deadline are minted. Otherwise `run_hole_punch` would
        // reject the call with `DeadlinePassed` and burn a punch
        // budget on a doomed attempt. We stamp the stored nonce with
        // `u64::MAX` so the freshly-allocated one (drawn from the
        // monotonic process counter) cannot collide.
        let (_dir, engine) = make_engine("f");
        {
            let mut book = engine.peer_book.write().await;
            book.start_punch_with(
                "PEER-A",
                SyncPunchAgreement {
                    nonce: u64::MAX,
                    deadline_unix_ms: 0,
                },
            );
        }
        let got = engine.ensure_sync_punch_agreement("PEER-A").await.unwrap();
        assert!(got.deadline_unix_ms > unix_now_ms());
        assert_ne!(got.nonce, u64::MAX);
    }

    #[tokio::test]
    async fn connect_to_with_strategy_rejects_untrusted_peer() {
        // The trust check runs before any traversal logic — an
        // untrusted device must not get as far as candidate selection
        // or UDP socket binding.
        let (_dir, engine) = make_engine("f");
        let err = engine
            .connect_to_with_strategy(Peer {
                device_id: "STRANGER".to_string(),
                address: "127.0.0.1:1".parse().unwrap(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not trusted"));
    }

    // ── Management-plane request handling ──

    use std::sync::Mutex as StdMutex;

    /// A fake [`ManageDispatch`] that records the caller principal it was
    /// invoked with and returns a canned result, so a test can assert the
    /// authenticated peer device id is threaded through as the caller and the
    /// dispatch outcome is reflected back in the reply frame.
    struct RecordingDispatch {
        seen_caller: StdMutex<Option<String>>,
        result: ManageResult,
    }

    impl RecordingDispatch {
        fn new(result: ManageResult) -> Self {
            Self {
                seen_caller: StdMutex::new(None),
                result,
            }
        }

        fn caller(&self) -> Option<String> {
            self.seen_caller.lock().ok().and_then(|c| c.clone())
        }
    }

    #[async_trait::async_trait]
    impl ManageDispatch for RecordingDispatch {
        async fn dispatch(
            &self,
            caller: &DeviceId,
            _command: ManageCommand,
            _scope: ManageScope,
            _token: Option<String>,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> ManageResult {
            if let Ok(mut seen) = self.seen_caller.lock() {
                *seen = Some(caller.as_str().to_owned());
            }
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn manage_request_uses_authenticated_peer_as_caller_and_replies() {
        let dispatch = Arc::new(RecordingDispatch::new(ManageResult::Ok {
            summary: "did the thing".to_owned(),
        }));
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_manage_dispatch(dispatch.clone());

        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        engine
            .handle_manage_request(
                "PEER-DEVICE-ID",
                CallerAuthentication::TlsVerified,
                7,
                ManageCommand::StatusRead,
                ManageScope::Node,
                None,
                &tx,
            )
            .await;

        // The authenticated peer device id is the caller principal.
        assert_eq!(dispatch.caller().as_deref(), Some("PEER-DEVICE-ID"));
        // The reply echoes the request id and carries the dispatch outcome.
        match rx.try_recv() {
            Ok(BepMessage::ManageResponse { request_id, result }) => {
                assert_eq!(request_id, 7);
                assert_eq!(
                    result,
                    ManageResult::Ok {
                        summary: "did the thing".to_owned()
                    }
                );
            }
            other => panic!("expected a ManageResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manage_request_without_dispatch_is_refused_unauthorised() {
        // No dispatch port configured — the node is not accepting remote
        // administration, so a request is refused with a typed unauthorised
        // error rather than silently dropped.
        let (_dir, engine) = make_engine("f");
        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        engine
            .handle_manage_request(
                "PEER-DEVICE-ID",
                CallerAuthentication::TlsVerified,
                3,
                ManageCommand::CacheEvict,
                ManageScope::Node,
                None,
                &tx,
            )
            .await;
        match rx.try_recv() {
            Ok(BepMessage::ManageResponse { request_id, result }) => {
                assert_eq!(request_id, 3);
                assert!(matches!(
                    result,
                    ManageResult::Err {
                        kind: ManageErrorKind::Unauthorised,
                        ..
                    }
                ));
            }
            other => panic!("expected an unauthorised ManageResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manage_request_on_unverified_transport_is_refused_before_dispatch() {
        // Caller-spoofing regression: a relayed or post-hole-punch session
        // carries a wire-asserted device id with no end-to-end TLS handshake. A
        // ManageRequest arriving on such a session must be refused with
        // Unauthorised BEFORE the dispatch port is consulted, so a party that
        // can open a tunnel cannot assert a granted manager's device id and have
        // commands authorised under that spoofed principal. The dispatch double
        // is configured to return Ok, so the only way the reply is unauthorised
        // is if the gate refused the request without ever calling dispatch.
        let dispatch = Arc::new(RecordingDispatch::new(ManageResult::Ok {
            summary: "should never run".to_owned(),
        }));
        let (_dir, engine) = make_engine("f");
        let engine = engine.with_manage_dispatch(dispatch.clone());

        let (tx, mut rx) = mpsc::unbounded_channel::<BepMessage>();
        engine
            .handle_manage_request(
                // The attacker asserts a powerful manager's device id on the wire.
                "SPOOFED-MANAGER-DEVICE-ID",
                CallerAuthentication::Unverified,
                11,
                ManageCommand::CacheEvict,
                ManageScope::Node,
                None,
                &tx,
            )
            .await;

        // The dispatch port was never reached — no caller principal recorded.
        assert_eq!(
            dispatch.caller(),
            None,
            "an unverified session must not reach the dispatch port",
        );
        match rx.try_recv() {
            Ok(BepMessage::ManageResponse { request_id, result }) => {
                assert_eq!(request_id, 11);
                assert!(
                    matches!(
                        result,
                        ManageResult::Err {
                            kind: ManageErrorKind::Unauthorised,
                            ..
                        }
                    ),
                    "a ManageRequest on an unverified transport must be refused, got {result:?}",
                );
            }
            other => panic!("expected an unauthorised ManageResponse, got {other:?}"),
        }
    }

    // ── Data-plane directional sharing gates ──

    /// A configurable [`DataAuthority`] double. Returns a fixed [`DataAccess`]
    /// for every (peer, folder) and records the quarantine rows it was handed,
    /// so a test can assert both the access decision taken and the receive-only
    /// conflict handling.
    struct FixedDataAuthority {
        access: DataAccess,
        quarantined: StdMutex<Vec<(String, String, String)>>,
    }

    impl FixedDataAuthority {
        fn new(read: bool, write: bool) -> Arc<Self> {
            Arc::new(Self {
                access: DataAccess { read, write },
                quarantined: StdMutex::new(Vec::new()),
            })
        }

        /// The `(peer, path, file_json)` triples quarantined so far.
        fn quarantined(&self) -> Vec<(String, String, String)> {
            self.quarantined
                .lock()
                .map(|q| q.clone())
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl DataAuthority for FixedDataAuthority {
        async fn data_access(
            &self,
            _peer: &DeviceId,
            _folder: &str,
            _presented_token: Option<&str>,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<DataAccess> {
            Ok(self.access)
        }

        async fn quarantine_received(
            &self,
            peer: &DeviceId,
            _folder: &str,
            path: &str,
            file_json: &str,
            _observed_at: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<()> {
            if let Ok(mut q) = self.quarantined.lock() {
                q.push((
                    peer.as_str().to_owned(),
                    path.to_owned(),
                    file_json.to_owned(),
                ));
            }
            Ok(())
        }
    }

    /// A [`DataAuthority`] whose `data_access` always fails, to exercise the
    /// fail-closed branch of [`SyncEngine::data_access_for`].
    struct FailingDataAuthority;

    #[async_trait::async_trait]
    impl DataAuthority for FailingDataAuthority {
        async fn data_access(
            &self,
            _peer: &DeviceId,
            _folder: &str,
            _presented_token: Option<&str>,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<DataAccess> {
            anyhow::bail!("data authority store is unavailable")
        }

        async fn quarantine_received(
            &self,
            _peer: &DeviceId,
            _folder: &str,
            _path: &str,
            _file_json: &str,
            _observed_at: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn sample_file(name: &str) -> FileInfo {
        FileInfo {
            name: name.to_owned(),
            file_type: FILE_TYPE_FILE,
            size: 11,
            modified: 1_700_000_000,
            sequence: 1,
            block_size: 128 * 1024,
            deleted: false,
            invalid: false,
            no_permissions: false,
            version: Version::default(),
            block_hashes: vec![[7u8; 32]],
        }
    }

    #[tokio::test]
    async fn default_open_when_authority_unset_allows_both_directions() {
        // The non-breaking default: with no DataAuthority wired, a trusted peer
        // keeps full read-write access exactly as before the feature.
        let (_dir, engine) = make_engine("f");
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::TlsVerified)
            .await;
        assert!(access.read, "unset authority must default-open read");
        assert!(access.write, "unset authority must default-open write");
    }

    #[tokio::test]
    async fn default_open_holds_for_unverified_session_when_port_unset() {
        // The non-breaking default applies on every transport: an unverified
        // (relayed / post-punch) session keeps full access while the port is
        // unset, so the pre-feature relay sync behaviour is unchanged.
        let (_dir, engine) = make_engine("f");
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::Unverified)
            .await;
        assert!(
            access.read,
            "unset authority + unverified must default-open read"
        );
        assert!(
            access.write,
            "unset authority + unverified must default-open write"
        );
    }

    #[tokio::test]
    async fn unverified_session_is_no_share_once_port_is_set() {
        // Once directional enforcement engages, an unverified session has no
        // trustworthy principal — it is no-share both ways regardless of grants.
        let (_dir, engine) = make_engine("f");
        engine
            .set_data_authority(FixedDataAuthority::new(true, true))
            .await;
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::Unverified)
            .await;
        assert!(
            !access.read,
            "unverified session must not read once port is set"
        );
        assert!(
            !access.write,
            "unverified session must not write once port is set"
        );
    }

    #[tokio::test]
    async fn authority_failure_fails_closed_to_no_share() {
        // A faulty authority store must not leak data: the decision fails closed
        // to no-share in both directions rather than defaulting to full access.
        let (_dir, engine) = make_engine("f");
        engine
            .set_data_authority(Arc::new(FailingDataAuthority))
            .await;
        let access = engine
            .data_access_for("PEER-A", CallerAuthentication::TlsVerified)
            .await;
        assert!(!access.read, "authority error must deny read");
        assert!(!access.write, "authority error must deny write");
    }

    #[tokio::test]
    async fn write_denied_peer_is_quarantined_not_merged() {
        // Receive-only semantics: a peer we will not accept writes from has its
        // proposed rows recorded as flagged local additions, never merged into
        // the authoritative index, and the session is not torn down.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, false);
        engine.set_data_authority(authority.clone()).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Index {
                    folder: "f".to_owned(),
                    files: vec![sample_file("drop.txt")],
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .expect("a write-denied frame is consumed without error");

        // Not merged into our authoritative index.
        assert!(
            engine.index.get("drop.txt").unwrap().is_none(),
            "a write-denied peer's row must not be merged",
        );
        // Recorded in the quarantine, keyed by peer + path, carrying the row.
        let q = authority.quarantined();
        assert_eq!(q.len(), 1, "the rejected row must be quarantined");
        assert_eq!(q[0].0, "PEER-A");
        assert_eq!(q[0].1, "drop.txt");
        assert!(
            q[0].2.contains("drop.txt"),
            "the quarantined JSON must carry the proposed row, got {}",
            q[0].2,
        );
    }

    #[tokio::test]
    async fn write_allowed_peer_is_merged() {
        // A write-allowed peer merges as before, and nothing is quarantined.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, true);
        engine.set_data_authority(authority.clone()).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Index {
                    folder: "f".to_owned(),
                    files: vec![sample_file("keep.txt")],
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        assert!(
            engine.index.get("keep.txt").unwrap().is_some(),
            "a write-allowed peer's row must be merged",
        );
        assert!(
            authority.quarantined().is_empty(),
            "nothing is quarantined when the peer may write",
        );
    }

    #[tokio::test]
    async fn unverified_session_cannot_write_even_with_write_grant() {
        // A relayed / post-punch session asserts its device id on the wire; a
        // data:write grant keyed to it must NOT be honoured, or a spoofed id
        // could push content. The rows are quarantined, never merged.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, true);
        engine.set_data_authority(authority.clone()).await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::Unverified,
                BepMessage::Index {
                    folder: "f".to_owned(),
                    files: vec![sample_file("spoof.txt")],
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        assert!(
            engine.index.get("spoof.txt").unwrap().is_none(),
            "an unverified session must not write even with a write grant",
        );
        assert_eq!(
            authority.quarantined().len(),
            1,
            "the unverified write is quarantined, not merged",
        );
    }

    #[tokio::test]
    async fn read_denied_peer_gets_empty_block_response() {
        // A read-denied peer that requests a block we hold is told "no such
        // block" uniformly: an empty Response, learning nothing of our content.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(false, true);
        engine.set_data_authority(authority).await;

        // We DO hold the block — the gate must refuse before serving it.
        let data = b"secret payload".repeat(8);
        let hash = BlockHash::from_data(&data);
        engine.blocks.store_block(&hash, &data).await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Request {
                    request_id: 3,
                    folder: "f".to_owned(),
                    name: "secret.txt".to_owned(),
                    block_offset: 0,
                    block_size: 128 * 1024,
                    block_hash: hash.0,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(BepMessage::Response { request_id, data }) => {
                assert_eq!(request_id, 3);
                assert!(
                    data.is_empty(),
                    "a read-denied peer must get an empty Response even when we hold the block",
                );
            }
            other => panic!("expected an empty Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_allowed_peer_gets_block() {
        // The companion to the above: a read-allowed peer is served the block.
        let (_dir, engine) = make_engine("f");
        let authority = FixedDataAuthority::new(true, true);
        engine.set_data_authority(authority).await;

        let data = b"shared payload".repeat(8);
        let hash = BlockHash::from_data(&data);
        engine.blocks.store_block(&hash, &data).await.unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::Request {
                    request_id: 4,
                    folder: "f".to_owned(),
                    name: "shared.txt".to_owned(),
                    block_offset: 0,
                    block_size: 128 * 1024,
                    block_hash: hash.0,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(BepMessage::Response {
                request_id,
                data: got,
            }) => {
                assert_eq!(request_id, 4);
                assert_eq!(got, data, "a read-allowed peer must receive the block");
            }
            other => panic!("expected the block Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cluster_config_captures_and_clears_presented_token() {
        // A data token on the peer's ClusterConfig is captured for the session;
        // a later ClusterConfig with no token clears it (no stale authority).
        let (_dir, engine) = make_engine("f");
        let (tx, _rx) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let manage_pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ManageResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::ClusterConfig {
                    folders: vec![Folder {
                        id: "f".to_owned(),
                        label: "f".to_owned(),
                    }],
                    data_token: Some("token-json".to_owned()),
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();
        assert_eq!(
            engine
                .presented_data_tokens
                .lock()
                .await
                .get("PEER-A")
                .cloned(),
            Some("token-json".to_owned()),
        );

        engine
            .handle_message(
                "PEER-A",
                CallerAuthentication::TlsVerified,
                BepMessage::ClusterConfig {
                    folders: vec![],
                    data_token: None,
                },
                &tx,
                &pending,
                &manage_pending,
            )
            .await
            .unwrap();
        assert!(
            engine
                .presented_data_tokens
                .lock()
                .await
                .get("PEER-A")
                .is_none(),
            "a ClusterConfig with no token must clear the prior token",
        );
    }

    #[tokio::test]
    async fn read_only_peer_cannot_push_an_accepted_change() {
        // End-to-end over a live loopback session: peer B is read-only from A's
        // point of view (A serves B, but will not accept B's writes). B uploads
        // a file and broadcasts it; A must NOT merge it — B's edit stays local
        // on B and is quarantined on A.
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        // A treats B as read-only: A serves B (read=true) but rejects B's
        // writes (write=false). B has no restriction on A (default-open).
        let a_authority = FixedDataAuthority::new(true, false);
        engine_a.set_data_authority(a_authority.clone()).await;

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = engine_a
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        engine_b
            .connect_to(Peer {
                device_id: engine_a.device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;

        let entry = IndexEntry {
            path: "from-b.txt".to_string(),
            is_dir: false,
            size: 11,
            modified: 1_700_000_000,
            block_hashes: vec![0u8; 32],
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        engine_b.index.upsert(&entry).unwrap();
        engine_b.broadcast_update(&entry).await;

        // Give A time to receive and (correctly) reject the IndexUpdate.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(
            engine_a.index.get("from-b.txt").unwrap().is_none(),
            "a read-only peer must not be able to push an accepted change",
        );
        assert!(
            a_authority
                .quarantined()
                .iter()
                .any(|(_, path, _)| path == "from-b.txt"),
            "the rejected change must be quarantined, not discarded",
        );
    }

    #[tokio::test]
    async fn default_trusted_peer_still_syncs_both_ways() {
        // With no data grants configured (authority unset on both sides), two
        // trusted peers keep full bidirectional sync — the non-breaking default.
        let (_dir_a, engine_a) = make_engine("shared");
        let (_dir_b, engine_b) = make_engine("shared");

        engine_a.trust(engine_b.device_id().to_string()).await;
        engine_b.trust(engine_a.device_id().to_string()).await;

        let (_cancel_tx_b, cancel_rx_b) = tokio::sync::watch::channel(false);
        let (addr_b, _b_task) = engine_b
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_b)
            .await
            .unwrap();
        engine_a
            .connect_to(Peer {
                device_id: engine_b.device_id().to_string(),
                address: addr_b,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;

        let entry = IndexEntry {
            path: "hello.txt".to_string(),
            is_dir: false,
            size: 11,
            modified: 1_700_000_000,
            block_hashes: vec![0u8; 32],
            deleted: false,
            row_version: 0,
            version: Vec::new(),
        };
        engine_a.index.upsert(&entry).unwrap();
        engine_a.broadcast_update(&entry).await;

        for _ in 0..40 {
            if engine_b.index.get("hello.txt").unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("default trusted peer did not receive the update");
    }
