#!/usr/bin/env bash
# Sequential benchmark runner — the sanctioned entry point for running benchmark
# binaries. Direct `cargo bench --workspace` is forbidden because Cargo may
# dispatch multiple bench binaries concurrently; this runner executes them one
# at a time (the pgrep guard below + a serial run loop with no `&`).
#
# The suite is criterion-only (wall-clock medians on the dev box). There is no
# iai-callgrind layer (instruction-count gates were dropped: they need valgrind,
# which never runs on the macOS dev machine).
#
# Quick reference:
#   run-benches.sh --list                       # enumerate registered benches
#   run-benches.sh                               # FULL run, all benches (slow)
#   run-benches.sh --smoke                       # curated <~60s tripwire subset
#   run-benches.sh --bench wal                   # just one bench bin (scoped compile)
#   run-benches.sh --bench vector_graph_retrieval --compile-only  # compile tripwire, no Criterion run
#   run-benches.sh --crate selene-db-graph       # just one package's benches
#   run-benches.sh --bench wal --filter append_batch     # one criterion group
#   run-benches.sh --bench graph_hub_delete --save-baseline pre-graph05
#   run-benches.sh --bench graph_hub_delete --baseline pre-graph05   # %-change diff
#   run-benches.sh --bench wal --sample-size 50 --measurement-time 5 # A/B fidelity
#   run-benches.sh --bench single_graph --filter graph_exact_vector_scan --vector-scales million
#   run-benches.sh --bench vector_index_rebuild --vector-scales 10000,50000
#   run-benches.sh --bench vector_index_rebuild --filter graph_vector_index_rebuild/ivf --vector-scales 100000
#   run-benches.sh --bench vector_index_rebuild --filter graph_vector_index_ivf_target_centroid_rebuild --vector-scales 10000
#   run-benches.sh --bench vector_turbo_projection --filter graph_turbo_quant_dimension_projection
#   run-benches.sh --bench vector_turbo_churn --filter graph_turbo_quant_churn
#   run-benches.sh --bench vector_ivf_pressure --filter graph_ivf_target_centroids --vector-scales 10000
#   run-benches.sh --bench vector_ivf_insert_drift --vector-scales 10000
#   run-benches.sh --profile quick --bench vector_wgpu --filter core_vector_wgpu_prototype
#   SELENE_VECTOR_IVF_INSERT_DRIFT_BPS=100,500,1000 run-benches.sh --bench vector_ivf_insert_drift --vector-scales 10000
#   run-benches.sh --bench vector_index_rebuild --allocator system    # allocator A/B
#   run-benches.sh --crate selene-db-graph --dry-run     # preview, run nothing
#
# Profiles select the workload envelope via
# crates/selene-testing/src/bench_profiles.rs::BenchProfile::scales():
#   quick  => one 1k scale, sample 10   (fast smoke)
#   full   => 10k/50k/100k, sample 30   (publish-quality / north star)
#   stress => adds 250k                 (opt-in larger envelope)
#
# Vector scale presets set both SELENE_VECTOR_BENCH_SCALES and
# SELENE_VECTOR_REBUILD_BENCH_SCALES for vector-only sweeps:
#   quick|full|stress mirror the profile scales
#   large => 250k/1M
#   million => 1M
#   comma-separated positive integers are accepted for custom sweeps
#
# Allocator:
#   mimalloc (default) => bench binaries install mimalloc as global allocator
#   system             => bench binaries compile without that global allocator

set -euo pipefail

