//! Typed range/equality index scan rule.

use crate::{
    BinaryOp, EdgeMatch, IndexTarget, Literal,
    plan::{
        BindingDef, BindingElement, ExecutionPlan, FilterPredicate, IndexKey, JoinTree, ScanAccess,
        ScanKind, TypedIndexBounds,
        optimize::{OptimizeContext, Rule, Transformed, binding_refs, cost, walk},
    },
};

use super::index_helpers::{compatible_value, single_label};

/// Rewrite property equality/range predicates to typed index access.
pub struct RangeIndexScan;

impl Rule for RangeIndexScan {
    fn name(&self) -> &'static str {
        "range_index_scan"
    }

    fn rewrite(
        &self,
        mut plan: ExecutionPlan,
        ctx: &OptimizeContext<'_>,
    ) -> Transformed<ExecutionPlan> {
        let Some(catalog) = ctx.index_catalog else {
            return Transformed::unchanged(plan);
        };
        let mut changed = super::expression_index_scan::rewrite(&mut plan, ctx);
        if let Some(pattern) = &mut plan.pattern_plan {
            changed |= rewrite_tree(&mut pattern.join_tree, &pattern.bindings, catalog);
        }
        let nested = walk::recurse_rule_subplans(plan, self, ctx);
        changed |= nested.changed;
        Transformed {
            plan: nested.plan,
            changed,
        }
    }
}

fn rewrite_tree(
    tree: &mut JoinTree,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
) -> bool {
    match tree {
        JoinTree::Unit | JoinTree::Paths(_) => false,
        JoinTree::Scan(scan) => rewrite_scan(scan, bindings, catalog),
        JoinTree::Expand { child, edge, .. } => {
            rewrite_tree(child, bindings, catalog) | rewrite_edge(edge, bindings, catalog)
        }
        JoinTree::HashJoin { left, right, .. } | JoinTree::Outer { left, right, .. } => {
            rewrite_tree(left, bindings, catalog) | rewrite_tree(right, bindings, catalog)
        }
        JoinTree::WorstCaseOptimal { .. } | JoinTree::Subplan(_) => false,
        // Walk each per-label branch; the disjunctive_label_expansion rule
        // runs at slot 5 (before this rule), so DisjunctiveScan only carries
        // single-label branches by the time we get here.
        JoinTree::DisjunctiveScan { branches, .. } => {
            branches.iter_mut().fold(false, |changed, branch| {
                rewrite_scan(branch, bindings, catalog) | changed
            })
        }
    }
}

