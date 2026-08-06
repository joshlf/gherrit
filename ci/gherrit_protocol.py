#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from typing import NoReturn

METADATA_PREFIX = "<!-- gherrit-meta: "
METADATA_SUFFIX = " -->"
TRAILER_TOKEN = "gherrit-pr-id"
CURRENT_ID = re.compile(r"^G[a-z2-7]{32}$")
LEGACY_ID = re.compile(r"^[GI][0-9a-f]{40}$")
DOWNLOAD_MARKER = "\n<details>\n<summary><strong>⬇️ Download this PR</strong></summary>"
HISTORY_MARKER = "\n\n**Latest Update:**"


def fail(message: str) -> NoReturn:
    print(message, file=sys.stderr)
    raise SystemExit(1)


def valid_id(value: object) -> bool:
    return isinstance(value, str) and bool(
        CURRENT_ID.fullmatch(value) or LEGACY_ID.fullmatch(value)
    )


def validate_id(value: str, context: str = "GHerrit ID") -> str:
    if not valid_id(value):
        fail(f"{context} is not a supported GHerrit ID.")
    return value


def parse_metadata(body: str) -> tuple[dict[str, str | None], int]:
    body = body.rstrip()
    start = body.rfind(METADATA_PREFIX)
    if start < 0:
        fail("Could not find terminal GHerrit metadata in the PR body.")

    candidate = body[start:]
    if not candidate.endswith(METADATA_SUFFIX) or start + len(candidate) != len(body):
        fail("GHerrit metadata comment is not terminal.")

    payload = candidate[len(METADATA_PREFIX) : -len(METADATA_SUFFIX)]
    try:
        metadata = json.loads(payload)
    except json.JSONDecodeError as primary:
        if not payload.endswith('"'):
            fail(f"GHerrit metadata is not valid JSON: {primary}")
        try:
            metadata = json.loads(payload[:-1])
        except json.JSONDecodeError:
            fail(f"GHerrit metadata is not valid JSON: {primary}")

    if not isinstance(metadata, dict) or set(metadata) != {"id", "parent", "child"}:
        fail("GHerrit metadata must contain exactly id, parent, and child.")

    for field in ("id", "parent", "child"):
        value = metadata[field]
        if field == "id":
            valid_type = isinstance(value, str)
        else:
            valid_type = value is None or isinstance(value, str)
        if not valid_type:
            fail(f"GHerrit metadata field {field} has the wrong type.")
        if isinstance(value, str) and not valid_id(value):
            fail(f"GHerrit metadata field {field} is not a supported GHerrit ID.")

    if metadata["parent"] == metadata["id"]:
        fail("GHerrit metadata names its own ID as its parent.")
    if metadata["child"] == metadata["id"]:
        fail("GHerrit metadata names its own ID as its child.")

    return metadata, start


def render_metadata(metadata: dict[str, str | None]) -> str:
    ordered = {
        "id": metadata["id"],
        "parent": metadata["parent"],
        "child": metadata["child"],
    }
    return (
        METADATA_PREFIX
        + json.dumps(ordered, separators=(",", ":"), ensure_ascii=False)
        + METADATA_SUFFIX
    )


def command_metadata(_: argparse.Namespace) -> None:
    metadata, _ = parse_metadata(sys.stdin.read())
    print(json.dumps(metadata, sort_keys=True, separators=(",", ":")))


def command_trailer_id(_: argparse.Namespace) -> None:
    ids: list[str] = []
    for line in sys.stdin.read().splitlines():
        token, separator, value = line.partition(":")
        if separator and token.strip().casefold() == TRAILER_TOKEN:
            ids.append(value.strip())
    if len(ids) != 1:
        fail("Commit does not carry exactly one gherrit-pr-id trailer.")
    print(validate_id(ids[0], "Commit gherrit-pr-id trailer"))


def history_section(
    *, gherrit_id: str, latest: int, repo_url: str, base_branch: str
) -> str:
    if latest <= 1:
        return ""

    lines = [
        f"\n\n**Latest Update:** v{latest} — [Compare vs v{latest - 1}]({repo_url}/compare/gherrit/{gherrit_id}/v{latest - 1}..gherrit/{gherrit_id}/v{latest})\n",
        "<details>",
        "<summary><strong>📚 Full Patch History</strong></summary>",
        "",
        "*Links show the diff between the row version and the column version.*",
        "",
    ]
    versions = list(range(latest - 1, 0, -1))
    lines.append("|Version|" + "".join(f" v{version} |" for version in versions) + "Base|")
    lines.append("|:---|" + ":---|" * len(versions) + ":---|")
    prefix = "vs " if latest <= 8 else ""
    sparse = latest > 8
    for row in range(latest, 0, -1):
        cells = [f"|v{row}|"]
        for column in versions:
            if column >= row:
                cells.append("|")
            elif not sparse or row == latest or row == column + 1:
                cells.append(
                    f"[{prefix}v{column}]({repo_url}/compare/gherrit/{gherrit_id}/v{column}..gherrit/{gherrit_id}/v{row})|"
                )
            else:
                cells.append("|")
        cells.append(
            f"[{prefix}Base]({repo_url}/compare/{base_branch}..gherrit/{gherrit_id}/v{row})|"
        )
        lines.append("".join(cells))
    lines.extend(["", "</details>"])
    return "\n".join(lines)