# ---------------------------------------------------------------------------
# Bench registry — the SINGLE source of truth for which benches exist.
# Format: <crate>|<bench-bin>|<needs_test_harness 0|1>
# A new [[bench]] target MUST be added here (a missing entry = the bench never
# runs and never lands in BENCHMARKS.md — the historical expression_eval orphan).
# selene-gql parse/analyze/plan_optimize/write_e2e carry
# required-features=["test-harness"]; Cargo SILENTLY skips a required-features
# bin if the feature is absent, so those entries are marked needs_test_harness=1.
# ---------------------------------------------------------------------------
REGISTRY="
selene-db-catalog|catalog_descriptors|0
selene-db|catalog_lifecycle|0
selene-db-core|value_clone|0
selene-db-core|vector_wgpu|0
selene-db-graph|single_graph|0
selene-db-graph|mixed_edge_storage|0
selene-db-graph|vector_index_rebuild|0
selene-db-graph|vector_pq|0
selene-db-graph|vector_ivf_pq|0
selene-db-graph|vector_turbo_projection|0
selene-db-graph|vector_turbo_churn|0
selene-db-graph|vector_ivf_pressure|0
selene-db-graph|vector_ivf_insert_drift|0
selene-db-graph|vector_mixed_workload|0
selene-db-graph|bulk_mutation|0
selene-db-graph|concurrent_read|0
selene-db-graph|bfs|0
selene-db-graph|write_txn_lifecycle|0
selene-db-graph|provider_fanout|0
selene-db-graph|bound_type_validation|0
selene-db-graph|concurrent_writers|0
selene-db-graph|graph_hub_delete|0
selene-db-graph|graph_delete_reclamation|0
selene-db-graph|graph_snapshot_roundtrip|0
selene-db-graph|graph_read_under_write|0
selene-db-graph|graph_mixed_workload|0
selene-db-graph|text_search_bm25|0
selene-db-persist|wal|0
selene-db-persist|snapshot|0
selene-db-gql|parse|1
selene-db-gql|analyze|1
selene-db-gql|plan_optimize|1
selene-db-gql|mixed_orientation|1
selene-db-gql|write_e2e|1
selene-db-gql|expression_eval|0
selene-db-gql|procedure_call_repeat|1
selene-db-gql|correlated_subquery|0
selene-db-gql|read_pipeline|0
selene-db-gql|execution_context|0
selene-db-algorithms|algo_bench|0
selene-db-algorithms|projection|0
selene-db-algorithms|vector_graph_retrieval|0
"

# Curated smoke subset — highest-signal, lowest-variance rows for a <~60s
# pre-push tripwire. Format: <crate>|<bench-bin>|<criterion-id-filter>
# Runs at --profile quick unless an explicit --profile overrides it.
SMOKE="
selene-db-graph|single_graph|node_fetch
selene-db-graph|single_graph|label_index
selene-db-graph|bulk_mutation|commit_batch
selene-db-persist|wal|append_batch_1000
selene-db-gql|plan_optimize|
selene-db-gql|expression_eval|
selene-db-algorithms|projection|projection_build
selene-db-algorithms|algo_bench|pagerank
"

PROFILE=""
SMOKE_MODE=0
LIST_MODE=0
DRY_RUN=0
COMPILE_ONLY=0
SAVE_BASELINE=""
BASELINE=""
FILTER=""
SAMPLE_SIZE=""
MEASUREMENT_TIME=""
VECTOR_SCALES_ARG=""
VECTOR_SCALES=""
ALLOCATOR="mimalloc"
SEL_CRATES=()
SEL_BENCHES=()

usage() {
  sed -n '2,48p' "$0"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --profile)          PROFILE="$2"; shift 2 ;;
    --crate)            SEL_CRATES+=("$2"); shift 2 ;;
    --bench)            SEL_BENCHES+=("$2"); shift 2 ;;
    --filter)           FILTER="$2"; shift 2 ;;
    --save-baseline)    SAVE_BASELINE="$2"; shift 2 ;;
    --baseline)         BASELINE="$2"; shift 2 ;;
    --sample-size|--samples) SAMPLE_SIZE="$2"; shift 2 ;;
    --measurement-time) MEASUREMENT_TIME="$2"; shift 2 ;;
    --vector-scales)    VECTOR_SCALES_ARG="$2"; shift 2 ;;
    --allocator)        ALLOCATOR="$2"; shift 2 ;;
    --smoke)            SMOKE_MODE=1; shift ;;
    --list)             LIST_MODE=1; shift ;;
    --dry-run)          DRY_RUN=1; shift ;;
    --compile-only)     COMPILE_ONLY=1; shift ;;
    -h|--help)          usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; echo "Run with --help for usage." >&2; exit 2 ;;
  esac
done

if [ -n "$SAVE_BASELINE" ] && [ -n "$BASELINE" ]; then
  echo "ERROR: --save-baseline and --baseline are mutually exclusive (save records a baseline; compare reads one)." >&2
  exit 2
fi

if [ -n "$PROFILE" ]; then
  case "$PROFILE" in
    quick|full|stress) ;;
    *) echo "ERROR: --profile must be quick, full, or stress" >&2; exit 2 ;;
  esac
fi

case "$ALLOCATOR" in
  mimalloc|system) ;;
  *) echo "ERROR: --allocator must be mimalloc or system" >&2; exit 2 ;;
esac

