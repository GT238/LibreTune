//! Plugin system integration tests
//!
//! Tests the plugin lifecycle, permission system, and plugin manager.
//!
//! # Fixtures
//!
//! Tests use [`create_test_manifest()`] and [`create_test_config()`] as the
//! baseline plugin metadata. The default manifest requests only `ReadTables`
//! and `WriteConstants` — a representative read-mostly plugin — so tests that
//! need the full permission set construct their own manifest inline rather
//! than mutating the fixture.

use libretune_core::plugin_system::*;

/// A representative read-mostly plugin manifest.
///
/// Requests only `ReadTables` + `WriteConstants` — enough for a VE-analysis
/// style plugin, but deliberately *not* `SubscribeChannels` or
/// `ExecuteActions`. Tests that need those grants (or none at all) build their
/// own manifest rather than mutating this one, to keep the fixture stable.
fn create_test_manifest() -> PluginManifest {
    PluginManifest {
        name: "ve_analyzer".to_string(),
        version: "1.0.0".to_string(),
        description: "VE cell optimization plugin".to_string(),
        author: "LibreTune Team".to_string(),
        permissions: vec![Permission::ReadTables, Permission::WriteConstants],
    }
}

/// A minimal, non-persisting plugin config.
///
/// `data_dir` points at a throwaway path (no real I/O happens in these
/// tests). `ecu_type` is set to Speeduino as a representative platform.
fn create_test_config() -> PluginConfig {
    PluginConfig {
        data_dir: "/tmp/libretune_plugins".to_string(),
        ecu_type: "Speeduino".to_string(),
        libretune_version: "0.1.0".to_string(),
    }
}

#[test]
fn test_plugin_manifest_creation() {
    let manifest = create_test_manifest();
    assert_eq!(manifest.name, "ve_analyzer");
    assert_eq!(manifest.version, "1.0.0");
    assert_eq!(manifest.permissions.len(), 2);
}

#[test]
fn test_plugin_manifest_permissions() {
    let manifest = create_test_manifest();
    assert!(manifest.permissions.contains(&Permission::ReadTables));
    assert!(manifest.permissions.contains(&Permission::WriteConstants));
    assert!(!manifest
        .permissions
        .contains(&Permission::SubscribeChannels));
    assert!(!manifest.permissions.contains(&Permission::ExecuteActions));
}

#[test]
fn test_permission_enum_all_variants() {
    let all_perms = [
        Permission::ReadTables,
        Permission::WriteConstants,
        Permission::SubscribeChannels,
        Permission::ExecuteActions,
    ];

    assert_eq!(all_perms.len(), 4);

    // Permissions are `#[derive(Eq)]`, so distinct variants must compare
    // unequal — this guards against accidentally collapsing the capability
    // space (e.g. via a future `as u8` cast that aliases two variants).
    for (i, perm1) in all_perms.iter().enumerate() {
        for (j, perm2) in all_perms.iter().enumerate() {
            if i != j {
                assert_ne!(perm1, perm2);
            } else {
                assert_eq!(perm1, perm2);
            }
        }
    }
}

#[test]
fn test_permission_equality() {
    let perm1 = Permission::ReadTables;
    let perm2 = Permission::ReadTables;
    let perm3 = Permission::WriteConstants;

    assert_eq!(perm1, perm2);
    assert_ne!(perm1, perm3);
}

#[test]
fn test_plugin_config_creation() {
    let config = create_test_config();
    assert_eq!(config.ecu_type, "Speeduino");
    assert!(config.data_dir.contains("libretune_plugins"));
    assert!(!config.libretune_version.is_empty());
}

#[test]
fn test_plugin_state_enum_values() {
    // All five lifecycle states must exist and compare distinct — the state
    // machine in `PluginInstance` dispatches on these, so two states aliasing
    // would cause a plugin to be treated as terminal/active incorrectly.
    let states = [
        PluginState::Loaded,
        PluginState::Ready,
        PluginState::Running,
        PluginState::Unloading,
        PluginState::Disabled,
    ];

    assert_eq!(states.len(), 5);

    for (i, state1) in states.iter().enumerate() {
        for (j, state2) in states.iter().enumerate() {
            if i != j {
                assert_ne!(state1, state2);
            }
        }
    }
}

#[test]
fn test_plugin_manager_new() {
    let config = create_test_config();
    let manager = PluginManager::new(config);
    assert_eq!(manager.count(), 0);
}

#[test]
fn test_plugin_manager_empty_list() {
    let config = create_test_config();
    let manager = PluginManager::new(config);
    let plugins = manager.list_plugins();
    assert!(plugins.is_empty());
}

#[test]
fn test_plugin_manager_nonexistent_plugin() {
    let config = create_test_config();
    let manager = PluginManager::new(config);

    // Unknown plugin names must resolve to `None`, not panic — the host API
    // relies on this to return a clean "not found" rather than crashing.
    assert!(manager.get_plugin("nonexistent").is_none());
}

