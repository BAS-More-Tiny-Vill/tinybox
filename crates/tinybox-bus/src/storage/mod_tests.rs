//! Tests for `storage_schema` and the v1 to v2 migration steps.

use super::*;
use serde_json::{Value, json};
use tinystoragedrivers_core::CollectionSpec;
use tinystoragedrivers_core::schema::{Item, MigrateOptions, Registry};
use tinystoragedrivers_core::{MemoryStorage, Precondition, Scope, ScopedStorage, StorageBackend};

/// A box as the v1 store wrote it: `BoxInfo` serialized by serde, keyed by id.
fn v1_box() -> Value {
    json!({
        "id": "build-1",
        "state": "Ready",
        "spec": {
            "runner": {"host": "local", "sandbox": "passthrough"},
            "workspace": {"host": "local", "sandbox": "passthrough"},
            "source": {"LocalDir": "/srv/work"},
            "resources": {
                "cpu_millis": 2000,
                "memory_bytes": 2_147_483_648_u64,
                "pids_max": 512,
                "disk_bytes": 8_589_934_592_u64,
            },
            "lifecycle": {"Ephemeral": {"ttl": {"secs": 3600, "nanos": 0}}},
            "network": {"Allowlist": {"hosts": ["example.test"], "ports": [443]}},
            "ports": [{"guest": 80, "host": null}],
            "env": {"KEY": "value"},
        },
        "created_at": {"secs_since_epoch": 1_700_000_000_u64, "nanos_since_epoch": 5_000_000_u32},
    })
}

fn v1_jail_index() -> Value {
    json!({
        "schema_version": 1,
        "records": {
            "j1": {
                "id": "j1",
                "label": "build",
                "dir": "/srv/j1",
                "backend_at_create": "landlock",
                "created_at_unix": 1_700_000_000_u64,
                "updated_at_unix": 1_700_000_060_u64,
                "notes": "keep",
            },
        },
    })
}

async fn v1_storage(seed: &[(&str, &str, Value)]) -> (Registry, MemoryStorage, ScopedStorage) {
    let registry = Registry::compose([storage_schema()]).expect("compose");
    let backend = MemoryStorage::new();
    registry.prepare(&backend).await.expect("prepare");
    let storage = backend.for_scope(&Scope::local()).expect("scope");
    for (collection, id, body) in seed {
        let docs = storage.documents();
        docs.ensure_collection(&CollectionSpec::new(*collection))
            .await
            .expect("legacy collection");
        docs.put(collection, id, body.clone(), Precondition::Absent)
            .await
            .expect("seed");
    }
    (registry, backend, storage)
}

#[test]
fn the_schema_declares_the_box_collections_for_the_module_scope() {
    let schema = storage_schema();
    assert_eq!(schema.owner.as_str(), "box");
    let declared: Vec<_> = schema
        .items
        .iter()
        .map(|item| match item {
            Item::Collection {
                name,
                version,
                scope,
                ..
            } => (name.as_str(), *version, *scope),
            other => panic!("unexpected item {other:?}"),
        })
        .collect();
    assert_eq!(
        declared,
        vec![
            ("box.boxes", 2, ScopeClass::Module),
            ("box.jail_index", 2, ScopeClass::Module),
        ]
    );
}

#[test]
fn the_schema_composes() {
    Registry::compose([storage_schema()]).expect("compose");
}

#[test]
fn the_collection_constants_are_the_declared_names() {
    assert_eq!(BOX_COLLECTION, "box.boxes");
    assert_eq!(JAIL_INDEX_COLLECTION, "box.jail_index");
}