fn rewrite_scan(
    scan: &mut crate::NodeOrEdgeScan,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
) -> bool {
    if !matches!(scan.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = single_label(&scan.label_predicate) else {
        return false;
    };
    let target = target_for_scan_kind(scan.kind);
    let Some(candidate) = best_candidate(
        &scan.property_predicates,
        bindings,
        catalog,
        target,
        label.clone(),
    ) else {
        return false;
    };
    // OPT-5 cost gate: take the typed-index probe only when its estimated output
    // is strictly cheaper than the residual baseline (label-scoped row count,
    // else total rows). When stats are absent the gate is a no-op (keeps the
    // structural decision), so the plan matches pre-OPT-5 HEAD. The probe's rows
    // are a subset of the residual evaluation — the executor still applies the
    // label predicate + any residual filters — so results are identical either
    // way.
    if let (Some(index_cost), Some(baseline)) = (
        cost::typed_index_cost(
            catalog,
            target,
            label.clone(),
            candidate.property.clone(),
            &candidate.bounds,
        ),
        cost::linear_baseline(catalog, target, label),
    ) && cost::should_decline_index(index_cost, baseline)
    {
        return false;
    }
    remove_indices(&mut scan.property_predicates, &candidate.consumed_indices);
    scan.access = ScanAccess::TypedIndexRange {
        handle: candidate.handle,
        property: candidate.property,
        kind: candidate.kind,
        bounds: candidate.bounds,
    };
    true
}

fn rewrite_edge(
    edge: &mut EdgeMatch,
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
) -> bool {
    if !matches!(edge.access, ScanAccess::Linear) {
        return false;
    }
    let Some(label) = single_label(&edge.label_predicate) else {
        return false;
    };
    let Some(candidate) = best_candidate(
        &edge.property_predicates,
        bindings,
        catalog,
        IndexTarget::Edge,
        label.clone(),
    ) else {
        return false;
    };
    if let (Some(index_cost), Some(baseline)) = (
        cost::typed_index_cost(
            catalog,
            IndexTarget::Edge,
            label.clone(),
            candidate.property.clone(),
            &candidate.bounds,
        ),
        cost::linear_baseline(catalog, IndexTarget::Edge, label),
    ) && cost::should_decline_index(index_cost, baseline)
    {
        return false;
    }
    remove_indices(&mut edge.property_predicates, &candidate.consumed_indices);
    edge.access = ScanAccess::TypedIndexRange {
        handle: candidate.handle,
        property: candidate.property,
        kind: candidate.kind,
        bounds: candidate.bounds,
    };
    true
}

struct Candidate {
    handle: crate::IndexHandle,
    property: selene_core::DbString,
    kind: crate::IndexKind,
    bounds: TypedIndexBounds,
    consumed_indices: Vec<usize>,
}

fn best_candidate(
    predicates: &[FilterPredicate],
    bindings: &[BindingDef],
    catalog: &dyn crate::IndexCatalog,
    target: IndexTarget,
    label: selene_core::DbString,
) -> Option<Candidate> {
    for (index, pred) in predicates.iter().enumerate() {
        let Some(matched) = binding_refs::match_property_predicate(pred, bindings) else {
            continue;
        };
        if !binding_is_target(bindings, matched.binding, target) {
            continue;
        }
        let Some(lookup) = catalog.typed_index(target, label.clone(), matched.key.clone()) else {
            continue;
        };
        let Some((bounds, mut consumed_indices)) = bounds_for_property(
            matched.key.clone(),
            predicates,
            bindings,
            index,
            lookup.kind,
        ) else {
            continue;
        };
        consumed_indices.sort_unstable();
        consumed_indices.dedup();
        return Some(Candidate {
            handle: lookup.handle,
            property: matched.key,
            kind: lookup.kind,
            bounds,
            consumed_indices,
        });
    }
    None
}

fn bounds_for_property(
    key: selene_core::DbString,
    predicates: &[FilterPredicate],
    bindings: &[BindingDef],
    first_index: usize,
    kind: crate::IndexKind,
) -> Option<(TypedIndexBounds, Vec<usize>)> {
    let mut equality: Option<IndexKey> = None;
    let mut lower: Option<(IndexKey, bool)> = None;
    let mut upper: Option<(IndexKey, bool)> = None;
    let mut consumed = Vec::new();

    for (index, pred) in predicates.iter().enumerate().skip(first_index) {
        let Some(matched) = binding_refs::match_property_predicate(pred, bindings) else {
            continue;
        };
        if matched.key != key {
            continue;
        }
        match matched.shape {
            binding_refs::PropertyPredicateShape::Equality(value) => {
                let key = compatible_value(value, kind)?;
                equality = Some(key);
                // Equality is the tighter probe — promote it to the index
                // bound, but leave any previously-accumulated range bounds
                // (`> 10` etc.) as residual predicates the executor still
                // enforces. Pre-PR-#175 this `consumed` carried the prior
                // range-bound indices alongside the equality, silently
                // dropping the range filter when the equality fired second
                // (Codex PR #175 F3). The bug pre-existed for literal
                // equality too; parameter equality made it observable.
                consumed = vec![index];
                lower = None;
                upper = None;
                break;
            }
            binding_refs::PropertyPredicateShape::Comparison { op, value } => {
                let key = compatible_value(value, kind)?;
                let candidate = (key, matches!(op, BinaryOp::Ge | BinaryOp::Le));
                let outcome = match op {
                    BinaryOp::Gt | BinaryOp::Ge => tighten_lower(lower.take(), candidate),
                    BinaryOp::Lt | BinaryOp::Le => tighten_upper(upper.take(), candidate),
                    _ => continue,
                };
                match outcome {
                    TightenOutcome::Bound(bound) => {
                        match op {
                            BinaryOp::Gt | BinaryOp::Ge => lower = Some(bound),
                            BinaryOp::Lt | BinaryOp::Le => upper = Some(bound),
                            _ => unreachable!("guarded above"),
                        }
                        consumed.push(index);
                    }
                    TightenOutcome::KeepExisting(existing) => {
                        match op {
                            BinaryOp::Gt | BinaryOp::Ge => lower = Some(existing),
                            BinaryOp::Lt | BinaryOp::Le => upper = Some(existing),
                            _ => unreachable!("guarded above"),
                        }
                        // Don't consume the candidate predicate; leave it as
                        // residual so the executor still enforces it. The
                        // index probe uses the existing bound.
                    }
                    TightenOutcome::Reject => return None,
                }
            }
            binding_refs::PropertyPredicateShape::InList(_)
            | binding_refs::PropertyPredicateShape::InListExpression(_) => {}
        }
    }

    if let Some(key) = equality {
        return Some((TypedIndexBounds::Equality(key), consumed));
    }
    match (lower, upper) {
        (Some((lo, lo_inclusive)), Some((hi, hi_inclusive))) => {
            // Contradictory bounds (lo > hi, or lo == hi with both exclusive) make the
            // range empty; refuse the rewrite when both sides are concrete literals so
            // the executor evaluates the predicates linearly and we don't paper over a
            // possibly-buggy executor range path. Parameter-bearing ranges defer the
            // satisfiability check to runtime.
            if let (IndexKey::Literal(lo_lit), IndexKey::Literal(hi_lit)) = (&lo, &hi)
                && !range_satisfiable(lo_lit, lo_inclusive, hi_lit, hi_inclusive)
            {
                return None;
            }
            Some((
                TypedIndexBounds::Range {
                    lo,
                    lo_inclusive,
                    hi,
                    hi_inclusive,
                },
                consumed,
            ))
        }
        (Some((key, false)), None) => Some((TypedIndexBounds::GreaterThan(key), consumed)),
        (Some((key, true)), None) => Some((TypedIndexBounds::GreaterEqual(key), consumed)),
        (None, Some((key, false))) => Some((TypedIndexBounds::LessThan(key), consumed)),
        (None, Some((key, true))) => Some((TypedIndexBounds::LessEqual(key), consumed)),
        (None, None) => None,
    }
}

/// Outcome of folding a new range bound against an existing one.
enum TightenOutcome {
    /// Use this bound as the new lower/upper and consume the candidate
    /// predicate.
    Bound((IndexKey, bool)),
    /// Keep the existing bound; do NOT consume the candidate predicate. This
    /// arises when the candidate is a parameter slot and the existing bound is
    /// a literal (or vice versa) — plan-time tightening is impossible across
    /// the literal/parameter boundary, so the index probe uses the existing
    /// bound and the candidate stays as a residual filter.
    KeepExisting((IndexKey, bool)),
    /// Refuse the rewrite entirely; only fires for un-orderable literal pairs
    /// (e.g. NaN floats).
    Reject,
}

/// Combine two lower bounds. Literal-literal pairs apply the existing
/// tightening rule (higher bound wins; exclusive wins on ties). Any pair
/// involving a parameter slot keeps the existing bound — the candidate stays
/// in the residual filter because runtime can't compare across the
/// literal/parameter boundary.
fn tighten_lower(
    existing: Option<(IndexKey, bool)>,
    candidate: (IndexKey, bool),
) -> TightenOutcome {
    let Some(existing) = existing else {
        return TightenOutcome::Bound(candidate);
    };
    match (&existing.0, &candidate.0) {
        (IndexKey::Literal(existing_lit), IndexKey::Literal(candidate_lit)) => {
            let Some(ordering) = compare_literals(existing_lit, candidate_lit) else {
                return TightenOutcome::Reject;
            };
            TightenOutcome::Bound(match ordering {
                std::cmp::Ordering::Less => candidate,
                std::cmp::Ordering::Greater => existing,
                std::cmp::Ordering::Equal => {
                    // Same literal; exclusive (false) wins because it filters
                    // one more value.
                    (existing.0, existing.1 && candidate.1)
                }
            })
        }
        _ => TightenOutcome::KeepExisting(existing),
    }
}

/// Combine two upper bounds (symmetric counterpart of [`tighten_lower`]).
fn tighten_upper(
    existing: Option<(IndexKey, bool)>,
    candidate: (IndexKey, bool),
) -> TightenOutcome {
    let Some(existing) = existing else {
        return TightenOutcome::Bound(candidate);
    };
    match (&existing.0, &candidate.0) {
        (IndexKey::Literal(existing_lit), IndexKey::Literal(candidate_lit)) => {
            let Some(ordering) = compare_literals(existing_lit, candidate_lit) else {
                return TightenOutcome::Reject;
            };
            TightenOutcome::Bound(match ordering {
                std::cmp::Ordering::Less => existing,
                std::cmp::Ordering::Greater => candidate,
                std::cmp::Ordering::Equal => (existing.0, existing.1 && candidate.1),
            })
        }
        _ => TightenOutcome::KeepExisting(existing),
    }
}

/// Compare two literals of the same kind. Returns `None` when the literals
/// are non-orderable (e.g., NaN floats) or kinds differ.
fn compare_literals(a: &Literal, b: &Literal) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Literal::Integer(lhs, _), Literal::Integer(rhs, _)) => Some(lhs.cmp(rhs)),
        (Literal::Integer(lhs, _), Literal::RadixInteger(rhs, _, _))
        | (Literal::RadixInteger(lhs, _, _), Literal::Integer(rhs, _))
        | (Literal::RadixInteger(lhs, _, _), Literal::RadixInteger(rhs, _, _)) => {
            Some(lhs.cmp(rhs))
        }
        (Literal::Float(lhs, _, _), Literal::Float(rhs, _, _)) => lhs.partial_cmp(rhs),
        (Literal::Decimal(lhs, _, _), Literal::Decimal(rhs, _, _)) => Some(lhs.cmp(rhs)),
        (Literal::String(lhs, _, _), Literal::String(rhs, _, _)) => {
            Some(lhs.as_str().cmp(rhs.as_str()))
        }
        (Literal::Bytes(lhs, _), Literal::Bytes(rhs, _)) => Some(lhs.as_ref().cmp(rhs.as_ref())),
        (Literal::Date(lhs, _, _), Literal::Date(rhs, _, _)) => Some(lhs.cmp(rhs)),
        (Literal::LocalDateTime(lhs, _, _), Literal::LocalDateTime(rhs, _, _)) => {
            Some(lhs.cmp(rhs))
        }
        (Literal::ZonedDateTime(lhs, _, _), Literal::ZonedDateTime(rhs, _, _)) => {
            Some(lhs.cmp(rhs))
        }
        (Literal::LocalTime(lhs, _, _), Literal::LocalTime(rhs, _, _)) => Some(lhs.cmp(rhs)),
        (Literal::ZonedTime(lhs, _, _), Literal::ZonedTime(rhs, _, _)) => Some(lhs.cmp(rhs)),
        (Literal::Duration(lhs, _, _), Literal::Duration(rhs, _, _)) => {
            let lhs = selene_core::duration_order_key(lhs);
            let rhs = selene_core::duration_order_key(rhs);
            selene_core::duration_keys_comparable(lhs, rhs).then(|| lhs.cmp(&rhs))
        }
        (Literal::Bool(lhs, _), Literal::Bool(rhs, _)) => Some(lhs.cmp(rhs)),
        _ => None,
    }
}