resolve_vector_scales() {
  case "$1" in
    quick) echo "1000"; return 0 ;;
    full) echo "10000,50000,100000"; return 0 ;;
    stress) echo "1000,10000,50000,100000,250000"; return 0 ;;
    large) echo "250000,1000000"; return 0 ;;
    million) echo "1000000"; return 0 ;;
  esac

  printf '%s\n' "$1" | awk -F',' '
    BEGIN { ok = 1 }
    {
      for (i = 1; i <= NF; i++) {
        part = $i
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", part)
        if (part !~ /^[1-9][0-9]*$/) {
          ok = 0
          exit
        }
        if (!(part in seen)) {
          seen[part] = 1
          vals[++n] = part + 0
        }
      }
    }
    END {
      if (!ok || n == 0) {
        exit 1
      }
      for (i = 1; i <= n; i++) {
        for (j = i + 1; j <= n; j++) {
          if (vals[j] < vals[i]) {
            tmp = vals[i]
            vals[i] = vals[j]
            vals[j] = tmp
          }
        }
      }
      for (i = 1; i <= n; i++) {
        printf "%s%d", (i == 1 ? "" : ","), vals[i]
      }
      printf "\n"
    }
  '
}

if [ -n "$VECTOR_SCALES_ARG" ]; then
  if ! VECTOR_SCALES="$(resolve_vector_scales "$VECTOR_SCALES_ARG")"; then
    echo "ERROR: --vector-scales must be quick, full, stress, large, million, or a comma-separated list of positive integers" >&2
    exit 2
  fi
fi

# crate for a bench name (first registry match); empty if unknown.
crate_for_bench() {
  printf '%s\n' "$REGISTRY" | awk -F'|' -v b="$1" '$2==b {print $1; exit}'
}

if [ "$LIST_MODE" -eq 1 ]; then
  echo "Registered benches (criterion; <crate> :: <bench> [test-harness]):"
  printf '%s\n' "$REGISTRY" | while IFS='|' read -r crate bench harness; do
    [ -n "$crate" ] || continue
    if [ "$harness" = "1" ]; then echo "  $crate :: $bench  (needs --features test-harness)"; else echo "  $crate :: $bench"; fi
  done
  echo ""
  echo "Smoke subset (--smoke), at --profile quick:"
  printf '%s\n' "$SMOKE" | while IFS='|' read -r crate bench filt; do
    [ -n "$crate" ] || continue
    echo "  $crate :: $bench${filt:+  (filter: $filt)}"
  done
  echo ""
  echo "Vector scale presets (--vector-scales): quick, full, stress, large, million, or a comma list"
  exit 0
fi

# Build the selected work list as lines: <crate>|<bench>|<needs_harness>|<filter>
SELECTED=""

if [ "$SMOKE_MODE" -eq 1 ]; then
  if [ "${#SEL_CRATES[@]}" -gt 0 ] || [ "${#SEL_BENCHES[@]}" -gt 0 ]; then
    echo "ERROR: --smoke runs a fixed curated subset; do not combine it with --crate/--bench." >&2
    exit 2
  fi
  [ -n "$PROFILE" ] || PROFILE="quick"
  while IFS='|' read -r crate bench filt; do
    [ -n "$crate" ] || continue
    harness="$(printf '%s\n' "$REGISTRY" | awk -F'|' -v c="$crate" -v b="$bench" '$1==c && $2==b {print $3; exit}')"
    SELECTED+="$crate|$bench|${harness:-0}|$filt"$'\n'
  done <<< "$(printf '%s\n' "$SMOKE")"
else
  [ -n "$PROFILE" ] || PROFILE="full"
  # Validate explicit --bench selections against the registry.
  for b in "${SEL_BENCHES[@]:-}"; do
    [ -n "$b" ] || continue
    if [ -z "$(crate_for_bench "$b")" ]; then
      echo "ERROR: unknown --bench '$b'. Use --list to see registered benches." >&2; exit 2
    fi
  done
  while IFS='|' read -r crate bench harness; do
    [ -n "$crate" ] || continue
    keep=1
    if [ "${#SEL_BENCHES[@]}" -gt 0 ]; then
      keep=0
      for b in "${SEL_BENCHES[@]}"; do [ "$b" = "$bench" ] && keep=1; done
    elif [ "${#SEL_CRATES[@]}" -gt 0 ]; then
      keep=0
      for c in "${SEL_CRATES[@]}"; do [ "$c" = "$crate" ] && keep=1; done
    fi
    [ "$keep" -eq 1 ] && SELECTED+="$crate|$bench|$harness|$FILTER"$'\n'
  done <<< "$(printf '%s\n' "$REGISTRY")"
