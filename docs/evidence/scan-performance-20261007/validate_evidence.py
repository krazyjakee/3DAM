"""Validate the checked-in reports and their explicitly scoped performance budgets."""
import json
import math
from pathlib import Path

root = Path(__file__).resolve().parent
reports = sorted(root.glob("*.json"))
scan_reports = 0
for report in reports:
    data = json.loads(report.read_text())
    if data.get("schema_version") != 1:
        continue
    scan_reports += 1
    expected_failures = {
        "native-hdd-separate-before.json": ["first_full_verification"],
        "native-hdd-separate-after.json": ["few_change_quick"],
        "native-hdd-separate-trace.json": ["first_full_verification", "cancel_full"],
    }.get(report.name, [])
    failed_phases = []
    failures = []
    for phase in data["profiles"]:
        failures.extend(phase["assertion_failures"])
        if phase["assertion_failures"]:
            failed_phases.append(phase["phase"])
        assert math.isfinite(phase["elapsed_ms"])
        assert phase["sql_writer_statements"] > 0
        assert phase["sql_commit_statements"] >= 0
        assert phase["checkpoint"]["logical_checkpoint_bytes"] is None or phase["checkpoint"]["logical_checkpoint_bytes"] >= 0
        assert phase["peak_rss_bytes"] > 0
        if phase["mode"] == "quick":
            assert phase["source"]["fetch_opens"] == 0
            assert phase["source"]["source_payload_bytes"] == 0
            assert phase["source"]["scratch_written_bytes"] == 0
        if phase["phase"] == "cancel_full":
            assert phase["cancellation_requested"]
            assert phase["job"]["state"] == "cancelled"
            assert phase["cancellation_source_quiescence_ms"] < 30000
    if expected_failures:
        assert failed_phases == expected_failures
        assert failures == ["foreground writer p95 exceeded the configured latency budget"] * len(expected_failures)
    else:
        assert not failures, (report.name, failures)

for name, budget_ms in [("generated-100k-after", 15000), ("generated-1m", 60000)]:
    data = json.loads((root / (name + ".json")).read_text())
    phases = {p["phase"]: p for p in data["profiles"]}
    for phase in phases.values():
        assert phase["peak_rss_bytes"] < 128 * 1024 * 1024
        if phase["mode"] == "quick":
            assert phase["elapsed_ms"] < budget_ms
    assert phases["unchanged_delta"]["asset_rows_updated"] == 0
    assert phases["first_full_verification"]["storage_usage"]["asset_count"] == data["config"]["paths"]

large = json.loads((root / "generated-large.json").read_text())
phases = {p["phase"]: p for p in large["profiles"]}
assert phases["few_change_quick"]["elapsed_ms"] < 5000
assert phases["verify_changes_full"]["source"]["source_payload_bytes"] >= 4294967296
assert phases["verify_changes_full"]["source"]["scratch_written_bytes"] >= 4294967296

replay = json.loads((root / "marker-replay.json").read_text())
for phase in replay["profiles"]:
    assert phase["explicit_commits"] == 157
    if phase["strategy"] == "private_spool":
        assert phase["asset_rows_updated"] == 0
        assert phase["main_wal_end_bytes"] == 0
        assert phase["main_checkpoint_logical_bytes"] == 0
    else:
        assert phase["asset_rows_updated"] == 20000

print(f"Validated {scan_reports} production reports, retained expected HDD/trace failures, scale/RSS/discovery budgets and ordered/shuffled observation evidence")

scheduler = json.loads((root / "shared-hdd-scheduler.json").read_text())
for phase in scheduler["phases"].values():
    assert phase["after"]["progress"] == 4
    for metric in ["foreground_p95_ms", "competitor_p95_ms"]:
        assert phase["before"][metric] > 60
        assert phase["after"][metric] <= 60
print("Validated governed serial-storage latency control")