#[tokio::test]
async fn a_v1_box_migrates_to_the_v2_body_under_the_same_id() {
    let (registry, _backend, storage) = v1_storage(&[("tinybox_boxes", "build-1", v1_box())]).await;
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect("migrate");
    let docs = storage.documents();
    let found = docs
        .get("box.boxes", "build-1")
        .await
        .expect("get")
        .expect("migrated");
    assert_eq!(
        found.doc,
        json!({
            "state": "ready",
            "created_ms": 1_700_000_000_005_i64,
            "spec": {
                "runner": {"host": "local", "sandbox": "passthrough"},
                "workspace": {"host": "local", "sandbox": "passthrough"},
                "source": {"kind": "local_dir", "path": "/srv/work"},
                "resources": {
                    "cpu_millis": 2000,
                    "memory_bytes": 2_147_483_648_u64,
                    "pids_max": 512,
                    "disk_bytes": 8_589_934_592_u64,
                },
                "lifecycle": {"kind": "ephemeral", "ttl_ms": 3_600_000},
                "network": {"kind": "allowlist", "hosts": ["example.test"], "ports": [443]},
                "ports": [{"guest": 80, "host": null}],
                "env": {"KEY": "value"},
            },
        })
    );
    assert_eq!(found.meta.schema, 2);
    assert!(
        docs.get("tinybox_boxes", "build-1")
            .await
            .expect("get")
            .is_none(),
        "the v1 row is removed once the step completes"
    );
}

#[tokio::test]
async fn a_v1_box_from_before_ports_and_times_migrates() {
    let mut old = v1_box();
    old["spec"].as_object_mut().expect("spec").remove("ports");
    old.as_object_mut().expect("box").remove("created_at");
    let (registry, _backend, storage) = v1_storage(&[("tinybox_boxes", "build-1", old)]).await;
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect("migrate");
    let found = storage
        .documents()
        .get("box.boxes", "build-1")
        .await
        .expect("get")
        .expect("migrated");
    assert_eq!(found.doc["created_ms"], Value::Null);
    assert_eq!(found.doc["spec"]["ports"], json!([]));
}

#[tokio::test]
async fn a_v1_box_that_does_not_decode_stops_the_step_and_is_kept() {
    let (registry, _backend, storage) = v1_storage(&[(
        "tinybox_boxes",
        "bad",
        json!({"id": "bad", "state": "Levitating"}),
    )])
    .await;
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect_err("undecodable box");
    assert!(
        storage
            .documents()
            .get("tinybox_boxes", "bad")
            .await
            .expect("get")
            .is_some()
    );
}

#[tokio::test]
async fn a_v1_jail_index_migrates_to_millisecond_times() {
    let (registry, _backend, storage) =
        v1_storage(&[("tinybox_jail_index", "main", v1_jail_index())]).await;
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect("migrate");
    let docs = storage.documents();
    let found = docs
        .get("box.jail_index", "main")
        .await
        .expect("get")
        .expect("migrated");
    assert_eq!(
        found.doc,
        json!({
            "records": {
                "j1": {
                    "id": "j1",
                    "label": "build",
                    "dir": "/srv/j1",
                    "backend_at_create": "landlock",
                    "created_ms": 1_700_000_000_000_i64,
                    "updated_ms": 1_700_000_060_000_i64,
                    "notes": "keep",
                },
            },
        })
    );
    assert_eq!(found.meta.schema, 2);
    assert!(
        docs.get("tinybox_jail_index", "main")
            .await
            .expect("get")
            .is_none()
    );
}

#[tokio::test]
async fn migrating_twice_changes_nothing() {
    let (registry, _backend, storage) = v1_storage(&[("tinybox_boxes", "build-1", v1_box())]).await;
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect("first");
    let report = registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect("second");
    assert_eq!(report.ran.len(), 0);
}

#[tokio::test]
async fn a_v1_jail_index_with_a_bad_time_stops_the_step_and_is_kept() {
    let mut bad = v1_jail_index();
    bad["records"]["j1"]["created_at_unix"] = json!("yesterday");
    let (registry, _backend, storage) = v1_storage(&[("tinybox_jail_index", "main", bad)]).await;
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .expect_err("bad time");
    let docs = storage.documents();
    assert!(
        docs.get("tinybox_jail_index", "main")
            .await
            .expect("get")
            .is_some()
    );
    assert!(
        docs.get("box.jail_index", "main")
            .await
            .expect("get")
            .is_none()
    );
}

#[test]
fn a_box_the_model_cannot_name_is_refused_by_the_step() {
    let error = box_to_v2(SourceRecord {
        id: "x".to_owned(),
        body: json!({"id": "x"}),
        meta: tinystoragedrivers_core::Meta::unknown(),
    })
    .expect_err("incomplete box");
    assert_eq!(
        error.kind(),
        tinystoragedrivers_core::ErrorKind::Serialization
    );
}