def command_promote_body(args: argparse.Namespace) -> None:
    body = sys.stdin.read().rstrip()
    metadata, metadata_start = parse_metadata(body)
    validate_id(args.id)
    validate_id(args.parent)
    if metadata["id"] != args.id:
        fail("Child PR metadata ID does not match the requested GHerrit ID.")
    if metadata["parent"] not in (args.parent, None):
        fail("Child PR metadata names an unexpected parent.")
    if args.latest < 1:
        fail("Latest patch version must be positive.")

    prefix = body[:metadata_start].rstrip()
    download = prefix.find(DOWNLOAD_MARKER)
    history = prefix.find(HISTORY_MARKER)
    if history >= 0:
        if download < 0 or history > download:
            fail("Existing GHerrit history section is malformed.")
        prefix = prefix[:history].rstrip() + prefix[download:]
        download = prefix.find(DOWNLOAD_MARKER)

    generated_history = history_section(
        gherrit_id=args.id,
        latest=args.latest,
        repo_url=args.repo_url.rstrip("/"),
        base_branch=args.base,
    )
    if generated_history:
        if download < 0:
            fail("Could not find the generated download section in the child PR body.")
        prefix = prefix[:download].rstrip() + generated_history + prefix[download:]

    metadata["parent"] = None
    warning = "<!-- WARNING: GHerrit relies on the following metadata to work properly. DO NOT EDIT OR REMOVE. -->"
    if not prefix.endswith(warning):
        prefix = prefix.rstrip() + "\n\n" + warning
    print(prefix + render_metadata(metadata), end="")


@dataclass
class TagObservation:
    direct: str | None = None
    peeled: str | None = None


def parse_version_observations(
    gherrit_id: str, lines: str
) -> dict[int, TagObservation]:
    validate_id(gherrit_id)
    observations: dict[int, TagObservation] = {}
    prefix = f"refs/tags/gherrit/{gherrit_id}/v"
    for line in lines.splitlines():
        try:
            oid, refname = line.split("\t", 1)
        except ValueError:
            continue
        peeled = refname.endswith("^{}")
        if peeled:
            refname = refname[:-3]
        if not refname.startswith(prefix):
            continue
        suffix = refname[len(prefix) :]
        if not suffix or not suffix.isascii() or not suffix.isdecimal():
            fail(f"Remote patch tag '{refname}' has a noncanonical version number.")
        version = int(suffix)
        if version < 1 or suffix != str(version):
            fail(f"Remote patch tag '{refname}' has a noncanonical version number.")
        observation = observations.setdefault(version, TagObservation())
        field = "peeled" if peeled else "direct"
        existing = getattr(observation, field)
        if existing is not None and existing != oid:
            fail(f"Remote patch tag '{refname}' was reported with conflicting object IDs.")
        setattr(observation, field, oid)

    if not observations:
        fail(f"Managed branch {gherrit_id} has no authoritative GHerrit patch-version tag.")
    latest = max(observations)
    for version in range(1, latest + 1):
        if version not in observations:
            fail(
                f"Remote patch history for GHerrit ID '{gherrit_id}' is missing authoritative version v{version} before v{latest}."
            )
        if observations[version].direct is None:
            fail(f"Remote patch tag gherrit/{gherrit_id}/v{version} has no tag ref object.")
    return observations


def tag_target(observation: TagObservation) -> str:
    return observation.peeled or observation.direct or fail(
        "Remote patch tag has no target object."
    )


def command_version_state(args: argparse.Namespace) -> None:
    observations = parse_version_observations(args.id, sys.stdin.read())
    latest = max(observations)
    target = tag_target(observations[latest])
    if target != args.expected_head:
        fail(
            f"Remote patch tag gherrit/{args.id}/v{latest} points to {target}, but managed branch {args.id} points to {args.expected_head}."
        )
    print(json.dumps({"latest": latest, "target": target, "next": latest + 1}, separators=(",", ":")))


def command_authenticate_version(args: argparse.Namespace) -> None:
    observations = parse_version_observations(args.id, sys.stdin.read())
    matching = [
        version
        for version, observation in observations.items()
        if tag_target(observation) == args.expected_target
    ]
    if not matching:
        fail(
            f"Commit {args.expected_target} is not authenticated by any authoritative patch-version tag for GHerrit ID '{args.id}'."
        )
    print(max(matching))


def main() -> None:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)

    metadata = commands.add_parser("metadata")
    metadata.set_defaults(function=command_metadata)

    trailer = commands.add_parser("trailer-id")
    trailer.set_defaults(function=command_trailer_id)

    promote = commands.add_parser("promote-body")
    promote.add_argument("--id", required=True)
    promote.add_argument("--parent", required=True)
    promote.add_argument("--latest", type=int, required=True)
    promote.add_argument("--repo-url", required=True)
    promote.add_argument("--base", required=True)
    promote.set_defaults(function=command_promote_body)

    versions = commands.add_parser("version-state")
    versions.add_argument("--id", required=True)
    versions.add_argument("--expected-head", required=True)
    versions.set_defaults(function=command_version_state)

    authenticate = commands.add_parser("authenticate-version")
    authenticate.add_argument("--id", required=True)
    authenticate.add_argument("--expected-target", required=True)
    authenticate.set_defaults(function=command_authenticate_version)

    args = parser.parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
