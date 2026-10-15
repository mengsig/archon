#!/bin/sh
# Hidden tests per repository, then the real pipeline: ledger | reports.
set -eu
T="$BENCH_TASK_DIR"
H="$T/hidden"
cd "$BENCH_RESULT_LEDGER"
cp "$T/ledger/tests/ledger.rs" "$H/hidden_currency.rs" tests/
python3 "$BENCH_GRADERS/expect_tests.py" cargo 5 -- cargo test -q --offline
cargo run -q --offline < "$H/e2e.tsv" > ledger.out
cd "$BENCH_RESULT_REPORTS"
cp "$T/reports/test_reports.py" "$H/test_reports_currency.py" .
python3 "$BENCH_GRADERS/expect_tests.py" unittest 4 -- python3 -m unittest -q
python3 reports.py < "$BENCH_RESULT_LEDGER/ledger.out" > e2e.out
diff -u "$H/e2e.expected" e2e.out
