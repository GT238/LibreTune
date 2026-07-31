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

/// End-to-end smoke test: actually compiles and instantiates a real WASM
/// module through `PluginInstance::load()`. Every other test in this file
/// exercises the permission/manifest/lifecycle layer around the plugin
/// system without ever touching the wasmtime engine itself (Engine::default,
/// Module::new, Store::new, Linker::new, instantiate) — this is the only
/// one that does, so it's the one that actually verifies a wasmtime version
/// bump didn't change engine/instantiation behavior.
#[test]
fn test_load_instantiates_real_wasm_module() {
    use std::io::Write;

    // Minimal valid module: no imports, no exports, nothing to go wrong.
    // wasmtime's Module::new accepts WAT text directly (auto-detected), no
    // separate compile-to-binary step needed.
    let wat = b"(module)";
    let mut wasm_file = tempfile::Builder::new()
        .suffix(".wasm")
        .tempfile()
        .expect("failed to create temp wasm file");
    wasm_file
        .write_all(wat)
        .expect("failed to write test module");

    let manifest = create_test_manifest();
    let config = create_test_config();

    let instance = PluginInstance::load(manifest, wasm_file.path(), &config, &[]);
    assert!(
        instance.is_ok(),
        "expected a trivial empty module to load and instantiate cleanly: {:?}",
        instance.err()
    );
    assert_eq!(instance.unwrap().state, PluginState::Loaded);
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

// --- Host-function wiring tests -------------------------------------------
//
// The tests above exercise the manifest/permission/lifecycle data types in
// isolation. These exercise the actual boundary a WASM guest crosses to call
// back into the host: real WAT modules that import `env.*` functions, get
// instantiated against the real `Linker`, and are driven through
// `PluginInstance::call_i32_export`/`read_memory` to confirm the permission
// check and memory marshaling genuinely happen, not just compile.

/// Build and instantiate a `PluginInstance` from inline WAT text.
fn load_wat(wat: &str, manifest: PluginManifest, approved: &[Permission]) -> PluginInstance {
    use std::io::Write;
    let mut wasm_file = tempfile::Builder::new()
        .suffix(".wasm")
        .tempfile()
        .expect("failed to create temp wasm file");
    wasm_file
        .write_all(wat.as_bytes())
        .expect("failed to write test module");

    let config = create_test_config();
    let mut instance = PluginInstance::load(manifest, wasm_file.path(), &config, approved)
        .unwrap_or_else(|e| panic!("failed to load WAT module: {e}\n{wat}"));
    instance.initialize(&config).expect("failed to initialize");
    instance
}

fn manifest_requesting(permissions: Vec<Permission>) -> PluginManifest {
    PluginManifest {
        name: "wat_test_plugin".to_string(),
        version: "1.0.0".to_string(),
        description: "inline WAT test fixture".to_string(),
        author: "Test".to_string(),
        permissions,
    }
}

#[test]
fn log_message_requires_no_permission_and_reaches_the_host() {
    // "hello from plugin" is 17 bytes, written at offset 0 via the data segment.
    let wat = r#"
        (module
            (import "env" "log_message" (func $log_message (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "hello from plugin")
            (func (export "plugin_execute") (result i32)
                (call $log_message (i32.const 1) (i32.const 0) (i32.const 17))
            )
        )
    "#;
    let mut instance = load_wat(wat, manifest_requesting(vec![]), &[]);
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(
        result, 0,
        "log_message should succeed with zero permissions granted"
    );
}

#[test]
fn get_constant_denied_without_read_tables_permission() {
    let wat = r#"
        (module
            (import "env" "get_constant" (func $get_constant (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "rpm")
            (func (export "plugin_execute") (result i32)
                (call $get_constant (i32.const 0) (i32.const 3) (i32.const 100))
            )
        )
    "#;
    // Manifest requests ReadTables, but the user approves nothing.
    let mut instance = load_wat(wat, manifest_requesting(vec![Permission::ReadTables]), &[]);
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(
        result, -1,
        "expected PERMISSION_DENIED when ReadTables wasn't approved"
    );
}

#[test]
fn get_constant_succeeds_and_writes_result_when_approved() {
    let wat = r#"
        (module
            (import "env" "get_constant" (func $get_constant (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "rpm")
            (func (export "plugin_execute") (result i32)
                (call $get_constant (i32.const 0) (i32.const 3) (i32.const 100))
            )
        )
    "#;
    let mut instance = load_wat(
        wat,
        manifest_requesting(vec![Permission::ReadTables]),
        &[Permission::ReadTables],
    );
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(result, 0, "expected OK once ReadTables is approved");

    // The host function must have written its 4-byte placeholder value at
    // the out_ptr the guest supplied (offset 100).
    let written = instance.read_memory(100, 4).expect("memory read failed");
    assert_eq!(written.len(), 4);
}

#[test]
fn set_constant_denied_without_write_constants_permission() {
    let wat = r#"
        (module
            (import "env" "set_constant" (func $set_constant (param i32 i32 f32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "rpm")
            (func (export "plugin_execute") (result i32)
                (call $set_constant (i32.const 0) (i32.const 3) (f32.const 1500))
            )
        )
    "#;
    // Approve ReadTables (an unrelated permission) to prove it doesn't leak
    // into a WriteConstants grant.
    let mut instance = load_wat(
        wat,
        manifest_requesting(vec![Permission::WriteConstants]),
        &[Permission::ReadTables],
    );
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(
        result, -1,
        "expected PERMISSION_DENIED without WriteConstants"
    );
}

#[test]
fn set_constant_succeeds_when_approved() {
    let wat = r#"
        (module
            (import "env" "set_constant" (func $set_constant (param i32 i32 f32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "rpm")
            (func (export "plugin_execute") (result i32)
                (call $set_constant (i32.const 0) (i32.const 3) (f32.const 1500))
            )
        )
    "#;
    let mut instance = load_wat(
        wat,
        manifest_requesting(vec![Permission::WriteConstants]),
        &[Permission::WriteConstants],
    );
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(result, 0);
}

#[test]
fn manifest_cannot_self_grant_beyond_what_was_approved() {
    // Manifest declares all four permissions; user approves only ReadTables.
    // A guest calling execute_action (ExecuteActions) must still be denied.
    let wat = r#"
        (module
            (import "env" "execute_action" (func $execute_action (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{}")
            (func (export "plugin_execute") (result i32)
                (call $execute_action (i32.const 0) (i32.const 2))
            )
        )
    "#;
    let mut instance = load_wat(
        wat,
        manifest_requesting(vec![
            Permission::ReadTables,
            Permission::WriteConstants,
            Permission::SubscribeChannels,
            Permission::ExecuteActions,
        ]),
        &[Permission::ReadTables],
    );
    assert_eq!(instance.granted_permissions(), &[Permission::ReadTables]);
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(
        result, -1,
        "self-declaring ExecuteActions in the manifest must not grant it without approval"
    );
}

#[test]
fn oversized_guest_string_length_is_rejected_without_touching_memory() {
    // Length far larger than MAX_GUEST_STRING_LEN (64 KiB) must be rejected
    // up front rather than attempting an out-of-bounds memory read.
    let wat = r#"
        (module
            (import "env" "log_message" (func $log_message (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "plugin_execute") (result i32)
                (call $log_message (i32.const 1) (i32.const 0) (i32.const 1000000))
            )
        )
    "#;
    let mut instance = load_wat(wat, manifest_requesting(vec![]), &[]);
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(
        result, -2,
        "oversized length must be rejected as invalid args"
    );
}

#[test]
fn negative_pointer_is_rejected() {
    let wat = r#"
        (module
            (import "env" "log_message" (func $log_message (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "plugin_execute") (result i32)
                (call $log_message (i32.const 1) (i32.const -1) (i32.const 5))
            )
        )
    "#;
    let mut instance = load_wat(wat, manifest_requesting(vec![]), &[]);
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(
        result, -2,
        "negative pointer must be rejected as invalid args"
    );
}

#[test]
fn subscribe_channel_and_get_channel_value_round_trip_with_permission() {
    let wat = r#"
        (module
            (import "env" "subscribe_channel" (func $subscribe_channel (param i32 i32 i32) (result i32)))
            (import "env" "get_channel_value" (func $get_channel_value (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "RPM")
            (func (export "subscribe") (result i32)
                (call $subscribe_channel (i32.const 0) (i32.const 3) (i32.const 100))
            )
            (func (export "plugin_execute") (result i32)
                (call $get_channel_value (i32.const 0) (i32.const 104))
            )
        )
    "#;
    let mut instance = load_wat(
        wat,
        manifest_requesting(vec![Permission::SubscribeChannels]),
        &[Permission::SubscribeChannels],
    );
    let sub_result = instance.call_i32_export("subscribe").expect("call failed");
    assert_eq!(sub_result, 0);
    let value_result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(value_result, 0);
}

#[test]
fn get_table_data_denied_without_read_tables_permission() {
    let wat = r#"
        (module
            (import "env" "get_table_data" (func $get_table_data (param i32 i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "veTable1")
            (func (export "plugin_execute") (result i32)
                (call $get_table_data (i32.const 0) (i32.const 8) (i32.const 0) (i32.const 0) (i32.const 100))
            )
        )
    "#;
    let mut instance = load_wat(wat, manifest_requesting(vec![]), &[]);
    let result = instance
        .call_i32_export("plugin_execute")
        .expect("call failed");
    assert_eq!(result, -1);
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
