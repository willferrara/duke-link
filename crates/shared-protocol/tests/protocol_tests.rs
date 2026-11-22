use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use iroh::SecretKey;
use shared_protocol::allowlist::{Allowlist, AllowlistError, InviteTicket, INVITE_TICKET_PREFIX};
use shared_protocol::config_overlay::{
    apply_overlay_file, BaselineStore, MergeDecision, OverlayError,
};
use shared_protocol::manifest::{
    build_horizon_pack_manifest, compute_bytes_sha256, compute_file_blake3, compute_file_sha256,
    read_manifest, validate_relative_path, verify_file_blake3, verify_file_sha256,
    write_manifest_atomic, FileEntry, FileRole, ManifestError, PackManifest, DEFAULT_HORIZON_REV,
};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn unique_test_dir(name: &str) -> PathBuf {
    let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "duke_link_protocol_test_{}_{}_{}",
        std::process::id(),
        seq,
        name
    ));
    let _ = fs::remove_dir_all(&dir);
    assert!(
        fs::create_dir_all(&dir).is_ok(),
        "failed to create unique test dir: {}",
        dir.display()
    );
    dir
}

#[test]
fn test_blake3_match_and_mismatch() {
    let dir = unique_test_dir("blake3");
    let file_path = dir.join("horizon-core.jar");
    let payload = b"horizon-mc-mod-binary-payload-v1";
    fs::write(&file_path, payload).expect("write test payload");

    let expected = blake3::hash(payload).to_hex().to_string();
    let computed = compute_file_blake3(&file_path).expect("compute blake3");
    assert_eq!(computed, expected);

    // Exact lowercase and uppercase hex both verify cleanly.
    verify_file_blake3(&file_path, &expected).expect("lowercase digest should verify");
    verify_file_blake3(&file_path, &expected.to_ascii_uppercase())
        .expect("uppercase digest should verify");

    // Corrupted digest returns typed HashMismatch error.
    let wrong_hex = "0".repeat(64);
    let err = verify_file_blake3(&file_path, &wrong_hex).expect_err("mismatch must error");
    match err {
        ManifestError::HashMismatch {
            path,
            expected: exp,
            actual,
        } => {
            assert_eq!(path, file_path);
            assert_eq!(exp, wrong_hex);
            assert_eq!(actual, expected);
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_atomic_manifest_write_and_backup() {
    let dir = unique_test_dir("atomic_manifest");
    let manifest_path = dir.join("nested").join("manifest.json");

    let v1 = PackManifest {
        schema_version: 1,
        pack_name: "horizon-friends".to_owned(),
        pack_version: "1.0.0".to_owned(),
        horizon_release_tag: None,
        horizon_rev: None,
        minecraft_version: None,
        files: vec![FileEntry {
            path: "mods/core.jar".to_owned(),
            blake3: blake3::hash(b"v1").to_hex().to_string(),
            sha256: None,
            size: 2,
            role: FileRole::EnforcedServerState,
        }],
    };

    write_manifest_atomic(&manifest_path, &v1).expect("initial atomic write");
    let loaded_v1 = read_manifest(&manifest_path).expect("read v1 manifest");
    assert_eq!(loaded_v1, v1);

    let bak_path = PathBuf::from(format!("{}.bak", manifest_path.display()));
    let tmp_path = PathBuf::from(format!("{}.tmp", manifest_path.display()));
    assert!(!bak_path.exists(), "no .bak before overwrite");
    assert!(!tmp_path.exists(), "no leftover .tmp after write");

    let v2 = PackManifest {
        schema_version: 1,
        pack_name: "horizon-friends".to_owned(),
        pack_version: "1.1.0".to_owned(),
        horizon_release_tag: None,
        horizon_rev: None,
        minecraft_version: None,
        files: vec![FileEntry {
            path: "mods/core.jar".to_owned(),
            blake3: blake3::hash(b"v2").to_hex().to_string(),
            sha256: None,
            size: 2,
            role: FileRole::EnforcedServerState,
        }],
    };

    write_manifest_atomic(&manifest_path, &v2).expect("overwrite atomic write");
    let loaded_v2 = read_manifest(&manifest_path).expect("read v2 manifest");
    assert_eq!(loaded_v2, v2);
    assert!(bak_path.exists(), ".bak must exist after overwriting");
    assert!(!tmp_path.exists(), "no leftover .tmp after overwrite");

    let backed_up = read_manifest(&bak_path).expect("read .bak manifest");
    assert_eq!(backed_up, v1, ".bak must contain previous manifest version");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_corrupt_json_manifest_and_baseline() {
    let dir = unique_test_dir("corrupt_json");
    let bad_manifest = dir.join("manifest.json");
    fs::write(&bad_manifest, "{ not valid json !!!").expect("write corrupt manifest");

    let manifest_err = read_manifest(&bad_manifest).expect_err("corrupt manifest must fail");
    assert!(
        matches!(manifest_err, ManifestError::Json { .. }),
        "expected ManifestError::Json, got {manifest_err:?}"
    );

    let bad_baseline = dir.join("baseline.json");
    fs::write(&bad_baseline, "[1, 2, 3").expect("write corrupt baseline");

    let baseline_err = BaselineStore::load(&bad_baseline).expect_err("corrupt baseline must fail");
    assert!(
        matches!(baseline_err, OverlayError::Json { .. }),
        "expected OverlayError::Json, got {baseline_err:?}"
    );

    // Non-existent baseline returns empty default cleanly.
    let missing_baseline = BaselineStore::load(&dir.join("does_not_exist.json"))
        .expect("missing baseline should return default");
    assert!(missing_baseline.hashes.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_three_way_config_merge_preservation_and_enforcement() {
    let dir = unique_test_dir("config_overlay");
    let baseline_path = dir.join("baseline.json");
    let mut baseline = BaselineStore::load(&baseline_path).expect("load empty baseline");

    // 1. InitialDefault: InstallNew -> AlreadyUpToDate -> UpdateUnmodifiedDefault -> PreserveUserModified
    let cfg_path = dir.join("config").join("options.txt");
    let default_v1 = b"fov:70\nrenderDistance:12\n";
    let entry_v1 = FileEntry {
        path: "config/options.txt".to_owned(),
        blake3: blake3::hash(default_v1).to_hex().to_string(),
        sha256: None,
        size: default_v1.len() as u64,
        role: FileRole::InitialDefault,
    };

    let d1 = apply_overlay_file(&cfg_path, &entry_v1, default_v1, &mut baseline)
        .expect("install initial default");
    assert_eq!(d1, MergeDecision::InstallNew);
    assert_eq!(fs::read(&cfg_path).expect("read v1"), default_v1);

    let d2 = apply_overlay_file(&cfg_path, &entry_v1, default_v1, &mut baseline)
        .expect("already up to date");
    assert_eq!(d2, MergeDecision::AlreadyUpToDate);

    // Server updates InitialDefault while user has NOT edited the file -> fast-forward.
    let default_v2 = b"fov:80\nrenderDistance:16\n";
    let entry_v2 = FileEntry {
        path: "config/options.txt".to_owned(),
        blake3: blake3::hash(default_v2).to_hex().to_string(),
        sha256: None,
        size: default_v2.len() as u64,
        role: FileRole::InitialDefault,
    };
    let d3 = apply_overlay_file(&cfg_path, &entry_v2, default_v2, &mut baseline)
        .expect("update unmodified default");
    assert_eq!(d3, MergeDecision::UpdateUnmodifiedDefault);
    assert_eq!(fs::read(&cfg_path).expect("read v2"), default_v2);

    // User customizes options.txt; server pushes v3 -> PreserveUserModified!
    let user_custom = b"fov:105\nrenderDistance:24\n";
    fs::write(&cfg_path, user_custom).expect("write user custom config");
    let default_v3 = b"fov:90\nrenderDistance:18\n";
    let entry_v3 = FileEntry {
        path: "config/options.txt".to_owned(),
        blake3: blake3::hash(default_v3).to_hex().to_string(),
        sha256: None,
        size: default_v3.len() as u64,
        role: FileRole::InitialDefault,
    };
    let d4 = apply_overlay_file(&cfg_path, &entry_v3, default_v3, &mut baseline)
        .expect("preserve user modified");
    assert_eq!(d4, MergeDecision::PreserveUserModified);
    assert_eq!(
        fs::read(&cfg_path).expect("read user custom"),
        user_custom,
        "user edits must remain untouched"
    );

    // 2. EnforcedServerState: local tampering MUST be overwritten by authoritative server bytes.
    let mod_path = dir.join("mods").join("horizon-sync.jar");
    let mod_v1 = b"authoritative-mod-v1";
    let mod_entry = FileEntry {
        path: "mods/horizon-sync.jar".to_owned(),
        blake3: blake3::hash(mod_v1).to_hex().to_string(),
        sha256: Some(compute_bytes_sha256(mod_v1)),
        size: mod_v1.len() as u64,
        role: FileRole::EnforcedServerState,
    };
    let m1 = apply_overlay_file(&mod_path, &mod_entry, mod_v1, &mut baseline)
        .expect("install enforced mod");
    assert_eq!(m1, MergeDecision::InstallNew);

    fs::write(&mod_path, b"tampered-local-bytes").expect("tamper enforced file");
    let m2 = apply_overlay_file(&mod_path, &mod_entry, mod_v1, &mut baseline)
        .expect("overwrite enforced mod");
    assert_eq!(m2, MergeDecision::OverwriteEnforced);
    assert_eq!(fs::read(&mod_path).expect("read restored mod"), mod_v1);

    // 3. Server payload hash mismatch is rejected before writing.
    let bad_err = apply_overlay_file(&mod_path, &mod_entry, b"wrong-server-bytes", &mut baseline);
    // AlreadyUpToDate happens first when local matches, so test against a new path.
    assert!(bad_err.is_ok());
    let fresh_path = dir.join("mods").join("corrupt.jar");
    let mismatch = apply_overlay_file(
        &fresh_path,
        &mod_entry,
        b"wrong-server-bytes",
        &mut baseline,
    )
    .expect_err("server hash mismatch must error");
    assert!(matches!(mismatch, OverlayError::ServerHashMismatch { .. }));

    // 4. BaselineStore atomic save & reload roundtrip.
    baseline
        .save_atomic(&baseline_path)
        .expect("save baseline atomically");
    let reloaded = BaselineStore::load(&baseline_path).expect("reload baseline");
    assert_eq!(reloaded, baseline);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_allowlist_enforcement_and_validation() {
    let dir = unique_test_dir("allowlist");
    let allowed_1 = SecretKey::generate().public();
    let allowed_2 = SecretKey::generate().public();
    let intruder = SecretKey::generate().public();

    let toml_body =
        format!("allowed_endpoints = [\n    \"{allowed_1}\",\n    \"{allowed_2}\",\n]\n");
    let toml_path = dir.join("allowlist.toml");
    fs::write(&toml_path, &toml_body).expect("write allowlist.toml");

    let list = Allowlist::load(&toml_path).expect("load allowlist");
    assert_eq!(list.len(), 2);
    assert!(!list.is_empty());
    assert!(list.is_allowed(&allowed_1));
    assert!(list.is_allowed(&allowed_2));
    assert!(!list.is_allowed(&intruder));

    // Invalid Ed25519 EndpointId must fail with typed error.
    let bad_toml = "allowed_endpoints = [\"not-a-valid-ed25519-key\"]\n";
    let err = Allowlist::from_toml_str(bad_toml).expect_err("invalid EndpointId must fail");
    assert!(
        matches!(err, AllowlistError::InvalidEndpointId { .. }),
        "expected InvalidEndpointId, got {err:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_invite_ticket_codec_and_path_validation() {
    let server_id = SecretKey::generate().public();
    let ticket = InviteTicket {
        server_endpoint_id: server_id,
        server_name: "Duke Horizon SMP".to_owned(),
        relay_url: Some("https://relay.example.invalid/".to_owned()),
    };

    let encoded = ticket.encode().expect("encode invite ticket");
    assert!(encoded.starts_with(INVITE_TICKET_PREFIX));

    let decoded = InviteTicket::decode(&encoded).expect("decode invite ticket");
    assert_eq!(decoded, ticket);

    // Without optional relay_url
    let minimal = InviteTicket {
        server_endpoint_id: server_id,
        server_name: "Minimal Server".to_owned(),
        relay_url: None,
    };
    let min_enc = minimal.encode().expect("encode minimal ticket");
    assert_eq!(
        InviteTicket::decode(&min_enc).expect("decode minimal"),
        minimal
    );

    // Empty server name rejected
    let empty_name = InviteTicket {
        server_endpoint_id: server_id,
        server_name: "   ".to_owned(),
        relay_url: None,
    };
    assert!(matches!(
        empty_name.encode(),
        Err(AllowlistError::EmptyServerName)
    ));

    // Corrupt base64 rejected
    assert!(matches!(
        InviteTicket::decode("duke1:!!!not-base64!!!"),
        Err(AllowlistError::Base64(_))
    ));

    // Relative path traversal validation
    assert!(validate_relative_path("mods/horizon.jar").is_ok());
    assert!(validate_relative_path("../escape.txt").is_err());
    assert!(validate_relative_path("/etc/passwd").is_err());
    assert!(validate_relative_path(r"mods\windows.jar").is_err());
    assert!(validate_relative_path("").is_err());
}

#[test]
fn test_horizon_pack_manifest_builder_and_sha256_verification() {
    let dir = unique_test_dir("horizon_pack_manifest");
    fs::create_dir_all(dir.join("extra-mods")).expect("create extra-mods dir");
    fs::write(dir.join("dukes-horizon.hpack"), b"hpack-archive-bytes").expect("write hpack");
    fs::write(dir.join("dukes-core.hmod"), b"core-hmod-bytes").expect("write core hmod");
    fs::write(dir.join("dukes-content.hmod"), b"content-hmod-bytes").expect("write content hmod");
    fs::write(dir.join("horizon-mods.txt"), b"mod.dukes_core:true\n").expect("write mods txt");
    fs::write(dir.join("extra-mods/jade_horizon.hmod"), b"jade-bytes").expect("write extra hmod");
    fs::write(dir.join("ignored.log"), b"should-be-ignored").expect("write ignored file");

    let manifest =
        build_horizon_pack_manifest(&dir, "dukes-horizon", "0.1.0").expect("build manifest");
    assert_eq!(manifest.horizon_rev.as_deref(), Some(DEFAULT_HORIZON_REV));
    assert_eq!(manifest.files.len(), 5);

    let hpack = manifest
        .files
        .iter()
        .find(|f| f.path == "dukes-horizon.hpack")
        .expect("hpack included");
    let sha = hpack.sha256.as_deref().expect("sha256 computed");
    assert_eq!(
        compute_file_sha256(&dir.join("dukes-horizon.hpack")).expect("file sha256"),
        sha
    );
    verify_file_sha256(&dir.join("dukes-horizon.hpack"), sha).expect("sha256 verifies");

    let _ = fs::remove_dir_all(&dir);
}
