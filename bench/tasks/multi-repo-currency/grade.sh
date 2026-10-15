#!/bin/sh
# Hidden tests per repository, then the real pipeline: ledger | reports.
set -eu
T="$BENCH_TASK_DIR"
H="$T/hidden"
cd "$BENCH_RESULT_LEDGER"
cp "$T/ledger/tests/ledger.rs" "$H/hidden_currency.rs" tests/
python3 "$BENCH_GRADERS/expect_tests.py" cargo 5 -- cargo test -q --offline
cargo build -q --offline
LEDGER_TARGET=$(cargo metadata --format-version 1 --no-deps --offline | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
cd "$BENCH_RESULT_REPORTS"
cp "$T/reports/test_reports.py" "$H/test_reports_currency.py" .
python3 "$BENCH_GRADERS/expect_tests.py" unittest 4 -- python3 -m unittest -q
"$LEDGER_TARGET/debug/ledger" < "$H/e2e.tsv" > ledger.out
python3 reports.py < ledger.out > e2e.out
diff -u "$H/e2e.expected" e2e.out
