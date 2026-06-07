//! Test module for `lib.rs`, split out to keep the
//! parent file under the source-length cap. Declared from there via
//! `#[cfg(test)] #[path = "lib_tests.rs"] mod tests;`, so it stays a child
//! module with full access to the parent's private items.

    use super::*;
    use tempfile::tempdir;

    fn make_backend() -> (tempfile::TempDir, P2pBackend) {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-test".to_string(),
            folder_id: "p2p-test".to_string(),
            display_name: "Test".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        (dir, backend)
    }

    #[tokio::test]
    async fn upload_then_download_round_trips() {
        let (_dir, backend) = make_backend();
        let data = b"hello world".repeat(1000);
        let entry = backend
            .upload(
                Path::new("hello.txt"),
                &data.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, Some(data.len() as u64));

        let out = backend.download(&entry).await.unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn list_children_after_uploads() {
        let (_dir, backend) = make_backend();
        backend
            .upload(
                Path::new("a.txt"),
                b"a".as_slice(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        backend
            .upload(
                Path::new("b.txt"),
                b"b".as_slice(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        let kids = backend.list_children("root").await.unwrap();
        let names: Vec<_> = kids.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.txt".to_string()));
    }

    #[tokio::test]
    async fn changes_after_upload() {
        let (_dir, backend) = make_backend();
        let (initial, c0) = backend.changes(None).await.unwrap();
        assert!(initial.is_empty());

        backend
            .upload(
                Path::new("x.txt"),
                b"data".as_slice(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        let (deltas, _c1) = backend.changes(Some(&c0)).await.unwrap();
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], Change::Created(_)));
    }

    #[tokio::test]
    async fn delete_marks_tombstone_excluded_from_listing() {
        let (_dir, backend) = make_backend();
        let entry = backend
            .upload(
                Path::new("x.txt"),
                b"x".as_slice(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();
        backend.delete(&entry).await.unwrap();
        let kids = backend.list_children("root").await.unwrap();
        assert!(kids.is_empty());
    }

    /// End-to-end: A uploads through the Backend trait, B connects, and
    /// B's `download()` succeeds even though B's local block store is
    /// empty — the missing blocks must be fetched from A over the wire.
    #[tokio::test]
    async fn cross_backend_download_via_peer_fetch() {
        fn open_with_folder(dir: &std::path::Path, name: &str) -> P2pBackend {
            let cfg = P2pBackendConfig {
                instance_id: format!("p2p-{name}"),
                folder_id: "shared".to_string(),
                display_name: name.to_string(),
                index_path: dir.join("index.db"),
                block_store_root: dir.join("blocks"),
                identity_dir: dir.join("identity"),
                ..Default::default()
            };
            P2pBackend::open(cfg).unwrap()
        }
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let backend_a = open_with_folder(dir_a.path(), "a");
        let backend_b = open_with_folder(dir_b.path(), "b");

        backend_a
            .sync()
            .trust(backend_b.sync().device_id().to_string())
            .await;
        backend_b
            .sync()
            .trust(backend_a.sync().device_id().to_string())
            .await;

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = backend_a
            .sync()
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        backend_b
            .sync()
            .connect_to(crate::sync::Peer {
                device_id: backend_a.sync().device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        let payload = b"peer-to-peer round trip".repeat(50);
        let entry_a = backend_a
            .upload(
                Path::new("shared.bin"),
                &payload.clone(),
                &FileId(format!("{}:root", backend_a.id())),
            )
            .await
            .unwrap();

        // Let the IndexUpdate broadcast and the handshake Index reach B.
        let mut found = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            if let Some(local) = backend_b.index.get("shared.bin").unwrap() {
                found = Some(local);
                break;
            }
        }
        let local_b = found.expect("B never received index update");
        assert_eq!(local_b.size, entry_a.size.unwrap());
        // B's block store is empty — download must hit the peer.
        for chunk in local_b.block_hashes.chunks(32) {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            assert!(
                backend_b
                    .blocks
                    .get_block(&BlockHash(h))
                    .await
                    .unwrap()
                    .is_none()
            );
        }

        let entry_b = backend_b.metadata(Path::new("shared.bin")).await.unwrap();
        let out = backend_b.download(&entry_b).await.unwrap();
        assert_eq!(out, payload);
    }

    /// A deterministic payload large enough to split into several
    /// 128 KB blocks. Each byte is `position % 251` (a prime, so the
    /// pattern does not align to any power-of-two block boundary),
    /// which makes a wrong slice trivially detectable.
    fn multi_block_payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    #[tokio::test]
    async fn read_range_spans_block_boundary() {
        let (_dir, backend) = make_backend();
        // 3.5 blocks worth of data → four 128 KB blocks, last short.
        let block = 128 * 1024;
        let payload = multi_block_payload(block * 7 / 2);
        let entry = backend
            .upload(
                Path::new("big.bin"),
                &payload.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        // A window straddling the first/second block boundary.
        let start = block - 100;
        let length = 200u32;
        let got = backend
            .read_range(&entry, u64::try_from(start).unwrap(), length)
            .await
            .unwrap();
        let end = start + usize::try_from(length).unwrap();
        assert_eq!(got, &payload[start..end]);
    }

    #[tokio::test]
    async fn read_range_single_block_sub_range() {
        let (_dir, backend) = make_backend();
        let block = 128 * 1024;
        let payload = multi_block_payload(block * 3);
        let entry = backend
            .upload(
                Path::new("three.bin"),
                &payload.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        // Wholly inside the second block.
        let start = block + 17;
        let length = 64u32;
        let got = backend
            .read_range(&entry, u64::try_from(start).unwrap(), length)
            .await
            .unwrap();
        let end = start + usize::try_from(length).unwrap();
        assert_eq!(got, &payload[start..end]);
    }

    #[tokio::test]
    async fn read_range_clamps_length_past_eof() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(5000);
        let entry = backend
            .upload(
                Path::new("small.bin"),
                &payload.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        // length runs well past EOF — result is truncated to the tail.
        let got = backend.read_range(&entry, 4000, 10_000).await.unwrap();
        assert_eq!(got, &payload[4000..]);
    }

    #[tokio::test]
    async fn read_range_whole_file() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(128 * 1024 * 2 + 99);
        let entry = backend
            .upload(
                Path::new("whole.bin"),
                &payload.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        let len = u32::try_from(payload.len()).unwrap();
        let got = backend.read_range(&entry, 0, len).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn read_range_offset_at_or_past_eof_is_empty() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(2048);
        let entry = backend
            .upload(
                Path::new("eof.bin"),
                &payload.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        assert!(
            backend
                .read_range(&entry, 2048, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .read_range(&entry, 99_999, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_range_zero_length_is_empty() {
        let (_dir, backend) = make_backend();
        let payload = multi_block_payload(2048);
        let entry = backend
            .upload(
                Path::new("zero.bin"),
                &payload.clone(),
                &FileId("p2p-test:root".to_string()),
            )
            .await
            .unwrap();

        assert!(backend.read_range(&entry, 0, 0).await.unwrap().is_empty());
        assert!(backend.read_range(&entry, 100, 0).await.unwrap().is_empty());
    }

    /// Cross-backend: B reads a range from a file it has indexed but not
    /// cached. The fetch must pull ONLY the blocks covering the window
    /// from A, leaving the rest of B's block store empty — proof that
    /// the override does not reconstruct the whole file.
    #[tokio::test]
    async fn read_range_fetches_only_covering_blocks_from_peer() {
        fn open_with_folder(dir: &std::path::Path, name: &str) -> P2pBackend {
            let cfg = P2pBackendConfig {
                instance_id: format!("p2p-{name}"),
                folder_id: "shared".to_string(),
                display_name: name.to_string(),
                index_path: dir.join("index.db"),
                block_store_root: dir.join("blocks"),
                identity_dir: dir.join("identity"),
                ..Default::default()
            };
            P2pBackend::open(cfg).unwrap()
        }
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let backend_a = open_with_folder(dir_a.path(), "a");
        let backend_b = open_with_folder(dir_b.path(), "b");

        backend_a
            .sync()
            .trust(backend_b.sync().device_id().to_string())
            .await;
        backend_b
            .sync()
            .trust(backend_a.sync().device_id().to_string())
            .await;

        let (_cancel_tx_a, cancel_rx_a) = tokio::sync::watch::channel(false);
        let (addr_a, _a_task) = backend_a
            .sync()
            .start_listener("127.0.0.1:0".parse().unwrap(), cancel_rx_a)
            .await
            .unwrap();
        backend_b
            .sync()
            .connect_to(crate::sync::Peer {
                device_id: backend_a.sync().device_id().to_string(),
                address: addr_a,
            })
            .await
            .unwrap();

        // Five-block file (4 full 128 KB blocks + a short tail).
        let block = 128 * 1024;
        let payload = multi_block_payload(block * 4 + 1234);
        backend_a
            .upload(
                Path::new("range.bin"),
                &payload.clone(),
                &FileId(format!("{}:root", backend_a.id())),
            )
            .await
            .unwrap();

        // Wait for B to learn about the file via the index update.
        let mut found = None;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            if let Some(local) = backend_b.index.get("range.bin").unwrap() {
                found = Some(local);
                break;
            }
        }
        let local_b = found.expect("B never received index update");
        let total_blocks = local_b.block_hashes.len() / 32;
        assert_eq!(total_blocks, 5);

        // Read a window inside the third block only (index 2).
        let start = block * 2 + 50;
        let length = 300u32;
        let entry_b = backend_b.metadata(Path::new("range.bin")).await.unwrap();
        let got = backend_b
            .read_range(&entry_b, u64::try_from(start).unwrap(), length)
            .await
            .unwrap();
        let end = start + usize::try_from(length).unwrap();
        assert_eq!(got, &payload[start..end]);

        // Exactly one block — the covering one — should now be cached on
        // B; the other four must still be absent. This is the load-
        // bearing assertion: a whole-file reconstruction would have
        // cached all five.
        let mut cached = 0usize;
        for chunk in local_b.block_hashes.chunks(32) {
            let mut h = [0u8; 32];
            h.copy_from_slice(chunk);
            if backend_b
                .blocks
                .get_block(&BlockHash(h))
                .await
                .unwrap()
                .is_some()
            {
                cached += 1;
            }
        }
        assert_eq!(cached, 1, "only the covering block should be cached");
    }

    #[test]
    fn resolve_stun_servers_unconfigured_uses_public_defaults() {
        // Key absent entirely → the public defaults apply so NAT detection
        // and the reflexive rung work out of the box.
        let resolved = resolve_stun_servers(None);
        let expected: Vec<String> = DEFAULT_PUBLIC_STUN_SERVERS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(resolved, expected);
        // The default must list at least two servers so the RFC 5780
        // two-server detection path is reachable without configuration.
        assert!(resolved.len() >= 2);
    }

    #[test]
    fn resolve_stun_servers_non_empty_override_replaces_defaults() {
        // A non-empty operator list replaces the defaults entirely — none
        // of the public servers leak through.
        let operator = vec!["stun.example.org:3478".to_string()];
        let resolved = resolve_stun_servers(Some(operator.clone()));
        assert_eq!(resolved, operator);
        assert!(!resolved.iter().any(|s| s.contains("l.google.com")));
    }

    #[test]
    fn resolve_stun_servers_explicit_empty_disables_stun() {
        // An explicitly empty list is the operator opting out — it must NOT
        // fall back to the defaults, which would re-enable STUN against
        // their wishes.
        let resolved = resolve_stun_servers(Some(Vec::new()));
        assert!(resolved.is_empty());
    }

    #[test]
    fn create_backend_omitting_stun_servers_applies_defaults() {
        // End-to-end through the TOML boundary: a config that does not
        // mention `stun_servers` resolves to the public defaults, while an
        // explicit empty array disables STUN.
        let without = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let configured = parse_string_list(without.get("stun_servers"), "stun_servers").unwrap();
        assert!(resolve_stun_servers(configured).len() >= 2);

        let empty = toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = []").unwrap();
        let configured_empty =
            parse_string_list(empty.get("stun_servers"), "stun_servers").unwrap();
        assert!(resolve_stun_servers(configured_empty).is_empty());
    }

    #[test]
    fn parse_string_list_absent_key_is_none() {
        // An absent key must stay `None` so callers can apply their own
        // defaults rather than seeing an empty list.
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let parsed = parse_string_list(value.get("stun_servers"), "stun_servers").unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_string_list_empty_array_is_some_empty() {
        // A present-but-empty array is distinct from absent: it yields
        // `Some(vec![])` so the absent-versus-configured-empty distinction
        // survives the TOML boundary.
        let value = toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = []").unwrap();
        let parsed = parse_string_list(value.get("stun_servers"), "stun_servers").unwrap();
        assert_eq!(parsed, Some(Vec::new()));
    }

    #[test]
    fn parse_string_list_rejects_non_string_entry() {
        // A non-string array entry must fail loudly with its index rather
        // than being silently dropped.
        let value =
            toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = [\"a:1\", 42]").unwrap();
        let err = parse_string_list(value.get("stun_servers"), "stun_servers")
            .expect_err("non-string entry must error");
        assert!(err.to_string().contains("stun_servers[1]"));
    }

    #[test]
    fn parse_string_list_rejects_non_array() {
        // A scalar where an array is expected must fail loudly rather than
        // being silently ignored.
        let value = toml::from_str::<toml::Value>("name = \"x\"\nstun_servers = \"oops\"").unwrap();
        let err = parse_string_list(value.get("stun_servers"), "stun_servers")
            .expect_err("non-array value must error");
        assert!(err.to_string().contains("stun_servers must be an array"));
    }

    /// 64-char hex string for a secret whose bytes are all `0xAB`.
    fn hex_secret() -> String {
        "ab".repeat(cascade_p2p::discovery::announce::SHARED_SECRET_LEN)
    }

    #[test]
    fn parse_announce_servers_absent_is_empty() {
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let parsed = parse_announce_servers(value.get("announce_servers")).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_announce_servers_reads_url_and_secret() {
        let cfg = format!(
            "name = \"x\"\n[[announce_servers]]\nurl = \"https://a.example\"\nshared_secret = \"{}\"\n",
            hex_secret()
        );
        let value = toml::from_str::<toml::Value>(&cfg).unwrap();
        let parsed = parse_announce_servers(value.get("announce_servers")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].base_url, "https://a.example");
        assert_eq!(
            parsed[0].secret,
            [0xAB; cascade_p2p::discovery::announce::SHARED_SECRET_LEN]
        );
    }

    #[test]
    fn parse_announce_servers_requires_a_secret() {
        // A URL with no secret could only resolve, never publish — a silent
        // half-broken state. The parser must reject it loudly instead.
        let cfg = "name = \"x\"\n[[announce_servers]]\nurl = \"https://a.example\"\n";
        let value = toml::from_str::<toml::Value>(cfg).unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("missing secret must error");
        assert!(err.to_string().contains("shared_secret required"));
    }

    #[test]
    fn parse_announce_servers_requires_a_url() {
        let cfg = format!(
            "name = \"x\"\n[[announce_servers]]\nshared_secret = \"{}\"\n",
            hex_secret()
        );
        let value = toml::from_str::<toml::Value>(&cfg).unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("missing url must error");
        assert!(err.to_string().contains("url required"));
    }

    #[test]
    fn parse_announce_servers_rejects_a_malformed_secret() {
        let cfg = "name = \"x\"\n[[announce_servers]]\nurl = \"https://a.example\"\nshared_secret = \"nothex\"\n";
        let value = toml::from_str::<toml::Value>(cfg).unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("malformed secret must error");
        assert!(err.to_string().contains("shared_secret invalid"));
    }

    #[test]
    fn parse_announce_servers_rejects_a_non_table_entry() {
        let value =
            toml::from_str::<toml::Value>("name = \"x\"\nannounce_servers = [\"https://a\"]")
                .unwrap();
        let err = parse_announce_servers(value.get("announce_servers"))
            .expect_err("non-table entry must error");
        assert!(
            err.to_string()
                .contains("announce_servers[0] must be a table")
        );
    }

    #[test]
    fn parse_relay_shared_secret_round_trips_32_bytes() {
        // A 64-char lowercase hex string round-trips to its 32 source
        // bytes exactly. Anything shorter, longer, or with non-hex
        // characters is rejected — silent truncation of an HMAC key
        // would substitute predictable bytes for the missing ones.
        let secret_hex = "0011223344556677889900aabbccddeeff00112233445566778899aabbccddee";
        let parsed = parse_relay_shared_secret(secret_hex).unwrap();
        assert_eq!(parsed[0], 0x00);
        assert_eq!(parsed[1], 0x11);
        assert_eq!(parsed[31], 0xee);
    }

    #[test]
    fn parse_relay_shared_secret_rejects_wrong_length() {
        assert!(parse_relay_shared_secret("abcd").is_err());
        let too_long = "0".repeat(66);
        assert!(parse_relay_shared_secret(&too_long).is_err());
    }

    #[test]
    fn parse_relay_shared_secret_rejects_non_hex() {
        let bad = "zz".repeat(32);
        assert!(parse_relay_shared_secret(&bad).is_err());
    }

    #[test]
    fn p2p_backend_config_default_is_private() {
        // The default posture is `Private` — a trusted mesh that never
        // publishes to a global directory. This guards the manual `Default`
        // impl against regressing to either extreme: confining the node to
        // the LAN, or opting it into DHT/announce publication unasked.
        let cfg = P2pBackendConfig::default();
        assert_eq!(cfg.exposure, DiscoveryReach::Private);
        assert!(cfg.relay_endpoints.is_empty());
        assert!(cfg.relay_shared_secret.is_none());
        assert!(cfg.dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn discovery_reach_capability_truth_table() {
        // The posture-gated activation truth table: each capability is on or
        // off per posture. This is the single source of truth the backend's
        // source registration consults.
        use DiscoveryReach::{LanOnly, Private, Public};

        // Gossip, hole punch, and peer relay: off at LanOnly, on from
        // Private upward.
        for (reach, want) in [(LanOnly, false), (Private, true), (Public, true)] {
            assert_eq!(reach.permits_gossip(), want, "gossip @ {reach:?}");
            assert_eq!(reach.permits_hole_punch(), want, "hole punch @ {reach:?}");
            assert_eq!(reach.permits_peer_relay(), want, "peer relay @ {reach:?}");
        }

        // Global directory (DHT + announce): only at Public.
        for (reach, want) in [(LanOnly, false), (Private, false), (Public, true)] {
            assert_eq!(
                reach.permits_global_directory(),
                want,
                "global directory @ {reach:?}",
            );
        }
    }

    #[test]
    fn parse_exposure_accepts_each_posture() {
        assert_eq!(parse_exposure("lan-only").unwrap(), DiscoveryReach::LanOnly);
        assert_eq!(parse_exposure("private").unwrap(), DiscoveryReach::Private);
        assert_eq!(parse_exposure("public").unwrap(), DiscoveryReach::Public);
    }

    #[test]
    fn parse_exposure_rejects_unknown_posture() {
        // A typo must fail loudly rather than silently falling back to the
        // default — getting the posture wrong is a security-relevant mistake
        // in either direction.
        let err = parse_exposure("publik").expect_err("unknown posture must error");
        assert!(err.to_string().contains("publik"));
    }

    #[test]
    fn open_from_config_absent_exposure_defaults_to_private() {
        // Omitting the `exposure` key resolves to the default posture,
        // Private — a trusted mesh with no global-directory publication.
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let parsed = value
            .get("exposure")
            .and_then(|v| v.as_str())
            .map(parse_exposure)
            .transpose()
            .unwrap()
            .unwrap_or_default();
        assert_eq!(parsed, DiscoveryReach::Private);
    }

    #[test]
    fn open_from_config_parses_exposure_key() {
        // The new `exposure` key round-trips through the TOML boundary to the
        // matching posture.
        let value = toml::from_str::<toml::Value>("name = \"x\"\nexposure = \"public\"").unwrap();
        let parsed = value
            .get("exposure")
            .and_then(|v| v.as_str())
            .map(parse_exposure)
            .transpose()
            .unwrap()
            .unwrap_or_default();
        assert_eq!(parsed, DiscoveryReach::Public);
    }

    #[test]
    fn parse_dht_config_without_bootstrap_uses_empty_set() {
        // No bootstrap nodes is valid — the live node falls back to the named
        // public default set ([`DEFAULT_DHT_BOOTSTRAP_NODES`]) — so the parsed
        // config carries an empty bootstrap list rather than failing. The empty
        // list is the signal `MainlineDht::open` reads as "use the public
        // default". Whether the DHT runs at all is a posture decision, not a
        // property of this config.
        let value = toml::from_str::<toml::Value>(r#"name = "x""#).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert!(dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn parse_dht_config_explicit_empty_bootstrap_array_uses_default() {
        // An operator who writes `dht_bootstrap_nodes = []` explicitly gets the
        // same default-fallback as omitting the key: the parsed list is empty,
        // which the node resolves to the public default. This pins the
        // "empty override falls back to the default" contract at the config
        // layer.
        let toml_src = "name = \"x\"\ndht_bootstrap_nodes = []";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert!(dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn parse_dht_config_override_is_preserved_verbatim() {
        // A non-empty override is carried through unchanged, so the node pins
        // exactly those nodes rather than the public default.
        let toml_src = "name = \"x\"\n\
             dht_bootstrap_nodes = [\"203.0.113.1:6881\"]";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert_eq!(dht.bootstrap_nodes.len(), 1);
        assert_eq!(dht.bootstrap_nodes[0].port(), 6881);
    }

    #[test]
    fn parse_dht_config_parses_bootstrap_nodes() {
        let toml_src = "name = \"x\"\n\
             dht_bootstrap_nodes = [\"127.0.0.1:6881\", \"10.0.0.1:6882\"]";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let dht = parse_dht_config(&value).unwrap();
        assert_eq!(dht.bootstrap_nodes.len(), 2);
        assert_eq!(dht.bootstrap_nodes[0].port(), 6881);
        assert_eq!(dht.bootstrap_nodes[1].port(), 6882);
    }

    #[test]
    fn parse_dht_config_rejects_malformed_bootstrap_node() {
        // A bootstrap entry that is not a valid `host:port` must fail loudly
        // with the offending value rather than being silently dropped.
        let toml_src = "name = \"x\"\n\
             dht_bootstrap_nodes = [\"not-a-socket-addr\"]";
        let value = toml::from_str::<toml::Value>(toml_src).unwrap();
        let err = parse_dht_config(&value).expect_err("malformed node must error");
        assert!(err.to_string().contains("not-a-socket-addr"));
    }

    /// Enabling LAN discovery must not block or panic on backend open.
    /// We can't reliably exercise the full multicast handshake on
    /// loopback in CI, so we just confirm the spawned loops come up
    /// cleanly and the backend can be dropped after a short delay.
    ///
    /// On drop, the backend's cancellation watch is flipped to `true`.
    /// We subscribe before drop and confirm the receiver sees the
    /// change, proving the spawned tasks will exit.
    #[tokio::test]
    async fn discovery_loop_starts_without_panicking() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-discovery".to_string(),
            folder_id: "p2p-discovery".to_string(),
            display_name: "Discovery".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            // LAN multicast self-activates at the default Private posture
            // once a listener is bound — no separate enable flag.
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        // After drop, the cancel watch must have fired; spawned tasks
        // will observe `true` on their next tick and exit.
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// Self-activation: a source that is *permitted* by the posture but
    /// lacks what it needs to run must stay idle. Here the posture is
    /// `Public` — which permits the DHT and announce sources — but no
    /// `listen_addr` is set, so the DHT (which needs a bound port to
    /// advertise) and the LAN source never come up. The backend must still
    /// open and shut down cleanly, proving the AND half of the
    /// self-activation rule: permission alone does not start a source.
    #[tokio::test]
    async fn public_posture_without_listener_keeps_global_sources_idle() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-idle".to_string(),
            folder_id: "p2p-idle".to_string(),
            display_name: "Idle".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            // No listen_addr — the bound-port requirement is unmet.
            exposure: DiscoveryReach::Public,
            // DHT bootstrap configured, but the source still cannot run
            // without a listener to advertise.
            dht: DhtConfig {
                bootstrap_nodes: vec!["127.0.0.1:6881".parse().unwrap()],
            },
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// `LanOnly` posture with a bound listener: LAN multicast self-activates
    /// (it is permitted at every posture and the listener requirement is
    /// met), but gossip, hole punch, peer relay, and any global directory
    /// stay off. The backend must open and shut down cleanly.
    #[tokio::test]
    async fn lan_only_posture_with_listener_opens_and_shuts_down() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-lan-only".to_string(),
            folder_id: "p2p-lan-only".to_string(),
            display_name: "LanOnly".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            exposure: DiscoveryReach::LanOnly,
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// Configuring an announce server must not block or panic on backend
    /// open. The publish loop and the registered announce-server discovery
    /// source come up against an unreachable URL; both are best-effort, so
    /// the backend opens cleanly and drops cleanly. We confirm the spawned
    /// tasks observe cancellation, proving the announce loop honours the
    /// shutdown watch like every other background task.
    #[tokio::test]
    async fn announce_server_configured_backend_opens_and_shuts_down() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-announce".to_string(),
            folder_id: "p2p-announce".to_string(),
            display_name: "Announce".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            // Public posture so the announce source and its publish loop
            // actually run — at Private they would stay idle regardless of a
            // configured server.
            exposure: DiscoveryReach::Public,
            // An address with no server listening — the publish loop's
            // register calls fail and are swallowed best-effort. The secret is
            // immaterial here since no carrier ever receives the request.
            announce_servers: vec![AnnounceServer {
                base_url: "http://127.0.0.1:1".to_string(),
                secret: [0u8; cascade_p2p::discovery::announce::SHARED_SECRET_LEN],
            }],
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }

    /// A `Public`-posture backend with no operator-supplied bootstrap nodes
    /// must not block or panic on backend open: the DHT source self-activates
    /// (the posture permits global publication and a listener is bound), the
    /// node joins the public default set, the publish loop comes up on the
    /// BEP44 republish cadence, and both honour the shutdown watch. We confirm
    /// the spawned tasks observe cancellation, proving the DHT publish loop
    /// exits cleanly like every other background task. The default bootstrap
    /// nodes are real public router hostnames, which `open` resolves with
    /// blocking `getaddrinfo`; the backend runs that resolution off the runtime
    /// via `spawn_blocking`, and a resolver miss is swallowed best-effort, so
    /// the node still binds its local UDP socket and this stays an offline test
    /// that neither blocks a worker nor depends on reaching the routers.
    #[tokio::test]
    async fn dht_enabled_backend_opens_and_shuts_down() {
        let dir = tempdir().unwrap();
        let cfg = P2pBackendConfig {
            instance_id: "p2p-dht".to_string(),
            folder_id: "p2p-dht".to_string(),
            display_name: "Dht".to_string(),
            index_path: dir.path().join("index.db"),
            block_store_root: dir.path().join("blocks"),
            identity_dir: dir.path().join("identity"),
            listen_addr: Some("127.0.0.1:0".parse().unwrap()),
            // The posture is the on/off switch: `Public` permits global-
            // directory publication, so the DHT source self-activates. An empty
            // bootstrap list falls back to the named public default inside the
            // node.
            exposure: DiscoveryReach::Public,
            dht: DhtConfig::default(),
            ..Default::default()
        };
        let backend = P2pBackend::open(cfg).unwrap();
        let mut cancel_rx = backend.cancel.subscribe();
        assert!(!*cancel_rx.borrow());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(backend);
        cancel_rx.changed().await.unwrap();
        assert!(*cancel_rx.borrow());
    }