fi

SELECTED="$(printf '%s' "$SELECTED" | sed '/^$/d')"
if [ -z "$SELECTED" ]; then
  echo "ERROR: selection matched no registered benches. Use --list." >&2
  exit 2
fi

# criterion passthrough args common to every selected bench.
CRIT_ARGS=()
[ -n "$SAVE_BASELINE" ]    && CRIT_ARGS+=(--save-baseline "$SAVE_BASELINE")
[ -n "$BASELINE" ]         && CRIT_ARGS+=(--baseline "$BASELINE")
[ -n "$SAMPLE_SIZE" ]      && CRIT_ARGS+=(--sample-size "$SAMPLE_SIZE")
[ -n "$MEASUREMENT_TIME" ] && CRIT_ARGS+=(--measurement-time "$MEASUREMENT_TIME")

# Resolve one bench's `cargo bench` argv into the global array RESOLVED.
# Per-bench filter is appended last as a criterion positional id-filter.
resolve_args() {
  local crate="$1" bench="$2" harness="$3" filt="$4" no_run="$5"
  RESOLVED=(bench -p "$crate" --bench "$bench")
  [ "$harness" = "1" ] && RESOLVED+=(--features test-harness)
  [ "$no_run" = "1" ] && RESOLVED+=(--no-run)
  local tail=()
  if [ "${#CRIT_ARGS[@]}" -gt 0 ]; then
    tail=("${CRIT_ARGS[@]}")
  fi
  [ -n "$filt" ] && tail+=("$filt")
  if [ "$no_run" != "1" ] && [ "${#tail[@]}" -gt 0 ]; then
    RESOLVED+=(-- "${tail[@]}")
  fi
}

bench_env_prefix() {
  local bench="${1:-}" filt="${2:-}"
  local prefix="SELENE_BENCH_PROFILE=$PROFILE SELENE_BENCH_ALLOCATOR=$ALLOCATOR"
  if [ -n "$VECTOR_SCALES" ]; then
    prefix+=" SELENE_VECTOR_BENCH_SCALES=$VECTOR_SCALES SELENE_VECTOR_REBUILD_BENCH_SCALES=$VECTOR_SCALES"
  fi
  local variant_filter
  variant_filter="$(vector_rebuild_variant_filter "$bench" "$filt")"
  if [ -n "$variant_filter" ]; then
    prefix+=" SELENE_VECTOR_REBUILD_VARIANT_FILTER=$variant_filter"
  fi
  local group_filter
  group_filter="$(vector_rebuild_group_filter "$bench" "$filt")"
  if [ -n "$group_filter" ]; then
    prefix+=" SELENE_VECTOR_REBUILD_GROUP_FILTER=$group_filter"
  fi
  local ivf_pressure_group_filter
  ivf_pressure_group_filter="$(vector_ivf_pressure_group_filter "$bench" "$filt")"
  if [ -n "$ivf_pressure_group_filter" ]; then
    prefix+=" SELENE_VECTOR_IVF_PRESSURE_GROUP_FILTER=$ivf_pressure_group_filter"
  fi
  if [ "$ALLOCATOR" = "system" ]; then
    prefix+=" RUSTFLAGS='${RUSTFLAGS:+$RUSTFLAGS }--cfg selene_bench_system_alloc'"
  fi
  printf '%s' "$prefix"
}

vector_rebuild_variant_filter() {
  local bench="$1" filt="$2"
  if [ "$bench" != "vector_index_rebuild" ] || [ -z "$filt" ]; then
    return 0
  fi
  case "$filt" in
    *ivf*) echo "ivf" ;;
    *hnsw*) echo "hnsw" ;;
  esac
}

vector_rebuild_group_filter() {
  local bench="$1" filt="$2"
  if [ "$bench" != "vector_index_rebuild" ] || [ -z "$filt" ]; then
    return 0
  fi
  case "$filt" in
    graph_vector_index_rebuild*) echo "rebuild" ;;
    graph_vector_index_ivf_target_centroid_rebuild*) echo "ivf_target_centroids" ;;
    graph_vector_index_recommended_rebuild*) echo "recommended_rebuild" ;;
    graph_vector_index_stale_query*) echo "stale_query" ;;
    graph_vector_index_dimension_projection*) echo "dimension_projection" ;;
  esac
}