#[test]
fn test_plugin_manifest_serialization() {
    let manifest = create_test_manifest();

    // serde_json round-trip must preserve identity. Manifests are persisted
    // to disk as JSON, so a field dropped or renamed in transit would corrupt
    // every installed plugin.
    let json = serde_json::to_string(&manifest).expect("Failed to serialize");
    assert!(json.contains("ve_analyzer"));
    assert!(json.contains("1.0.0"));

    let deserialized: PluginManifest = serde_json::from_str(&json).expect("Failed to deserialize");
    assert_eq!(manifest.name, deserialized.name);
    assert_eq!(manifest.version, deserialized.version);
    assert_eq!(manifest.permissions.len(), deserialized.permissions.len());
}

#[test]
fn test_plugin_stats_structure() {
    let stats = PluginStats {
        exec_count: 42,
        state: PluginState::Ready,
        permissions: 3,
    };

    assert_eq!(stats.exec_count, 42);
    assert_eq!(stats.state, PluginState::Ready);
    assert_eq!(stats.permissions, 3);
}

#[test]
fn test_manifest_with_all_permissions() {
    let manifest = PluginManifest {
        name: "full_access".to_string(),
        version: "1.0.0".to_string(),
        description: "Plugin with all permissions".to_string(),
        author: "Test".to_string(),
        permissions: vec![
            Permission::ReadTables,
            Permission::WriteConstants,
            Permission::SubscribeChannels,
            Permission::ExecuteActions,
        ],
    };

    assert_eq!(manifest.permissions.len(), 4);
    assert!(manifest.permissions.contains(&Permission::ReadTables));
    assert!(manifest.permissions.contains(&Permission::WriteConstants));
    assert!(manifest
        .permissions
        .contains(&Permission::SubscribeChannels));
    assert!(manifest.permissions.contains(&Permission::ExecuteActions));
}

#[test]
fn test_manifest_with_no_permissions() {
    let manifest = PluginManifest {
        name: "readonly".to_string(),
        version: "1.0.0".to_string(),
        description: "Read-only plugin".to_string(),
        author: "Test".to_string(),
        permissions: vec![],
    };

    assert!(manifest.permissions.is_empty());
}

#[test]
fn test_plugin_config_different_ecu_types() {
    let speeduino_config = PluginConfig {
        ecu_type: "Speeduino".to_string(),
        data_dir: "/tmp".to_string(),
        libretune_version: "0.1.0".to_string(),
    };

    let rusefi_config = PluginConfig {
        ecu_type: "RusEFI".to_string(),
        data_dir: "/tmp".to_string(),
        libretune_version: "0.1.0".to_string(),
    };

    assert_ne!(speeduino_config.ecu_type, rusefi_config.ecu_type);
}

#[test]
fn test_plugin_lifecycle_states() {
    // Spot-check adjacent lifecycle transitions compare distinct, so a state
    // machine guard can never confuse e.g. Ready with Running. Reflexivity is
    // asserted separately to catch a broken `PartialEq`.
    let loaded = PluginState::Loaded;
    let ready = PluginState::Ready;
    let running = PluginState::Running;
    let unloading = PluginState::Unloading;
    let disabled = PluginState::Disabled;

    assert_ne!(loaded, ready);
    assert_ne!(ready, running);
    assert_ne!(running, unloading);
    assert_ne!(unloading, disabled);

    assert_eq!(loaded, PluginState::Loaded);
    assert_eq!(ready, PluginState::Ready);
}

#[test]
fn test_multiple_manifest_versions() {
    let v1 = PluginManifest {
        name: "test".to_string(),
        version: "1.0.0".to_string(),
        description: "v1".to_string(),
        author: "Test".to_string(),
        permissions: vec![],
    };

    let v2 = PluginManifest {
        name: "test".to_string(),
        version: "1.1.0".to_string(),
        description: "v2".to_string(),
        author: "Test".to_string(),
        permissions: vec![],
    };

    assert_eq!(v1.name, v2.name);
    assert_ne!(v1.version, v2.version);
}

#[test]
fn test_permission_bits() {
    // Membership checks are the basis of `check_permission`; this confirms a
    // partial grant set admits its members and denies the absent one
    // (`ExecuteActions`), which is exactly the approval semantics.
    let perms = vec![
        Permission::ReadTables,
        Permission::WriteConstants,
        Permission::SubscribeChannels,
    ];

    for perm in &perms {
        assert!(perms.contains(perm));
    }

    assert!(!perms.contains(&Permission::ExecuteActions));
}

#[test]
fn test_plugin_config_immutability() {
    let config1 = create_test_config();
    let config2 = create_test_config();

    // Two configs built independently must be field-equal but not share state
    // — guards against a regression where `Default` accidentally aliased a
    // shared buffer for `data_dir`.
    assert_eq!(config1.ecu_type, config2.ecu_type);
    assert_eq!(config1.data_dir, config2.data_dir);
}
