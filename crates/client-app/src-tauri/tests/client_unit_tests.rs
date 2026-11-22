use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use duke_link_client::launcher::{
    build_horizon_launch_args, detect_horizon_installs_in, detect_minecraft_installs_in,
    encode_servers_dat_nbt, resolve_horizon_layout, validate_username, write_local_servers_dat,
    LauncherKind,
};
use duke_link_client::settings::{load_profile, save_profile_atomic, PlayerProfile};
use duke_link_client::sync::{
    map_horizon_target_rel_path, plan_manifest_delta, resolve_safe_instance_path, SyncAction,
};
use shared_protocol::config_overlay::BaselineStore;
use shared_protocol::manifest::{FileEntry, FileRole, PackManifest};

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

fn make_temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "duke_link_client_unit_{}_{}_{}",
        std::process::id(),
        seq,
        label
    ));
    let _ = fs::remove_dir_all(&dir);
    assert!(
        fs::create_dir_all(&dir).is_ok(),
        "failed to create temp dir: {}",
        dir.display()
    );
    dir
}

#[test]
fn test_detect_minecraft_prism_and_modrinth_installs() {
    let root = make_temp_dir("detect_installs");
    let app_data = root.join("Roaming");
    let home_dir = root.join("Home");

    // Populate Roaming: .minecraft (with assets), PrismLauncher (no assets), ModrinthApp (with assets)
    let mc_root = app_data.join(".minecraft");
    let mc_assets = mc_root.join("assets");
    let prism_root = app_data.join("PrismLauncher");
    let modrinth_root = app_data.join("ModrinthApp");
    let modrinth_assets = modrinth_root.join("assets");

    fs::create_dir_all(&mc_assets).expect("create .minecraft/assets");
    fs::create_dir_all(&prism_root).expect("create PrismLauncher");
    fs::create_dir_all(&modrinth_assets).expect("create ModrinthApp/assets");

    // Also populate Linux-style home PrismLauncher to verify both roots are scanned.
    let home_theseus = home_dir
        .join(".local")
        .join("share")
        .join("com.modrinth.theseus");
    fs::create_dir_all(&home_theseus).expect("create linux theseus dir");

    let installs = detect_minecraft_installs_in(Some(&app_data), Some(&home_dir));
    assert_eq!(installs.len(), 4);

    let mc = installs
        .iter()
        .find(|i| i.kind == LauncherKind::VanillaMinecraft)
        .expect("vanilla minecraft detected");
    assert_eq!(mc.root_dir, mc_root);
    assert_eq!(mc.assets_dir, Some(mc_assets));

    let prism = installs
        .iter()
        .find(|i| i.kind == LauncherKind::PrismLauncher)
        .expect("prism launcher detected");
    assert_eq!(prism.root_dir, prism_root);
    assert_eq!(prism.assets_dir, None);

    let modrinth_win = installs
        .iter()
        .find(|i| i.root_dir == modrinth_root)
        .expect("windows modrinth detected");
    assert_eq!(modrinth_win.kind, LauncherKind::ModrinthApp);
    assert_eq!(modrinth_win.assets_dir, Some(modrinth_assets));

    let modrinth_linux = installs
        .iter()
        .find(|i| i.root_dir == home_theseus)
        .expect("linux modrinth detected");
    assert_eq!(modrinth_linux.kind, LauncherKind::ModrinthApp);

    // Empty directories return empty vector cleanly.
    let empty_root = make_temp_dir("detect_empty");
    assert!(detect_minecraft_installs_in(Some(&empty_root), None).is_empty());

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&empty_root);
}

