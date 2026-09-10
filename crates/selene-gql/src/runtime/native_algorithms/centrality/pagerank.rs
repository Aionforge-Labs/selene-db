//! Runtime binding for `algo.pagerank`.

use std::collections::BTreeSet;

use selene_algorithms::{
    GraphProjection, PageRankConfig, PageRankOrientation, pagerank_with_checker,
};
use selene_core::{CancellationChecker, DbString, NodeId, Record, Value};
use selene_graph::SeleneGraph;

use crate::procedure_registry::ProcedureError;
use crate::runtime::native_algorithms::args::{
    nullable_db_string, nullable_f64, nullable_option_usize, nullable_usize, required_string,
};
use crate::runtime::native_algorithms::error::{algorithm_aborted, invalid_argument};
use crate::runtime::native_algorithms::meta::parameter;
use crate::runtime::native_algorithms::parallel::parse_parallelism;
use crate::runtime::native_algorithms::state::{AlgorithmCatalogs, with_projection};
use crate::{GqlType, ProcedureDefaultValue, ProcedureParameter, ProcedureResult, RecordType};

use super::pagerank_filter::{
    PageRankEdgeFilter, intersect_result_nodes, nullable_edge_filter, resolve_edge_result_nodes,
};

/// Default damping factor used when the GQL argument is NULL.
pub(super) const DEFAULT_DAMPING: f64 = 0.85;
/// Default maximum iteration count used when the GQL argument is NULL.
pub(super) const DEFAULT_MAX_ITERATIONS: usize = 100;
/// Default convergence tolerance used when the GQL argument is NULL.
pub(super) const DEFAULT_TOLERANCE: f64 = 1e-6;

const PAGERANK_PROC: &str = "algo.pagerank";
const DAMPING_CONVERGENCE_DETAIL: &str = "algo.pagerank damping must be finite and in [0.0, 1.0) so PageRank keeps a positive teleport floor and retains its convergence guarantee";

pub(in crate::runtime::native_algorithms) fn pagerank_signature() -> Vec<ProcedureParameter> {
    vec![
        parameter("projection_name", GqlType::String, false),
        parameter("damping", GqlType::Float, true),
        parameter("max_iterations", GqlType::Integer, true),
        parameter("tolerance", GqlType::Float, true),
        parameter("parallelism", GqlType::Integer, true),
        parameter("orientation", GqlType::String, true)
            .with_default_doc("natural")
            .with_default(ProcedureDefaultValue::String("natural")),
        parameter(
            "personalization",
            GqlType::List(Box::new(GqlType::Record(RecordType::Open))),
            true,
        )
        .with_default_doc("NULL (uniform teleport)")
        .with_default(ProcedureDefaultValue::Null),
        parameter("result_label", GqlType::String, true)
            .with_default_doc("NULL (all projection nodes)")
            .with_default(ProcedureDefaultValue::Null),
        parameter("limit", GqlType::Integer, true)
            .with_default_doc("NULL (all matching nodes)")
            .with_default(ProcedureDefaultValue::Null),
        parameter(
            "result_nodes",
            GqlType::List(Box::new(GqlType::NodeRef)),
            true,
        )
        .with_default_doc("NULL (all matching nodes)")
        .with_default(ProcedureDefaultValue::Null),
        parameter("edge_filter_label", GqlType::String, true)
            .with_default_doc("NULL (no edge filter)")
            .with_default(ProcedureDefaultValue::Null),
        parameter("edge_filter_property", GqlType::String, true)
            .with_default_doc("NULL (no edge filter)")
            .with_default(ProcedureDefaultValue::Null),
        parameter(
            "edge_filter_values",
            GqlType::List(Box::new(GqlType::AnyProperty)),
            true,
        )
        .with_default_doc("NULL (no edge filter)")
        .with_default(ProcedureDefaultValue::Null),
        parameter("edge_filter_endpoint", GqlType::String, true)
            .with_default_doc("NULL (no edge filter)")
            .with_default(ProcedureDefaultValue::Null),
    ]
}