vector_ivf_pressure_group_filter() {
  local bench="$1" filt="$2"
  if [ "$bench" != "vector_ivf_pressure" ] || [ -z "$filt" ]; then
    return 0
  fi
  case "$filt" in
    graph_ivf_candidate_pressure*) echo "candidate_pressure" ;;
    graph_ivf_target_centroids*) echo "target_centroids" ;;
  esac
}

run_cargo_for_bench() {
  local bench="$1" filt="$2"
  local variant_filter
  variant_filter="$(vector_rebuild_variant_filter "$bench" "$filt")"
  local group_filter
  group_filter="$(vector_rebuild_group_filter "$bench" "$filt")"
  local ivf_pressure_group_filter
  ivf_pressure_group_filter="$(vector_ivf_pressure_group_filter "$bench" "$filt")"
  if [ -n "$variant_filter" ] && [ -n "$group_filter" ]; then
    SELENE_VECTOR_REBUILD_VARIANT_FILTER="$variant_filter" SELENE_VECTOR_REBUILD_GROUP_FILTER="$group_filter" cargo "${RESOLVED[@]}"
  elif [ -n "$variant_filter" ]; then
    SELENE_VECTOR_REBUILD_VARIANT_FILTER="$variant_filter" cargo "${RESOLVED[@]}"
  elif [ -n "$group_filter" ]; then
    SELENE_VECTOR_REBUILD_GROUP_FILTER="$group_filter" cargo "${RESOLVED[@]}"
  elif [ -n "$ivf_pressure_group_filter" ]; then
    SELENE_VECTOR_IVF_PRESSURE_GROUP_FILTER="$ivf_pressure_group_filter" cargo "${RESOLVED[@]}"
  else
    cargo "${RESOLVED[@]}"
  fi
}

if [ "$DRY_RUN" -eq 1 ]; then
  echo "==> DRY RUN (profile=$PROFILE) — resolved invocations, nothing executed:"
  while IFS='|' read -r crate bench harness filt; do
    [ -n "$crate" ] || continue
    no_run=0
    [ "$COMPILE_ONLY" -eq 1 ] && no_run=1
    resolve_args "$crate" "$bench" "$harness" "$filt" "$no_run"
    echo "  $(bench_env_prefix "$bench" "$filt") cargo ${RESOLVED[*]}"
  done <<< "$SELECTED"
  exit 0
fi

# Strictly-sequential guarantee: refuse to start if another bench run is live.
if [ "${SELENE_BENCH_FORCE_CONFLICT:-0}" = "1" ] || pgrep -f "cargo bench" 2>/dev/null | grep -v "$$" >/dev/null 2>&1; then
  echo "ERROR: detected another cargo bench process; benchmark execution is strictly sequential." >&2
  exit 1
fi

export SELENE_BENCH_PROFILE="$PROFILE"
export SELENE_BENCH_ALLOCATOR="$ALLOCATOR"
if [ -n "$VECTOR_SCALES" ]; then
  export SELENE_VECTOR_BENCH_SCALES="$VECTOR_SCALES"
  export SELENE_VECTOR_REBUILD_BENCH_SCALES="$VECTOR_SCALES"
fi
if [ "$ALLOCATOR" = "system" ]; then
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--cfg selene_bench_system_alloc"
fi

# Compile pre-pass (parallel compilation WITHIN each invocation is allowed;
# done up front so cold-compile time never pollutes the first measurement).
echo "==> Compiling selected bench binaries (profile=$PROFILE, allocator=$ALLOCATOR)"
while IFS='|' read -r crate bench harness filt; do
  [ -n "$crate" ] || continue
  resolve_args "$crate" "$bench" "$harness" "$filt" 1
  echo "  cargo ${RESOLVED[*]}"
  run_cargo_for_bench "$bench" "$filt"
done <<< "$SELECTED"

if [ "$COMPILE_ONLY" -eq 1 ]; then
  echo "==> Compile-only complete; Criterion run pass skipped."
  exit 0
fi

# Run pass — STRICTLY SEQUENTIAL (no `&`, no --workspace, no xargs -P).
while IFS='|' read -r crate bench harness filt; do
  [ -n "$crate" ] || continue
  resolve_args "$crate" "$bench" "$harness" "$filt" 0
  echo "==> [criterion] $crate :: $bench (profile=$PROFILE, allocator=$ALLOCATOR)${filt:+ filter=$filt}"
  run_cargo_for_bench "$bench" "$filt"
done <<< "$SELECTED"
