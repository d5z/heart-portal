//! Concurrent access tests for BeingAccessor.
//!
//! Validates that multiple BeingAccessor instances can safely read/write
//! the same .being file concurrently via SQLite WAL mode.

use heart_shared::being::{AccessMode, BeingAccessor};
use heart_shared::reality::{RealityKind, RealityLayer, RealityNode, RealityRealm};

fn make_node(key: &str, value: &str) -> RealityNode {
    let now = chrono::Utc::now().to_rfc3339();
    RealityNode {
        key: key.to_string(),
        value: value.to_string(),
        kind: RealityKind::Fact,
        layer: RealityLayer::Surface,
        confidence: 1.0,
        ttl_secs: None,
        verified_at: now.clone(),
        updated_at: now,
        source: None,
        edges: vec![],
        dim: None,
        river_seq: None,
            realm: RealityRealm::World,
    }
}

/// Two writers (Owner + SenseWriter) alternately writing different reality keys.
#[test]
fn test_two_writers_reality() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two_writers.being");

    let owner = BeingAccessor::open(&path, AccessMode::Owner).unwrap();
    let sense = BeingAccessor::open(&path, AccessMode::SenseWriter).unwrap();

    let owner_reality = owner.reality();
    let sense_reality = sense.reality();

    // Alternate writes from both accessors
    for i in 0..20 {
        let key_o = format!("owner:key:{i}");
        let key_s = format!("sense:key:{i}");
        owner_reality.upsert(&make_node(&key_o, &format!("oval_{i}"))).unwrap();
        sense_reality.upsert(&make_node(&key_s, &format!("sval_{i}"))).unwrap();
    }

    // Both sides can read everything back
    for i in 0..20 {
        let o = owner_reality.get(&format!("owner:key:{i}")).unwrap().unwrap();
        assert_eq!(o.value, format!("oval_{i}"));

        let s = sense_reality.get(&format!("sense:key:{i}")).unwrap().unwrap();
        assert_eq!(s.value, format!("sval_{i}"));

        // Cross-read: sense accessor sees owner's writes
        let cross = sense_reality.get(&format!("owner:key:{i}")).unwrap().unwrap();
        assert_eq!(cross.value, format!("oval_{i}"));
    }
}

/// ReadOnly accessor can read data while Owner is actively writing.
#[test]
fn test_reader_during_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reader_writer.being");

    let owner = BeingAccessor::open(&path, AccessMode::Owner).unwrap();
    let owner_reality = owner.reality();

    // Owner writes initial batch
    for i in 0..10 {
        owner_reality.upsert(&make_node(&format!("init:{i}"), "before")).unwrap();
    }

    // ReadOnly opens — should see the initial data
    let reader = BeingAccessor::open(&path, AccessMode::ReadOnly).unwrap();
    let reader_reality = reader.reality();

    // Owner continues writing
    for i in 10..20 {
        owner_reality.upsert(&make_node(&format!("after:{i}"), "after")).unwrap();
    }

    // Reader can see initial data
    for i in 0..10 {
        let node = reader_reality.get(&format!("init:{i}")).unwrap().unwrap();
        assert_eq!(node.value, "before");
    }

    // Reader can also see newer data (WAL — reads see committed writes)
    for i in 10..20 {
        let node = reader_reality.get(&format!("after:{i}")).unwrap().unwrap();
        assert_eq!(node.value, "after");
    }
}

/// Two SenseWriters doing heavy concurrent writes via threads.
/// busy_timeout(5000ms) should prevent SQLITE_BUSY errors.
#[test]
fn test_busy_timeout_handles_contention() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contention.being");

    // Owner creates the file first
    let owner = BeingAccessor::open(&path, AccessMode::Owner).unwrap();
    drop(owner);

    let path_a = path.clone();
    let path_b = path.clone();

    let handle_a = std::thread::spawn(move || {
        let acc = BeingAccessor::open(&path_a, AccessMode::SenseWriter).unwrap();
        let r = acc.reality();
        for i in 0..100 {
            r.upsert(&make_node(&format!("writer_a:{i}"), &format!("a_{i}")))
                .unwrap();
        }
    });

    let handle_b = std::thread::spawn(move || {
        let acc = BeingAccessor::open(&path_b, AccessMode::SenseWriter).unwrap();
        let r = acc.reality();
        for i in 0..100 {
            r.upsert(&make_node(&format!("writer_b:{i}"), &format!("b_{i}")))
                .unwrap();
        }
    });

    handle_a.join().unwrap();
    handle_b.join().unwrap();

    // Verify all writes landed
    let reader = BeingAccessor::open(&path, AccessMode::ReadOnly).unwrap();
    let r = reader.reality();
    for i in 0..100 {
        let a = r.get(&format!("writer_a:{i}")).unwrap().unwrap();
        assert_eq!(a.value, format!("a_{i}"));
        let b = r.get(&format!("writer_b:{i}")).unwrap().unwrap();
        assert_eq!(b.value, format!("b_{i}"));
    }
}

/// Owner creates and seeds, drops. SenseWriter then writes. ReadOnly reads.
#[test]
fn test_owner_creates_sensewriter_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lifecycle.being");

    // Owner creates and seeds
    let owner = BeingAccessor::open(&path, AccessMode::Owner).unwrap();
    let id = owner.being_id().to_string();
    drop(owner);

    // SenseWriter opens and writes reality
    let sense = BeingAccessor::open(&path, AccessMode::SenseWriter).unwrap();
    assert_eq!(sense.being_id(), id);
    let sr = sense.reality();
    sr.upsert(&make_node("sense:temp", "22C")).unwrap();
    sr.upsert(&make_node("sense:humidity", "45%")).unwrap();
    drop(sense);

    // ReadOnly opens and reads
    let reader = BeingAccessor::open(&path, AccessMode::ReadOnly).unwrap();
    assert_eq!(reader.being_id(), id);
    let rr = reader.reality();
    let temp = rr.get("sense:temp").unwrap().unwrap();
    assert_eq!(temp.value, "22C");
    let hum = rr.get("sense:humidity").unwrap().unwrap();
    assert_eq!(hum.value, "45%");
}