pub(in crate::runtime::native_algorithms) fn pagerank(
    catalogs: &AlgorithmCatalogs,
    snapshot: &SeleneGraph,
    args: &[Value],
    checker: CancellationChecker<'_>,
) -> Result<ProcedureResult, ProcedureError> {
    let ParsedPageRankArgs {
        projection_name,
        config,
        mut result_options,
        edge_filter,
    } = parse_pagerank_args(args)?;
    crate::runtime::reference_access::require_live_nodes(
        snapshot,
        config
            .personalization
            .iter()
            .flatten()
            .map(|(node, _)| node)
            .chain(result_options.result_nodes.iter().flatten()),
    )?;
    if let Some(filter) = &edge_filter {
        let edge_nodes = resolve_edge_result_nodes(snapshot, filter)?;
        result_options.result_nodes =
            intersect_result_nodes(result_options.result_nodes.take(), edge_nodes);
    }
    with_projection(catalogs, snapshot, &projection_name, |projection| {
        validate_personalization_nodes(projection, config.personalization.as_deref())?;
        let scores =
            pagerank_with_checker(projection, config, checker).map_err(algorithm_aborted)?;
        let rows = pagerank_rows(snapshot, scores, &result_options);
        Ok(ProcedureResult { rows })
    })
}

#[derive(Debug)]
struct ParsedPageRankArgs {
    projection_name: String,
    config: PageRankConfig,
    result_options: PageRankResultOptions,
    edge_filter: Option<PageRankEdgeFilter>,
}

#[derive(Debug, Default)]
struct PageRankResultOptions {
    result_label: Option<DbString>,
    limit: Option<usize>,
    result_nodes: Option<BTreeSet<NodeId>>,
}

fn parse_pagerank_args(args: &[Value]) -> Result<ParsedPageRankArgs, ProcedureError> {
    if !(5..=10).contains(&args.len()) && args.len() != 14 {
        return Err(invalid_argument(format!(
            "{PAGERANK_PROC} expected 5 to 10 arguments, or 14 with edge filter arguments, got {}",
            args.len()
        )));
    }
    let projection_name = required_string(PAGERANK_PROC, args, 0, "projection_name")?;
    let damping = nullable_f64(PAGERANK_PROC, args, 1, "damping", DEFAULT_DAMPING)?;
    let max_iter = nullable_usize(
        PAGERANK_PROC,
        args,
        2,
        "max_iterations",
        DEFAULT_MAX_ITERATIONS,
    )?;
    let tolerance = nullable_f64(PAGERANK_PROC, args, 3, "tolerance", DEFAULT_TOLERANCE)?;
    let parallelism = parse_parallelism(PAGERANK_PROC, &args[4])?;
    let orientation = if args.len() >= 6 {
        nullable_orientation(&args[5])?
    } else {
        PageRankOrientation::Natural
    };
    let personalization = if args.len() >= 7 {
        nullable_personalization(&args[6])?
    } else {
        None
    };
    let result_label = if args.len() >= 8 {
        nullable_db_string(PAGERANK_PROC, args, 7, "result_label")?
    } else {
        None
    };
    let limit = if args.len() >= 9 {
        nullable_option_usize(PAGERANK_PROC, args, 8, "limit")?
    } else {
        None
    };
    let result_nodes = if args.len() >= 10 {
        nullable_result_nodes(&args[9])?
    } else {
        None
    };
    let edge_filter = nullable_edge_filter(args)?;
    validate_config(damping, tolerance)?;
    Ok(ParsedPageRankArgs {
        projection_name,
        config: PageRankConfig {
            damping,
            max_iter,
            tolerance,
            parallelism,
            orientation,
            personalization,
        },
        result_options: PageRankResultOptions {
            result_label,
            limit,
            result_nodes,
        },
        edge_filter,
    })
}

fn pagerank_rows(
    snapshot: &SeleneGraph,
    mut scores: Vec<(NodeId, f64)>,
    options: &PageRankResultOptions,
) -> Vec<Vec<Value>> {
    if let Some(label) = &options.result_label {
        scores.retain(|(node, _)| {
            snapshot
                .node_labels(*node)
                .is_some_and(|labels| labels.contains(label))
        });
    }
    if let Some(result_nodes) = &options.result_nodes {
        scores.retain(|(node, _)| result_nodes.contains(node));
    }
    if let Some(limit) = options.limit {
        scores.truncate(limit);
    }
    scores
        .into_iter()
        .map(|(node_id, score)| vec![Value::NodeRef(node_id), Value::Float(score)])
        .collect()
}

