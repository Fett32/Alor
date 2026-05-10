"""Drift-detector + version-gate tests for the Alor IPC protocol.

Canonical spec: `proto/alor_protocol.yaml` in the repo root.
Every MSG_* constant defined in any implementation file
(`src-tauri/src/wrapper/protocol.rs`, `wrapper/src/protocol.rs`,
`orchestrator-py/agent_client.py`) MUST match the spec in name
AND wire value. `PROTOCOL_VERSION` MUST match across all four
(spec + 3 implementations).

Pre-fix (before 2026-04-20 audit T2), the protocol was
triplicated with no single source of truth and no mechanism to
detect drift. Adding a new message kind to Rust without updating
Python was a silent failure mode. Renaming a field was the same.
Version bumps weren't possible because there was no version.

This test closes the gap: any drift — new const in one file but
not the spec, spec entry with no consumer, version mismatch —
fails CI loudly. The FORCING FUNCTION is the spec: to ship a
schema change you MUST edit the YAML, which means reviewers see
it.

Follows the suite convention: standalone asyncio.run(main()),
non-zero exit on failure, no pytest. Runs via
orchestrator-py/run_tests.sh.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any

import yaml


# ---- paths ---------------------------------------------------------------

REPO_ROOT = Path(__file__).resolve().parent.parent
PROTOCOL_YAML = REPO_ROOT / "proto" / "alor_protocol.yaml"
TAURI_PROTO_RS = REPO_ROOT / "src-tauri" / "src" / "wrapper" / "protocol.rs"
WRAPPER_PROTO_RS = REPO_ROOT / "wrapper" / "src" / "protocol.rs"
PYTHON_CLIENT = REPO_ROOT / "orchestrator-py" / "agent_client.py"


# ---- parsers -------------------------------------------------------------


def load_spec() -> dict[str, Any]:
    with PROTOCOL_YAML.open() as f:
        return yaml.safe_load(f)


def parse_rust_msg_consts(path: Path) -> dict[str, str]:
    """Extract `pub const MSG_FOO: &str = "value";` pairs."""
    out: dict[str, str] = {}
    pattern = re.compile(r'pub const (MSG_[A-Z_]+):\s*&str\s*=\s*"([^"]+)"\s*;')
    for line in path.read_text().splitlines():
        m = pattern.search(line)
        if m:
            out[m.group(1)] = m.group(2)
    return out


def parse_rust_protocol_version(path: Path) -> int:
    """Extract `pub const PROTOCOL_VERSION: u32 = N;` value."""
    m = re.search(
        r'pub const PROTOCOL_VERSION:\s*u32\s*=\s*(\d+)\s*;',
        path.read_text(),
    )
    if not m:
        raise AssertionError(f"{path.name} missing PROTOCOL_VERSION const")
    return int(m.group(1))


def parse_python_msg_consts(path: Path) -> dict[str, str]:
    """Extract module-level `MSG_FOO = "value"` pairs."""
    out: dict[str, str] = {}
    pattern = re.compile(r'^(MSG_[A-Z_]+)\s*=\s*"([^"]+)"\s*$')
    for line in path.read_text().splitlines():
        m = pattern.match(line)
        if m:
            out[m.group(1)] = m.group(2)
    return out


def parse_python_protocol_version(path: Path) -> int:
    m = re.search(
        r'^PROTOCOL_VERSION\s*=\s*(\d+)\s*$',
        path.read_text(),
        re.MULTILINE,
    )
    if not m:
        raise AssertionError(f"{path.name} missing PROTOCOL_VERSION assignment")
    return int(m.group(1))


# ---- check framework -----------------------------------------------------


FAIL = 0


def check(label: str, got: Any, want: Any) -> None:
    global FAIL
    if got == want:
        print(f"ok    {label}")
    else:
        print(f"FAIL  {label}: got {got!r}, want {want!r}", file=sys.stderr)
        FAIL += 1


def check_true(label: str, cond: bool) -> None:
    check(label, bool(cond), True)


# ---- tests ----------------------------------------------------------------


def test_protocol_version_matches_across_spec_and_code() -> None:
    """`protocol_version` in the YAML, `PROTOCOL_VERSION` in both
    Rust files, and `PROTOCOL_VERSION` in Python ALL agree.
    Bumping the spec without also bumping the code (or vice
    versa) fails here.
    """
    spec = load_spec()
    spec_ver = spec["protocol_version"]
    tauri_ver = parse_rust_protocol_version(TAURI_PROTO_RS)
    wrapper_ver = parse_rust_protocol_version(WRAPPER_PROTO_RS)
    python_ver = parse_python_protocol_version(PYTHON_CLIENT)

    check("spec → tauri: PROTOCOL_VERSION agrees", tauri_ver, spec_ver)
    check("spec → wrapper: PROTOCOL_VERSION agrees", wrapper_ver, spec_ver)
    check("spec → python: PROTOCOL_VERSION agrees", python_ver, spec_ver)


def test_every_rust_msg_const_appears_in_spec() -> None:
    """For each `pub const MSG_*: &str = "..."` in either Rust
    protocol file, the spec must contain a matching entry by
    name AND wire value. Catches: new kind added to Rust but
    not the spec.
    """
    spec = load_spec()
    spec_by_name = {m["name"]: m["value"] for m in spec["messages"]}

    tauri_consts = parse_rust_msg_consts(TAURI_PROTO_RS)
    wrapper_consts = parse_rust_msg_consts(WRAPPER_PROTO_RS)

    for name, value in tauri_consts.items():
        check_true(
            f"tauri const {name} is in spec",
            name in spec_by_name,
        )
        if name in spec_by_name:
            check(
                f"tauri const {name} value matches spec",
                value,
                spec_by_name[name],
            )

    for name, value in wrapper_consts.items():
        check_true(
            f"wrapper const {name} is in spec",
            name in spec_by_name,
        )
        if name in spec_by_name:
            check(
                f"wrapper const {name} value matches spec",
                value,
                spec_by_name[name],
            )


def test_every_python_msg_const_appears_in_spec() -> None:
    """Same check for Python. Catches: new kind added to Python
    without updating the spec or the Rust sides.
    """
    spec = load_spec()
    spec_by_name = {m["name"]: m["value"] for m in spec["messages"]}

    python_consts = parse_python_msg_consts(PYTHON_CLIENT)
    for name, value in python_consts.items():
        check_true(
            f"python const {name} is in spec",
            name in spec_by_name,
        )
        if name in spec_by_name:
            check(
                f"python const {name} value matches spec",
                value,
                spec_by_name[name],
            )


def test_every_spec_message_has_at_least_one_consumer() -> None:
    """Each spec entry lists `participants`. Assert every entry
    has at least one participant (i.e., no orphaned spec entries
    that no code actually defines). This prevents the spec
    growing stale in the other direction: old kinds removed from
    all implementations but left in the YAML.
    """
    spec = load_spec()
    for m in spec["messages"]:
        name = m["name"]
        parts = m.get("participants", [])
        check_true(
            f"spec entry {name} has at least one participant",
            len(parts) > 0,
        )
        for p in parts:
            check_true(
                f"spec entry {name} participant {p!r} is a known id",
                p in {"tauri", "wrapper", "python"},
            )


def test_each_spec_participant_matches_code_presence() -> None:
    """For every (message, participant) pair in the spec, assert
    the corresponding code file DOES define the constant.
    Catches: spec claims Python speaks a message, but Python's
    agent_client.py doesn't define it (or vice versa).
    """
    spec = load_spec()
    tauri_consts = parse_rust_msg_consts(TAURI_PROTO_RS)
    wrapper_consts = parse_rust_msg_consts(WRAPPER_PROTO_RS)
    python_consts = parse_python_msg_consts(PYTHON_CLIENT)

    code_by_participant = {
        "tauri": tauri_consts,
        "wrapper": wrapper_consts,
        "python": python_consts,
    }

    for m in spec["messages"]:
        name = m["name"]
        for participant in m.get("participants", []):
            code_map = code_by_participant[participant]
            check_true(
                f"spec {name} lists {participant}; {participant} code defines it",
                name in code_map,
            )
            if name in code_map:
                check(
                    f"spec {name}@{participant}: value matches",
                    code_map[name],
                    m["value"],
                )


def test_no_code_defines_a_constant_not_in_spec() -> None:
    """Inverse of the above: no code file ships a MSG_* that the
    spec doesn't know about. A new kind must land in the spec
    first.
    """
    spec = load_spec()
    spec_names = {m["name"] for m in spec["messages"]}

    for label, path_fn in [
        ("tauri", lambda: parse_rust_msg_consts(TAURI_PROTO_RS)),
        ("wrapper", lambda: parse_rust_msg_consts(WRAPPER_PROTO_RS)),
        ("python", lambda: parse_python_msg_consts(PYTHON_CLIENT)),
    ]:
        for name in path_fn():
            check_true(
                f"{label} const {name} has a spec entry",
                name in spec_names,
            )


# ---- version-gate behavior tests -----------------------------------------


def test_envelope_from_line_rejects_mismatched_version() -> None:
    """Python's `Envelope.from_line` raises `ProtocolVersionError`
    when the wire envelope carries a version that isn't the
    local `PROTOCOL_VERSION`. This is the fail-loud gate the
    brief asked for.
    """
    import agent_client

    good_wire = json.dumps({
        "type": agent_client.MSG_TASK_ACCEPT,
        "correlation_id": "c-1",
        "protocol_version": agent_client.PROTOCOL_VERSION,
        "payload": {"task_id": "t-1"},
    })
    env = agent_client.Envelope.from_line(good_wire)
    check("matching version parses", env.kind, agent_client.MSG_TASK_ACCEPT)
    check("matching version carries through", env.protocol_version, agent_client.PROTOCOL_VERSION)

    bad_wire = json.dumps({
        "type": agent_client.MSG_TASK_ACCEPT,
        "correlation_id": "c-2",
        "protocol_version": agent_client.PROTOCOL_VERSION + 42,  # future
        "payload": {"task_id": "t-2"},
    })
    raised = False
    try:
        agent_client.Envelope.from_line(bad_wire)
    except agent_client.ProtocolVersionError as e:
        raised = True
        check_true(
            "error message mentions the mismatch",
            "protocol version mismatch" in str(e),
        )
        check_true(
            "error message shows both wire + local",
            f"{agent_client.PROTOCOL_VERSION + 42}" in str(e)
            and f"{agent_client.PROTOCOL_VERSION}" in str(e),
        )
    check_true("mismatch raises ProtocolVersionError", raised)


def test_envelope_from_line_accepts_pre_gate_envelope() -> None:
    """Back-compat: an envelope without the `protocol_version`
    field (written before the gate landed) parses cleanly and is
    treated as the current version. Outbox frames queued in v0
    must still replay in v1.
    """
    import agent_client

    pre_gate_wire = json.dumps({
        "type": agent_client.MSG_TASK_ACCEPT,
        "correlation_id": "c-old",
        "payload": {"task_id": "t-old"},
        # NO protocol_version field.
    })
    env = agent_client.Envelope.from_line(pre_gate_wire)
    check(
        "pre-gate wire defaults to current version",
        env.protocol_version,
        agent_client.PROTOCOL_VERSION,
    )
    check("pre-gate kind carries through", env.kind, agent_client.MSG_TASK_ACCEPT)


def test_envelope_new_stamps_current_version() -> None:
    """Every new envelope built locally carries the current
    version — this is what makes the gate useful. If
    `Envelope.new` ever drops the stamp (e.g. someone introduces
    a builder path that forgets it), peers on the same version
    would reject us.
    """
    import agent_client

    env = agent_client.Envelope.new(agent_client.MSG_TASK_ACCEPT, {"task_id": "x"})
    check("new() stamps current version", env.protocol_version, agent_client.PROTOCOL_VERSION)

    # Round-trip through to_json / from_line should preserve it.
    line = env.to_json().decode()
    reparsed = agent_client.Envelope.from_line(line)
    check("round-trip preserves version", reparsed.protocol_version, env.protocol_version)


# ---- driver --------------------------------------------------------------


def main() -> None:
    test_protocol_version_matches_across_spec_and_code()
    test_every_rust_msg_const_appears_in_spec()
    test_every_python_msg_const_appears_in_spec()
    test_every_spec_message_has_at_least_one_consumer()
    test_each_spec_participant_matches_code_presence()
    test_no_code_defines_a_constant_not_in_spec()
    test_envelope_from_line_rejects_mismatched_version()
    test_envelope_from_line_accepts_pre_gate_envelope()
    test_envelope_new_stamps_current_version()


if __name__ == "__main__":
    main()
    if FAIL:
        print(f"\nFAIL — {FAIL} check(s) failed", file=sys.stderr)
        sys.exit(1)
    print("\nPASS — IPC protocol schema consistent across spec + 3 implementations.")
