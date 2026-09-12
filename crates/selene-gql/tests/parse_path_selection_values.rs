//! Counted-group token boundaries must admit a following path-value binding.

use selene_gql::{
    ast::{format_read_statement, structurally_eq},
    parse,
};

#[test]
fn counted_groups_before_named_paths_round_trip_without_keyword_drift() {
    for prefix in [
        "SHORTEST GROUP",
        "SHORTEST 2 GROUPS",
        "SHORTEST 2 WALK PATHS GROUPS",
    ] {
        for name in ["p", "`path`", "grouping"] {
            let source = format!("MATCH {prefix} {name} = (a)-[r{{0,2}}]->(b) RETURN {name}");
            let parsed = parse(&source).unwrap_or_else(|e| panic!("{source}: {e}"));
            let rendered = format_read_statement(&parsed).unwrap();
            assert!(structurally_eq(&parsed, &parse(&rendered).unwrap()));
        }
    }
    for prefix in [
        "SHORTEST 2 GROUPSp",
        "SHORTEST 2 GROUP_",
        "SHORTEST 2 GROUP2",
    ] {
        assert!(parse(&format!("MATCH {prefix} (a) RETURN a")).is_err());
    }
}