fn validate_config(damping: f64, tolerance: f64) -> Result<(), ProcedureError> {
    if !damping.is_finite() || !(0.0..1.0).contains(&damping) {
        return Err(invalid_argument(DAMPING_CONVERGENCE_DETAIL));
    }
    if !tolerance.is_finite() {
        return Err(invalid_argument("algo.pagerank tolerance must be finite"));
    }
    if tolerance < 0.0 {
        return Err(invalid_argument(
            "algo.pagerank tolerance must be non-negative",
        ));
    }
    Ok(())
}

fn nullable_orientation(value: &Value) -> Result<PageRankOrientation, ProcedureError> {
    let Value::String(value) = value else {
        return match value {
            Value::Null => Ok(PageRankOrientation::Natural),
            other => Err(invalid_argument(format!(
                "{PAGERANK_PROC} expected orientation to be STRING or NULL, got {other:?}"
            ))),
        };
    };
    match value.as_str().to_ascii_lowercase().as_str() {
        "natural" => Ok(PageRankOrientation::Natural),
        "reverse" => Ok(PageRankOrientation::Reverse),
        "undirected" => Ok(PageRankOrientation::Undirected),
        other => Err(invalid_argument(format!(
            "{PAGERANK_PROC} orientation must be NATURAL, REVERSE, or UNDIRECTED; got {other:?}"
        ))),
    }
}

fn nullable_personalization(value: &Value) -> Result<Option<Vec<(NodeId, f64)>>, ProcedureError> {
    let Value::List(entries) = value else {
        return match value {
            Value::Null => Ok(None),
            other => Err(invalid_argument(format!(
                "{PAGERANK_PROC} expected personalization to be LIST<RECORD> or NULL, got {other:?}"
            ))),
        };
    };
    let mut seeds = Vec::with_capacity(entries.len());
    let mut total = 0.0;
    for (index, entry) in entries.iter().enumerate() {
        let (node, weight) = personalization_entry(entry, index)?;
        if !weight.is_finite() {
            return Err(invalid_argument(format!(
                "{PAGERANK_PROC} personalization[{index}].weight must be finite"
            )));
        }
        if weight < 0.0 {
            return Err(invalid_argument(format!(
                "{PAGERANK_PROC} personalization[{index}].weight must be non-negative"
            )));
        }
        total += weight;
        seeds.push((node, weight));
    }
    if seeds.is_empty() || total <= 0.0 {
        return Err(invalid_argument(format!(
            "{PAGERANK_PROC} personalization must include at least one positive weight"
        )));
    }
    if !total.is_finite() {
        return Err(invalid_argument(format!(
            "{PAGERANK_PROC} personalization total weight must be finite"
        )));
    }
    Ok(Some(seeds))
}

fn nullable_result_nodes(value: &Value) -> Result<Option<BTreeSet<NodeId>>, ProcedureError> {
    let Value::List(values) = value else {
        return match value {
            Value::Null => Ok(None),
            other => Err(invalid_argument(format!(
                "{PAGERANK_PROC} result_nodes must be LIST<NODE> or NULL, got {other:?}"
            ))),
        };
    };
    let mut nodes = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let Value::NodeRef(node) = value else {
            return Err(invalid_argument(format!(
                "{PAGERANK_PROC} result_nodes[{index}] must be a NODE, got {value:?}"
            )));
        };
        nodes.insert(*node);
    }
    Ok(Some(nodes))
}

fn personalization_entry(value: &Value, index: usize) -> Result<(NodeId, f64), ProcedureError> {
    match value {
        Value::Record(record) => {
            let fields = match record.as_ref() {
                Record::Open(fields) => fields,
                _ => {
                    return Err(invalid_argument(format!(
                        "{PAGERANK_PROC} expected personalization[{index}] to be an open RECORD"
                    )));
                }
            };
            let mut node = None;
            let mut weight = None;
            for (field, value) in fields {
                match field.as_str() {
                    "node" | "node_id" => {
                        if node.replace(node_field(value, index)?).is_some() {
                            return Err(invalid_argument(format!(
                                "{PAGERANK_PROC} personalization[{index}] contains duplicate node field"
                            )));
                        }
                    }
                    "weight" => {
                        if weight.replace(weight_field(value, index)?).is_some() {
                            return Err(invalid_argument(format!(
                                "{PAGERANK_PROC} personalization[{index}] contains duplicate weight field"
                            )));
                        }
                    }
                    other => {
                        return Err(invalid_argument(format!(
                            "{PAGERANK_PROC} personalization[{index}] contains unexpected field '{other}'"
                        )));
                    }
                }
            }
            let node = node.ok_or_else(|| {
                invalid_argument(format!(
                    "{PAGERANK_PROC} personalization[{index}] missing node_id"
                ))
            })?;
            let weight = weight.ok_or_else(|| {
                invalid_argument(format!(
                    "{PAGERANK_PROC} personalization[{index}] missing weight"
                ))
            })?;
            Ok((node, weight))
        }
        Value::List(values) if values.len() == 2 => Ok((
            node_field(&values[0], index)?,
            weight_field(&values[1], index)?,
        )),
        other => Err(invalid_argument(format!(
            "{PAGERANK_PROC} expected personalization[{index}] to be RECORD{{node_id, weight}} or [NODE, weight], got {other:?}"
        ))),
    }
}