/// Whether `[lo, hi]` (with the given inclusivities) contains at least one
/// value of the literal kind. Returns `true` for any orderable, non-empty
/// range; `false` for empty or non-orderable ranges (caller treats `false`
/// as "refuse rewrite").
fn range_satisfiable(lo: &Literal, lo_inclusive: bool, hi: &Literal, hi_inclusive: bool) -> bool {
    let Some(ordering) = compare_literals(lo, hi) else {
        return false;
    };
    match ordering {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => lo_inclusive && hi_inclusive,
    }
}

fn target_for_scan_kind(kind: ScanKind) -> IndexTarget {
    match kind {
        ScanKind::Node => IndexTarget::Node,
        ScanKind::Edge => IndexTarget::Edge,
    }
}

fn binding_is_target(
    bindings: &[BindingDef],
    binding_id: crate::BindingId,
    target: IndexTarget,
) -> bool {
    let element = match target {
        IndexTarget::Node => BindingElement::Node,
        IndexTarget::Edge => BindingElement::Edge,
    };
    bindings
        .iter()
        .any(|binding| binding.binding == binding_id && binding.element == element)
}

fn remove_indices(predicates: &mut Vec<FilterPredicate>, indices: &[usize]) {
    let mut cursor = 0usize;
    predicates.retain(|_| {
        let remove = indices.binary_search(&cursor).is_ok();
        cursor += 1;
        !remove
    });
}
