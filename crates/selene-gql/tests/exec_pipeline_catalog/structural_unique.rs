//! Structural UNIQUE identity at the query-to-graph commit boundary.
use super::{empty_closed_graph, planned, run_write};
use selene_core::{LabelSet, PropertyMap, Value};
use selene_graph::{GraphError, TypeViolation};

#[test]
fn unique_records_share_numeric_and_field_name_identity() {
    let graph = empty_closed_graph(3799);
    for source in [
        "CREATE NODE TYPE :Item (key :: RECORD UNIQUE)",
        "INSERT (:Item { key: { a: 1, b: [2] } }) FINISH",
    ] {
        run_write(&graph, &planned(source)).unwrap().1.unwrap();
    }
    let error = run_write(
        &graph,
        &planned("INSERT (:Item { key: { b: [2.0], a: 1.0 } }) FINISH"),
    )
    .unwrap()
    .1
    .unwrap_err();
    assert!(matches!(
        error,
        GraphError::TypeViolation(TypeViolation::UniquePropertyDuplicate { .. })
    ));
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn unique_durations_share_unit_group_identity() {
    let graph = empty_closed_graph(3800);
    for source in [
        "CREATE NODE TYPE :Item (key :: DURATION (DAY TO SECOND) UNIQUE)",
        "INSERT (:Item { key: DURATION 'PT1H' }) FINISH",
    ] {
        run_write(&graph, &planned(source)).unwrap().1.unwrap();
    }
    let error = run_write(
        &graph,
        &planned("INSERT (:Item { key: DURATION 'PT60M' }) FINISH"),
    )
    .unwrap()
    .1
    .unwrap_err();
    assert!(matches!(
        error,
        GraphError::TypeViolation(TypeViolation::UniquePropertyDuplicate { .. })
    ));
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn unique_zoned_values_share_instant_identity_across_zones() {
    let utc: jiff::Zoned = "2026-09-10T16:00:00Z[UTC]".parse().unwrap();
    let local = utc.with_time_zone(jiff::tz::TimeZone::get("America/New_York").unwrap());
    assert_eq!(utc, local);
    assert_ne!(utc.to_string(), local.to_string());
    for (ty, values) in [
        (
            "ZONED DATETIME",
            [
                Value::ZonedDateTime(Box::new(utc.clone())),
                Value::ZonedDateTime(Box::new(local.clone())),
            ],
        ),
        (
            "ZONED TIME",
            [
                Value::ZonedTime(Box::new(utc.clone())),
                Value::ZonedTime(Box::new(local.clone())),
            ],
        ),
    ] {
        let graph = empty_closed_graph(3801);
        run_write(
            &graph,
            &planned(&format!("CREATE NODE TYPE :Item (key :: {ty} UNIQUE)")),
        )
        .unwrap()
        .1
        .unwrap();
        for (index, value) in values.into_iter().enumerate() {
            let mut properties = PropertyMap::new();
            properties.set(super::db_string("key"), value).unwrap();
            let mut tx = graph.begin_write();
            tx.mutator()
                .create_node(LabelSet::single(super::db_string("Item")), properties)
                .unwrap();
            if index == 0 {
                tx.commit().unwrap();
            } else {
                assert!(
                    matches!(
                        tx.commit(),
                        Err(GraphError::TypeViolation(
                            TypeViolation::UniquePropertyDuplicate { .. }
                        ))
                    ),
                    "{ty}"
                );
            }
        }
        assert_eq!(graph.read().node_count(), 1);
    }
}