#[test]
fn test_username_validation_and_servers_dat_nbt() {
    assert!(validate_username("Duke_01").is_ok());
    assert!(validate_username("abc").is_ok());
    assert!(validate_username("SixteenCharName1").is_ok());

    assert!(validate_username("ab").is_err(), "too short");
    assert!(validate_username("SeventeenCharName").is_err(), "too long");
    assert!(validate_username("bad name").is_err(), "space rejected");
    assert!(validate_username("bad-name!").is_err(), "symbols rejected");

    let nbt = encode_servers_dat_nbt("Duke SMP", "127.0.0.1:25565");
    assert_eq!(nbt.first().copied(), Some(10), "starts with TAG_Compound");
    assert_eq!(nbt.last().copied(), Some(0), "ends with TAG_End");
    assert!(
        nbt.windows(b"127.0.0.1:25565".len())
            .any(|w| w == b"127.0.0.1:25565"),
        "contains local tunnel address"
    );

    let dir = make_temp_dir("servers_dat");
    let written =
        write_local_servers_dat(&dir, "Duke SMP", "127.0.0.1:25565").expect("write servers.dat");
    assert_eq!(written, dir.join("servers.dat"));
    assert_eq!(fs::read(&written).expect("read servers.dat"), nbt);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_blake3_manifest_delta_planning() {
    let instance_dir = make_temp_dir("delta_plan");
    fs::create_dir_all(instance_dir.join("mods")).expect("create mods dir");
    fs::create_dir_all(instance_dir.join("config")).expect("create config dir");

    // 1. Matching enforced mod -> Skip
    let matching_bytes = b"horizon-core-v1.jar";
    let matching_hash = blake3::hash(matching_bytes).to_hex().to_string();
    fs::write(instance_dir.join("mods/core.jar"), matching_bytes).expect("write matching mod");

    // 2. Tampered enforced mod -> UpdateChanged
    let enforced_server_bytes = b"authoritative-anti-cheat-v2.jar";
    let enforced_server_hash = blake3::hash(enforced_server_bytes).to_hex().to_string();
    fs::write(instance_dir.join("mods/enforced.jar"), b"tampered-local")
        .expect("write tampered mod");

    // 3. Missing new mod -> DownloadNew
    let new_mod_bytes = b"brand-new-mod.jar";
    let new_mod_hash = blake3::hash(new_mod_bytes).to_hex().to_string();

    // 4. Unmodified InitialDefault with newer server version -> UpdateChanged
    let old_default = b"lodDistance=12\n";
    let old_default_hash = blake3::hash(old_default).to_hex().to_string();
    let new_default = b"lodDistance=16\n";
    let new_default_hash = blake3::hash(new_default).to_hex().to_string();
    fs::write(instance_dir.join("config/lod.cfg"), old_default).expect("write old default");

    // 5. User-edited InitialDefault -> PreserveUserModified
    let orig_keys = b"jump=SPACE\n";
    let orig_keys_hash = blake3::hash(orig_keys).to_hex().to_string();
    let server_keys_v2 = b"jump=SPACE\nsprint=LCTRL\n";
    let server_keys_v2_hash = blake3::hash(server_keys_v2).to_hex().to_string();
    fs::write(instance_dir.join("config/keys.cfg"), b"jump=RSHIFT\n").expect("write user keys");

    let mut baseline = BaselineStore::default();
    baseline
        .hashes
        .insert("config/lod.cfg".to_owned(), old_default_hash);
    baseline
        .hashes
        .insert("config/keys.cfg".to_owned(), orig_keys_hash);

    let manifest = PackManifest {
        schema_version: 1,
        pack_name: "horizon-pack".to_owned(),
        pack_version: "2.0.0".to_owned(),
        horizon_release_tag: None,
        horizon_rev: None,
        minecraft_version: None,
        files: vec![
            FileEntry {
                path: "mods/core.jar".to_owned(),
                blake3: matching_hash,
                sha256: None,
                size: matching_bytes.len() as u64,
                role: FileRole::EnforcedServerState,
            },
            FileEntry {
                path: "mods/enforced.jar".to_owned(),
                blake3: enforced_server_hash,
                sha256: None,
                size: enforced_server_bytes.len() as u64,
                role: FileRole::EnforcedServerState,
            },
            FileEntry {
                path: "mods/new.jar".to_owned(),
                blake3: new_mod_hash,
                sha256: None,
                size: new_mod_bytes.len() as u64,
                role: FileRole::EnforcedServerState,
            },
            FileEntry {
                path: "config/lod.cfg".to_owned(),
                blake3: new_default_hash,
                sha256: None,
                size: new_default.len() as u64,
                role: FileRole::InitialDefault,
            },
            FileEntry {
                path: "config/keys.cfg".to_owned(),
                blake3: server_keys_v2_hash,
                sha256: None,
                size: server_keys_v2.len() as u64,
                role: FileRole::InitialDefault,
            },
        ],
    };

    let plan = plan_manifest_delta(&instance_dir, &manifest, &baseline).expect("plan delta");
    assert_eq!(plan.len(), 5);
    assert_eq!(plan[0].action, SyncAction::Skip);
    assert_eq!(plan[1].action, SyncAction::UpdateChanged);
    assert_eq!(plan[2].action, SyncAction::DownloadNew);
    assert_eq!(plan[3].action, SyncAction::UpdateChanged);
    assert_eq!(plan[4].action, SyncAction::PreserveUserModified);

    let _ = fs::remove_dir_all(&instance_dir);
}

#[test]
fn test_resolve_safe_instance_path_blocks_traversal() {
    let instance_dir = make_temp_dir("path_guard");

    let safe = resolve_safe_instance_path(&instance_dir, "mods/horizon.jar")
        .expect("valid relative path should resolve");
    assert!(safe.ends_with(PathBuf::from("mods").join("horizon.jar")));

    assert!(resolve_safe_instance_path(&instance_dir, "../escape.txt").is_err());
    assert!(resolve_safe_instance_path(&instance_dir, "mods/../../escape.txt").is_err());
    assert!(resolve_safe_instance_path(&instance_dir, "/etc/passwd").is_err());
    assert!(resolve_safe_instance_path(&instance_dir, r"mods\backslash.jar").is_err());

    let _ = fs::remove_dir_all(&instance_dir);
}

#[test]
fn test_horizon_path_mapping_profile_toml_and_launch_args() {
    assert_eq!(
        map_horizon_target_rel_path("dukes-core.hmod").expect("core hmod"),
        "horizon-mods/dukes-core.hmod"
    );
    assert_eq!(
        map_horizon_target_rel_path("extra-mods/jade_horizon.hmod").expect("extra hmod"),
        "horizon-mods/jade_horizon.hmod"
    );
    assert_eq!(
        map_horizon_target_rel_path("horizon-mods.txt").expect("mods txt"),
        "horizon-mods.txt"
    );
    assert_eq!(
        map_horizon_target_rel_path("dist/dukes-horizon.hpack").expect("hpack"),
        "dukes-horizon.hpack"
    );

    let data_dir = make_temp_dir("profile_toml");
    let profile = PlayerProfile {
        player_name: "PlayerOne_26".to_owned(),
        horizon_install_dir: Some(data_dir.join("horizon")),
        direct_server_addr: None,
    };
    save_profile_atomic(&data_dir, &profile).expect("save profile.toml");
    let loaded = load_profile(&data_dir).expect("load profile.toml");
    assert_eq!(loaded, profile);

    let install_root = data_dir.join("horizon");
    fs::create_dir_all(install_root.join("resources")).expect("create resources");
    fs::create_dir_all(install_root.join("game").join("horizon-mods")).expect("create game mods");
    let detected = detect_horizon_installs_in(std::slice::from_ref(&install_root));
    assert_eq!(detected.len(), 1);
    let layout = resolve_horizon_layout(&install_root);
    let args = build_horizon_launch_args(
        &layout.resources_dir,
        &layout.game_dir,
        "PlayerOne_26",
        "127.0.0.1:25565",
    );
    assert_eq!(args.len(), 8);
    assert_eq!(args[0], "--resources");
    assert_eq!(args[2], "--game-dir");
    assert_eq!(args[4], "--name");
    assert_eq!(args[5], "PlayerOne_26");
    assert_eq!(args[6], "--server");
    assert_eq!(args[7], "127.0.0.1:25565");

    let _ = fs::remove_dir_all(&data_dir);
}
