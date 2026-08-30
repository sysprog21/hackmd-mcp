#!/bin/sh
set -eu

if [ ! -x /usr/bin/time ]; then
    echo "error: /usr/bin/time is required" >&2
    exit 2
fi

report=${1:-memory-baseline.tsv}
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

printf 'features\tworkload\tpeak_rss_kib\telapsed_seconds\n' >"$report"

measure() {
    feature_label=$1
    workload=$2
    shift 2
    timing="$work_dir/timing"
    /usr/bin/time -o "$timing" -f '%M\t%e' "$@" >/dev/null
    printf '%s\t%s\t' "$feature_label" "$workload" >>"$report"
    cat "$timing" >>"$report"
}

for feature_label in default otel; do
    if [ "$feature_label" = default ]; then
        feature_args=''
    else
        feature_args='--features otel'
    fi

    # Build first so the measurements exclude compilation.
    # shellcheck disable=SC2086
    cargo build --release --locked $feature_args >/dev/null
    # shellcheck disable=SC2086
    cargo test --release --locked $feature_args --no-run >/dev/null

    state_dir="$work_dir/state-$feature_label"
    # shellcheck disable=SC2086
    measure "$feature_label" startup env HACKMD_MCP_STATE_DIR="$state_dir" \
        target/release/hackmd-mcp --self-check
    # shellcheck disable=SC2086
    measure "$feature_label" list-10000 cargo test --release --locked $feature_args \
        client::tests::benchmark_10k_note_list_cache_and_filter -- --ignored --exact
    # shellcheck disable=SC2086
    measure "$feature_label" pull-10mib cargo test --release --locked $feature_args \
        sync::pull::tests::benchmark_10_mib_pull -- --ignored --exact
    # shellcheck disable=SC2086
    measure "$feature_label" safe-push cargo test --release --locked $feature_args \
        sync::push::tests::safe_push_patches_reads_back_and_advances_baseline -- --exact
    # shellcheck disable=SC2086
    measure "$feature_label" three-way-conflict cargo test --release --locked $feature_args \
        sync::push::tests::conflict_diff_bounds_maximum_size_single_lines_before_formatting -- --exact
done

printf 'wrote %s\n' "$report"