fn node_field(value: &Value, index: usize) -> Result<NodeId, ProcedureError> {
    match value {
        Value::NodeRef(node) => Ok(*node),
        other => Err(invalid_argument(format!(
            "{PAGERANK_PROC} personalization[{index}].node_id must be a NODE, got {other:?}"
        ))),
    }
}

fn weight_field(value: &Value, index: usize) -> Result<f64, ProcedureError> {
    match value {
        Value::Float(value) => Ok(*value),
        Value::Float32(value) => Ok(f64::from(*value)),
        Value::Int(value) => Ok(*value as f64),
        Value::Uint(value) => Ok(*value as f64),
        other => Err(invalid_argument(format!(
            "{PAGERANK_PROC} personalization[{index}].weight must be numeric, got {other:?}"
        ))),
    }
}

fn validate_personalization_nodes(
    projection: &GraphProjection,
    personalization: Option<&[(NodeId, f64)]>,
) -> Result<(), ProcedureError> {
    let Some(personalization) = personalization else {
        return Ok(());
    };
    for (node, _) in personalization {
        if !projection.contains(*node) {
            return Err(invalid_argument(format!(
                "{PAGERANK_PROC} personalization seed node {} is not in projection '{}'",
                node.get(),
                projection.name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use selene_core::{NodeId, Record, Value, db_string};
    use smallvec::smallvec;

    use super::*;

    fn projection_name() -> Value {
        Value::String(db_string("p").expect("test string fits DB string cap"))
    }

    fn invalid_argument_detail(err: ProcedureError) -> String {
        let ProcedureError::InvalidArgument { detail } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        detail
    }

    fn seed_record(node: NodeId, weight: Value) -> Value {
        Value::Record(Box::new(Record::Open(smallvec![
            (
                db_string("node_id").expect("test field fits DB string cap"),
                Value::NodeRef(node),
            ),
            (
                db_string("weight").expect("test field fits DB string cap"),
                weight,
            ),
        ])))
    }

    #[test]
    fn null_args_resolve_to_defaults() {
        let parsed = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ])
        .expect("NULL args resolve");
        let config = parsed.config;

        assert_eq!(config.damping, DEFAULT_DAMPING);
        assert_eq!(config.max_iter, DEFAULT_MAX_ITERATIONS);
        assert_eq!(config.tolerance, DEFAULT_TOLERANCE);
        assert_eq!(config.parallelism, selene_algorithms::Parallelism::Auto);
        assert_eq!(config.orientation, PageRankOrientation::Natural);
        assert_eq!(config.personalization, None);
    }

    #[test]
    fn pagerank_orientation_parses_modes() {
        for (source, expected) in [
            ("NATURAL", PageRankOrientation::Natural),
            ("reverse", PageRankOrientation::Reverse),
            ("Undirected", PageRankOrientation::Undirected),
        ] {
            let parsed = parse_pagerank_args(&[
                projection_name(),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::String(db_string(source).expect("test string fits DB string cap")),
            ])
            .expect("orientation parses");
            let config = parsed.config;

            assert_eq!(config.orientation, expected);
            assert_eq!(config.personalization, None);
        }
    }

    #[test]
    fn pagerank_orientation_rejects_unknown_mode() {
        let err = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::String(db_string("sideways").expect("test string fits DB string cap")),
        ])
        .expect_err("unknown orientation rejected");

        let detail = invalid_argument_detail(err);
        assert!(detail.contains("NATURAL"));
        assert!(detail.contains("REVERSE"));
        assert!(detail.contains("UNDIRECTED"));
    }

    #[test]
    fn pagerank_personalization_parses_weighted_records() {
        let parsed = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::List(vec![
                seed_record(NodeId::new(7), Value::Int(2)),
                seed_record(NodeId::new(9), Value::Float(1.5)),
            ]),
        ])
        .expect("weighted personalization records parse");

        assert_eq!(
            parsed.config.personalization,
            Some(vec![(NodeId::new(7), 2.0), (NodeId::new(9), 1.5)])
        );
    }

    #[test]
    fn pagerank_result_options_parse_as_trailing_nullable_args() {
        let parsed = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::String(db_string("Fact").expect("test string fits DB string cap")),
            Value::Int(2),
        ])
        .expect("result options parse");

        assert_eq!(
            parsed.result_options.result_label,
            Some(db_string("Fact").expect("test string fits DB string cap"))
        );
        assert_eq!(parsed.result_options.limit, Some(2));

        let parsed_zero = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int(0),
        ])
        .expect("zero result limit parses");
        assert_eq!(parsed_zero.result_options.result_label, None);
        assert_eq!(parsed_zero.result_options.limit, Some(0));
    }

    #[test]
    fn pagerank_result_options_reject_wrong_types() {
        let label_err = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int(1),
        ])
        .expect_err("non-string result label rejected");
        assert!(invalid_argument_detail(label_err).contains("result_label"));

        let limit_err = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int(-1),
        ])
        .expect_err("negative result limit rejected");
        assert!(invalid_argument_detail(limit_err).contains("limit"));
    }

    #[test]
    fn pagerank_personalization_rejects_negative_weights() {
        let err = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::List(vec![seed_record(NodeId::new(7), Value::Float(-1.0))]),
        ])
        .expect_err("negative personalization weight rejected");

        let detail = invalid_argument_detail(err);
        assert!(detail.contains("personalization[0].weight"));
        assert!(detail.contains("non-negative"));
    }

    #[test]
    fn pagerank_personalization_rejects_zero_total_weight() {
        let err = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::List(vec![seed_record(NodeId::new(7), Value::Float(0.0))]),
        ])
        .expect_err("zero-total personalization rejected");

        let detail = invalid_argument_detail(err);
        assert!(detail.contains("at least one positive weight"));
    }

    #[test]
    fn zero_max_iterations_is_valid() {
        let parsed = parse_pagerank_args(&[
            projection_name(),
            Value::Float(DEFAULT_DAMPING),
            Value::Int(0),
            Value::Float(DEFAULT_TOLERANCE),
            Value::Null,
        ])
        .expect("zero max_iter is accepted");

        assert_eq!(parsed.config.max_iter, 0);
    }

    #[test]
    fn pagerank_rejects_damping_one_with_clear_error() {
        let err = parse_pagerank_args(&[
            projection_name(),
            Value::Float(1.0),
            Value::Null,
            Value::Null,
            Value::Null,
        ])
        .expect_err("damping one rejected");

        let detail = invalid_argument_detail(err);
        assert!(detail.contains("[0.0, 1.0)"));
        assert!(detail.contains("teleport"));
        assert!(detail.contains("convergence guarantee"));
    }

    #[test]
    fn pagerank_rejects_damping_nan_or_inf() {
        for damping in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = parse_pagerank_args(&[
                projection_name(),
                Value::Float(damping),
                Value::Null,
                Value::Null,
                Value::Null,
            ])
            .expect_err("non-finite damping rejected");

            let detail = invalid_argument_detail(err);
            assert!(detail.contains("finite"));
            assert!(detail.contains("[0.0, 1.0)"));
            assert!(detail.contains("convergence guarantee"));
        }
    }

    #[test]
    fn out_of_range_damping_rejected() {
        let err = parse_pagerank_args(&[
            projection_name(),
            Value::Float(1.1),
            Value::Null,
            Value::Null,
            Value::Null,
        ])
        .expect_err("out-of-range damping rejected");

        let detail = invalid_argument_detail(err);
        assert!(detail.contains("[0.0, 1.0)"));
    }

    #[test]
    fn negative_tolerance_rejected() {
        let err = parse_pagerank_args(&[
            projection_name(),
            Value::Null,
            Value::Null,
            Value::Float(-0.1),
            Value::Null,
        ])
        .expect_err("negative tolerance rejected");

        assert!(matches!(err, ProcedureError::InvalidArgument { .. }));
    }
}
