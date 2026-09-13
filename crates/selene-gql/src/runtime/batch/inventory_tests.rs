//! Generated supported value-family inventory through actual batch statements.
//! Structural and non-null value semantics have separate independent fixtures;
//! this guard prevents a selected family from losing its statement entry path.

use super::{BatchPolicy, fixtures::person_graph, query::execute_with_test_policy};
use crate::{EmptyProcedureRegistry, runtime::TxContext};
use selene_core::Value;

#[test]
fn every_generated_supported_value_family_has_a_batch_entry_path() {
    let graph = person_graph();
    let mut checked = 0;
    for capability in selene_profile::capabilities().iter().filter(|c| {
        c.status == selene_profile::CapabilityStatus::Supported && c.id.as_str().starts_with("GV")
    }) {
        let ty = match capability.id.as_str() {
            "GV01" => "UINT8",
            "GV02" => "INT8",
            "GV03" | "GV05" => "UINT16",
            "GV04" | "GV18" => "INT16",
            "GV06" | "GV08" => "UINT32",
            "GV07" => "INT32",
            "GV09" | "GV12" | "GV19" => "INT64",
            "GV10" | "GV11" => "UINT64",
            "GV13" => "UINT128",
            "GV14" => "INT128",
            "GV17" => "DECIMAL(12, 3)",
            "GV21" | "GV22" => "FLOAT32",
            "GV23" => "DOUBLE",
            "GV24" => "FLOAT64",
            "GV30" | "GV31" | "GV32" => "STRING(2, 8)",
            "GV35" => "BYTES",
            "GV36" | "GV37" | "GV38" => "BYTES(2, 8)",
            "GV39" => "LOCAL DATETIME",
            "GV40" => "ZONED DATETIME",
            "GV41" => "DURATION(DAY TO SECOND)",
            "GV45" | "GV47" => "RECORD",
            "GV46" => "RECORD{a :: INT}",
            "GV48" => "RECORD{a :: RECORD}",
            "GV50" => "LIST<INT>",
            "GV55" => "PATH",
            "GV68" => "ANY PROPERTY VALUE",
            "GV90" => "INT NOT NULL",
            other => panic!("selected value family {other} needs a batch entry fixture"),
        };
        let value = if capability.id.as_str() == "GV90" {
            "1"
        } else {
            "NULL"
        };
        let source = format!("RETURN {value} IS TYPED {ty} AS accepted");
        let parsed = crate::parse(&source).unwrap_or_else(|error| panic!("{source}: {error:?}"));
        let analyzed = crate::analyze(parsed, &EmptyProcedureRegistry, None).unwrap();
        let plan = crate::plan(&analyzed, &EmptyProcedureRegistry).unwrap();
        let ctx = TxContext::read_only(
            graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            graph.index_providers(),
        );
        for size in [1, 7, 1024] {
            let table =
                execute_with_test_policy(&plan, &ctx, BatchPolicy::new(size, 4096).unwrap())
                    .unwrap();
            assert_eq!(
                table.rows()[0].values(),
                [Value::Bool(true)],
                "{}",
                capability.id.as_str()
            );
            assert_eq!(
                table.schema().columns[0].name.as_ref().unwrap().as_str(),
                "accepted"
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "empty generated selection is not evidence");
}
