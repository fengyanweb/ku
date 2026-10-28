#!/usr/bin/env python3
"""Prepare and track an offline native-deployment RC; never publish a Release.

This is not the full compiler SDK exporter. The installed CLI is supplied for
service supervision and explicitly checked native-C builds. Default Rust-runner
RLIBs, VSIX and native-TLS packs are deliberately not promised by this bundle.
Dynamic service rehearsals are run by the existing deployment/acceptance tools;
verify/rehearse bind their execution receipts instead of replaying operations.
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
REPO = HERE.parent
SHA = re.compile(r"[0-9a-f]{64}\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")
NAME = re.compile(r"v0\.0\.18-rc\.[1-9][0-9]{0,5}\Z")
TARGET = "aarch64-unknown-linux-gnu"
KINDS = ("business", "lifecycle", "resource_limits", "observability", "rollback")
REAL_CHECKS = ("install_and_start_passed", "business_correctness_passed", "business_slo_passed",
               "resource_limits_verified", "health_and_observability_verified", "drain_and_restart_passed",
               "dependency_failure_passed", "rollback_rehearsed", "target_preflight_passed")
MAX_JSON = 2 * 1024 * 1024


class RCError(ValueError):
    pass


def exporter(repo=REPO):
    path = repo / ".github/scripts/export-release.py"
    if not path.is_file():
        # Installed deployment tools carry this exact helper beside the tool.
        path = HERE / "export-release.py"
    spec = importlib.util.spec_from_file_location("ku_rc_exporter", path)
    if spec is None or spec.loader is None:
        raise RCError("trusted archive helper is missing")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def command_owner():
    path = REPO / ".github/scripts/verify-native-three-os.py"
    if not path.is_file():
        path = HERE / "verify-native-three-os.py"
    spec = importlib.util.spec_from_file_location("ku_rc_process_owner", path)
    if spec is None or spec.loader is None:
        raise RCError("existing bounded process owner is missing")
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    module.COMMAND_TIMEOUT_SECONDS = 30
    module.MAX_CAPTURE_BYTES = 65536
    return module


def manager():
    path = HERE / "service/manage.py" if (HERE / "service/manage.py").is_file() else HERE / "manage.py"
    spec = importlib.util.spec_from_file_location("ku_rc_manage", path)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode()


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        while block := stream.read(1024 * 1024):
            h.update(block)
    return h.hexdigest()


def plain(path, directory=False):
    path = Path(path)
    # Resolve conventional OS parent aliases such as /var -> /private/var,
    # while refusing a substituted leaf or any reparse/symlink payload.
    if path.is_symlink():
        raise RCError(f"symlink input is not allowed: {path}")
    path = path.resolve(strict=True)
    info = path.stat()
    if not (stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)):
        raise RCError(f"not a plain {'directory' if directory else 'file'}: {path}")
    return path


def load(path):
    path = plain(path)
    if path.stat().st_size > MAX_JSON:
        raise RCError("JSON input exceeds 2MiB")
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise RCError(f"duplicate JSON key: {key}")
            result[key] = value
        return result
    def invalid(value):
        raise RCError(f"non-finite JSON number: {value}")
    result = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique,
                        parse_constant=invalid)
    if not isinstance(result, dict):
        raise RCError("JSON root must be an object")
    return result


def save(path, value):
    path = Path(path)
    temporary = path.with_name(path.name + ".writing")
    with temporary.open("xb") as stream:
        stream.write(encoded(value))
    temporary.replace(path)


def git(repo, *args):
    result = subprocess.run(["git", *args], cwd=repo, capture_output=True, timeout=10)
    if result.returncode:
        raise RCError("cannot read candidate Git identity")
    return result.stdout


def check_elf(path):
    with Path(path).open("rb") as stream:
        header = stream.read(64)
    if len(header) != 64 or header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != 183:
        raise RCError("native-deployment candidate requires a Linux ARM64 ELF")


def no_secrets(value):
    forbidden = {"password", "passwd", "secret", "token", "_token", "access_token", "private_key"}
    if isinstance(value, dict):
        for key, child in value.items():
            lower = key.lower()
            credential_key = lower in forbidden or "password" in lower or lower == "mysql_pwd" or lower.endswith(("_secret", "_token", "_private_key"))
            if credential_key and child not in (None, ""):
                raise RCError("configuration must reference external secret files, not contain secrets")
            no_secrets(child)
    elif isinstance(value, list):
        for child in value:
            no_secrets(child)
    elif isinstance(value, str) and (re.search(r"password\s*=\s*\S", value, re.I)
                                           or "PRIVATE KEY-----" in value):
        raise RCError("configuration contains inline credential material")


def binding(manifest):
    return {key: manifest[key] for key in ("candidate_commit", "cli_sha256", "application_sha256",
                                           "config_template_sha256", "archive_sha256")}


def derive_gate(gate):
    if gate.get("live_action_pending"):
        gate["overall"] = "blocked"
        gate["next_action"] = "Reconcile the recorded incomplete live action with actual deployment status; resume never replays it."
        return gate
    checks = gate["rc_checks"]
    required = ("identity", "install", *KINDS)
    if all(checks.get(name) is True for name in required):
        if gate["checks"].get("canary_actually_deployed") is True:
            gate["overall"] = "canary_observing"
            gate["next_action"] = "Read actual observation records; deployment does not certify observation completion."
        elif gate.get("real_profile_sha256") and all(gate["checks"].get(key) is True for key in REAL_CHECKS):
            gate["overall"] = "ready_for_canary"
            gate["next_action"] = "Review the bound canary action and authorization before cutover."
        else:
            gate["overall"] = "rc_prepared"
            gate["next_action"] = "Provide a real application, target, contract, rollback object and authorization before canary."
    elif any(checks.get(name) is False for name in required):
        gate["overall"] = "blocked"
        gate["next_action"] = "Resolve the recorded candidate failure; retain its evidence and prepare a corrected identity."
    else:
        gate["overall"] = "in_progress"
        gate["next_action"] = "Complete RC checks: " + ", ".join(name for name in required if checks.get(name) is not True)
    # Sample evidence never fills missing real-business or real-traffic facts.
    gate["checks"]["canary_actually_deployed"] = gate["checks"].get("canary_actually_deployed")
    return gate


def state(folder):
    folder = plain(folder, directory=True)
    gate = load(folder / "production-gate.json")
    manifest_path = folder / "release-manifest.json"
    if digest(manifest_path) != gate["release_manifest_sha256"]:
        raise RCError("release manifest differs from the bound gate")
    manifest = load(manifest_path)
    if manifest.get("format") != "ku-native-deployment-rc-v1" or manifest.get("target") != TARGET:
        raise RCError("unexpected RC manifest format/target")
    if not NAME.fullmatch(manifest.get("name", "")) or not COMMIT.fullmatch(manifest.get("candidate_commit", "")):
        raise RCError("invalid frozen candidate name/commit")
    for key in ("cli_sha256", "application_sha256", "config_template_sha256", "archive_sha256"):
        if not SHA.fullmatch(manifest.get(key, "")):
            raise RCError("invalid frozen artifact digest")
    helper = exporter()
    files = manifest.get("files")
    if not isinstance(files, dict) or not files or len(files) > helper.MAX_FILES:
        raise RCError("invalid RC member inventory")
    for name, spec in files.items():
        helper.checked_relative(name)
        if (not isinstance(spec, dict) or set(spec) != {"size", "sha256", "mode"}
                or type(spec["size"]) is not int or not 0 <= spec["size"] <= helper.MAX_BYTES
                or not SHA.fullmatch(spec["sha256"]) or spec["mode"] not in (0o644, 0o755)):
            raise RCError("invalid RC member identity")
    archive = plain(folder / manifest["archive"])
    if archive.parent != folder or digest(archive) != manifest["archive_sha256"]:
        raise RCError("candidate archive identity differs")
    return folder, manifest, gate


def prepare(args):
    repo = plain(args.repo, directory=True)
    if not NAME.fullmatch(args.name) or not COMMIT.fullmatch(args.candidate_commit):
        raise RCError("invalid RC name or candidate commit")
    if git(repo, "rev-parse", "HEAD").decode().strip() != args.candidate_commit:
        raise RCError("candidate commit must equal the actual checkout HEAD")
    ku, app, source = (plain(path) for path in (args.ku, args.app, args.source))
    for path, expected in ((ku, args.ku_sha256), (app, args.app_sha256)):
        if not SHA.fullmatch(expected) or digest(path) != expected:
            raise RCError("compiler/application SHA does not match the supplied artifact")
        check_elf(path)
    config = load(args.config)
    no_secrets(config)
    recipe = load(args.build_record)
    no_secrets(recipe)
    if recipe.get("cli_sha256") != args.ku_sha256 or recipe.get("application_sha256") != args.app_sha256:
        raise RCError("build record does not bind the supplied CLI/application")
    for key in ("cli_source_commit", "application_source_sha256", "commands", "toolchain", "runtime_dependencies"):
        if not recipe.get(key):
            raise RCError(f"build record is missing {key}")
    if recipe["application_source_sha256"] != digest(source):
        raise RCError("application source differs from the build record")
    sysroot = plain(args.rust_sysroot, directory=True)
    toolchain = recipe["toolchain"]
    if not isinstance(toolchain, dict) or not isinstance(toolchain.get("rustc"), str):
        raise RCError("build record toolchain.rustc must identify the CLI build compiler")
    rust = subprocess.run([str(sysroot / "bin/rustc"), "--version"], capture_output=True, timeout=5)
    rust_version = rust.stdout.decode().strip()
    if rust.returncode or rust_version != toolchain["rustc"].splitlines()[0]:
        raise RCError("Rust attribution sysroot differs from the recorded CLI compiler")
    out = args.output.absolute()
    if os.path.lexists(out):
        raise RCError("RC output must be a fresh directory")
    plain(out.parent, directory=True)
    helper = exporter(repo)
    payload_sources = {"tools/production_rc.py": repo / "deploy/production_rc.py",
        "tools/manage.py": repo / "deploy/service/manage.py",
        "tools/export-release.py": repo / ".github/scripts/export-release.py",
        "tools/verify-native-three-os.py": repo / ".github/scripts/verify-native-three-os.py"}
    for path in payload_sources.values():
        relative = path.relative_to(repo).as_posix()
        if git(repo, "show", f"HEAD:{relative}") != path.read_bytes():
            raise RCError(f"packaged deployment source is uncommitted: {relative}")
    with tempfile.TemporaryDirectory(prefix="ku-native-rc-") as temporary:
        root = Path(temporary).resolve(); staging = root / "payload"; staging.mkdir()
        stage = helper.Stager(staging)
        stage.add("bin/ku", source=ku); stage.add("app/service", source=app)
        stage.add("source/service.ku", source=source)
        stage.add("source/hello.ku", data=helper.canonical_example((repo / "examples/hello.ku").read_bytes()))
        stage.add("config.template.json", data=encoded(config))
        stage.add("build-record.json", data=encoded(recipe))
        for name, path in payload_sources.items():
            stage.add(name, source=path)
        extras = []
        for item in args.extra_file:
            name, separator, origin = item.partition("=")
            if not separator:
                raise RCError("--extra-file requires ARCHIVE_RELATIVE=REGULAR_FILE")
            extra = plain(origin); helper.checked_relative(name)
            if extra.name.lower() in ("db.conn", ".env", ".pgpass", "token", "service.token") or extra.suffix.lower() in (".key", ".pem"):
                raise RCError("secret/runtime files must not enter a deployment archive")
            if extra.is_relative_to(repo) and git(repo, "show", "HEAD:" + extra.relative_to(repo).as_posix()) != extra.read_bytes():
                raise RCError("supplemental repository asset is not committed")
            if extra.suffix.lower() == ".json":
                no_secrets(load(extra))
            stage.add(name, source=extra)
            extras.append({"member": name, "origin": str(extra), "sha256": stage.files[name]["sha256"], "role": "explicit supplemental artifact; not an execution instruction"})
        stage.add("production-runbook.md", source=plain(args.runbook))
        attribution = helper.collect_licenses(stage, plain(args.cargo_metadata), repo, sysroot)
        attribution["rust"].update(required_toolchain=None, material_toolchain=rust_version,
            scope="Original materials match the CLI build compiler; precompiled app/ku service do not require rustc at runtime. Default Rust runner SDK is not supplied.")
        stage.add("THIRD_PARTY.json", data=helper.json_bytes(attribution))
        stage.add("KU-LICENSE-NOTICE.txt", data=b"Ku's own code remains UNLICENSED. No open-source license grant for Ku is included. Third-party licenses apply only to their components.\n")
        scope = {"format": "ku-native-deployment-rc-v1", "name": args.name,
            "target": TARGET, "candidate_commit": args.candidate_commit,
            "cli_sha256": args.ku_sha256, "application_sha256": args.app_sha256,
            "config_template_sha256": hashlib.sha256(encoded(config)).hexdigest(),
            "application": None, "deployment_target": None, "business_contract_sha256": None,
            "sample_only": True, "published": False, "full_compiler_SDK": False,
            "supported_CLI_scope": "ku service plus explicitly verified native-C checks; no default Rust runner or native-TLS SDK promise"}
        stage.add("BUNDLE.json", data=encoded(scope))
        instructions = (f"# {args.name} native deployment ({TARGET})\n\n"
            "Offline candidate, not a public Release or a full compiler SDK. Keep the entire extracted layout.\n"
            "Use bin/ku service for the supplied precompiled app/service. Use external private configuration/token files.\n"
            "External libpq/libc/other dynamic libraries are not bundled; target preflight must check their actual versions/ldd.\n"
            "The sample contract is not a real-business SLO or permission to deploy. No automatic database replay.\n"
            "Use tools/production_rc.py preflight with the original state folder for an isolated install/smoke check.\n"
            "See production-runbook.md, build-record.json and the external release-manifest.json.\n")
        stage.add("README-INSTALL.md", data=instructions.encode())
        sums = "".join(f"{spec['sha256']}  {name}\n" for name, spec in sorted(stage.files.items()))
        stage.add("SHA256SUMS", data=sums.encode())
        filename = f"ku-{args.name}-{TARGET}-native-deployment.tar.gz"
        archive = root / filename
        with archive.open("xb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.USTAR_FORMAT) as tar:
                for name, record in sorted(stage.files.items()):
                    member = tarfile.TarInfo(name); member.size = record["size"]; member.mode = record["mode"]
                    with (staging / name).open("rb") as stream:
                        tar.addfile(member, stream)
        verified = root / "verified"; verified.mkdir()
        helper.verify_archive(archive, stage.files, verified)
        manifest = {**scope, "archive": filename, "archive_sha256": digest(archive),
            "archive_bytes": archive.stat().st_size, "files": stage.files,
            "build_record_sha256": hashlib.sha256(encoded(recipe)).hexdigest(),
            "extra_files": extras,
            "worktree_status": git(repo, "status", "--porcelain=v1", "--untracked-files=all").decode(),
            "recipe": recipe, "generated_unix_seconds": time.time()}
        out.mkdir(mode=0o700)
        shutil.copyfile(archive, out / filename)
        (out / "release-manifest.json").write_bytes(encoded(manifest))
        gate = {"schema": "ku-production-rc-gate-v1", "overall": "in_progress", "scope": scope,
            "release_manifest_sha256": digest(out / "release-manifest.json"),
            "checks": {key: None for key in ("identity_bound", "install_and_start_passed",
                "business_correctness_passed", "business_slo_passed", "resource_limits_verified",
                "health_and_observability_verified", "drain_and_restart_passed", "dependency_failure_passed",
                "rollback_rehearsed", "target_preflight_passed", "canary_authorized", "canary_actually_deployed",
                "observation_complete", "canary_slo_passed")},
            "rc_checks": {"identity": True, "install": None, **{key: None for key in KINDS}},
            "existing_evidence": [], "new_evidence": [], "external_dependencies": [
                "Real application, machine/access, write risk, traffic/SLO, rollback object and cutover authorization are not supplied."],
            "unresolved_issues": [], "previous_performance_gate": {"status": "not_accepted", "modified_by_this_task": False},
            "receipt_semantics": "Local execution receipts bind identity; not third-party attestation. Sample receipts do not fill real-traffic fields."}
        gate["checks"]["identity_bound"] = True
        save(out / "production-gate.json", derive_gate(gate))
    return {"state": str(out), "archive_sha256": manifest["archive_sha256"], "overall": gate["overall"]}


def installed_matches(prefix, manifest, gate):
    prefix = plain(prefix, directory=True)
    owner = load(prefix / ".ku-rc-install-owner.json")
    if owner.get("release_manifest_sha256") != gate["release_manifest_sha256"]:
        raise RCError("installation belongs to another candidate")
    for name, record in manifest["files"].items():
        path = plain(prefix / name)
        if not path.is_relative_to(prefix) or path.stat().st_size != record["size"] or digest(path) != record["sha256"]:
            raise RCError(f"installed candidate differs: {name}")
    return prefix


def slot_program(prefix, program, slot, expected_sha256):
    if slot not in ("a", "b", "spare"):
        raise RCError("RC canary supports only the explicitly prepared a/b/spare slots")
    expected = prefix / "runtime/slots" / slot / "service"
    program = plain(program)
    if (program != expected or digest(program) != expected_sha256
            or digest(plain(prefix / "app/service")) != expected_sha256):
        raise RCError("slot copy differs from the installed candidate or approved runtime slot")
    return program


def configured_cli(config, prefix, manifest):
    cli = plain(config["ku"])
    if cli != prefix / "bin/ku" or digest(cli) != manifest["cli_sha256"]:
        raise RCError("deployment configuration does not use the bound installed CLI")
    return cli


def preflight(args):
    folder, manifest, gate = state(args.state)
    if gate["rc_checks"].get("install") is False and not args.correction_reason:
        raise RCError("previous install failed; record --correction-reason before another installation attempt")
    if (platform.system(), platform.machine().lower()) not in (("Linux", "aarch64"), ("Linux", "arm64")):
        raise RCError("this candidate must be installed and checked on native Linux ARM64")
    prefix = args.install_root.absolute()
    if os.path.lexists(prefix):
        raise RCError("install prefix already exists; refusing to overwrite or assume ownership")
    plain(prefix.parent, directory=True); prefix.mkdir(mode=0o700)
    record = {"schema": "ku-rc-install-v1", "status": "running", "prefix": str(prefix),
              "binding": binding(manifest), "checks": [], "dynamic_dependencies": {},
              "correction_reason": args.correction_reason, "host": {"system": platform.system(), "machine": platform.machine()}}
    helper = exporter()
    owner = command_owner()
    try:
        helper.verify_archive(folder / manifest["archive"], manifest["files"], prefix)
        (prefix / ".ku-rc-install-owner.json").write_bytes(encoded({
            "release_manifest_sha256": gate["release_manifest_sha256"], "archive_sha256": manifest["archive_sha256"]}))
        for name in ("bin/ku", "app/service"):
            result = owner.run_bounded(["ldd", str(prefix / name)], prefix, "RC installed dependencies")
            output = result.stdout + result.stderr
            if result.returncode or len(output) > 65536 or b"not found" in output:
                raise RCError(f"dynamic dependency check failed: {name}")
            record["dynamic_dependencies"][name] = output.decode(errors="replace")
        with tempfile.TemporaryDirectory(prefix="ku-rc-install-smoke-") as temporary:
            work = Path(temporary).resolve(); shutil.copyfile(prefix / "source/hello.ku", work / "hello.ku")
            checks = (("version", ["--version"], "ku 0.0.18\n"),
                      ("check-hello", ["check", "hello.ku"], "check ok: hello.ku\n"),
                      ("native-build-hello", ["build", "--backend", "c", "--profile", "release", "hello.ku", "-o", "hello"], None))
            for name, arguments, expected in checks:
                completed = owner.run_bounded([str(prefix / "bin/ku"), *arguments], work, name)
                if completed.returncode or completed.stderr:
                    raise RCError(f"installed command failed: {name} exit{completed.returncode}")
                output = completed.stdout.decode().replace("\r\n", "\n")
                result = {"exit_code": completed.returncode, "stdout_bytes": len(completed.stdout),
                          "stdout_sha256": hashlib.sha256(completed.stdout).hexdigest(), "stderr_bytes": 0}
                if expected is not None and output != expected:
                    raise RCError(f"unexpected installed command result: {name}")
                record["checks"].append({"name": name, **result})
            completed = owner.run_bounded([str(work / "hello")], work, "run-native-hello")
            if completed.returncode or completed.stderr:
                raise RCError("installed native hello failed")
            output = completed.stdout.decode().replace("\r\n", "\n")
            result = {"exit_code": completed.returncode, "stdout_bytes": len(completed.stdout),
                      "stdout_sha256": hashlib.sha256(completed.stdout).hexdigest(), "stderr_bytes": 0}
            if output != "Hello Ku\n":
                raise RCError("installed native hello returned wrong result")
            record["checks"].append({"name": "run-native-hello", **result})
        installed_matches(prefix, manifest, gate)
        record["status"] = "passed"; gate["rc_checks"]["install"] = True
        gate["installed_prefix"] = str(prefix)
    except BaseException as error:
        record.update(status="failed", error=f"{type(error).__name__}: {error}"[:1000])
        gate["rc_checks"]["install"] = False
        raise
    finally:
        record["finished_unix_seconds"] = time.time()
        raw = encoded(record); recorded_sha = hashlib.sha256(raw).hexdigest()
        history = folder / "evidence"; history.mkdir(exist_ok=True)
        historical = history / f"install-{recorded_sha[:16]}.json"
        if not historical.exists():
            historical.write_bytes(raw)
        elif historical.read_bytes() != raw:
            raise RCError("installation receipt filename collision")
        save(folder / "installation-check.json", record)
        gate["new_evidence"].append({"kind": "install", "path": historical.relative_to(folder).as_posix(), "sha256": recorded_sha})
        save(folder / "production-gate.json", derive_gate(gate))
    return record


def attach(args):
    folder, manifest, gate = state(args.state)
    receipt = load(args.receipt)
    if receipt.get("schema") != "ku-production-rc-receipt-v1" or receipt.get("kind") not in (*KINDS, "target_preflight"):
        raise RCError("unknown RC execution receipt")
    kind = receipt["kind"]
    if args.command == "rehearse" and kind not in ("lifecycle", "rollback"):
        raise RCError("rehearse accepts only actual lifecycle/rollback receipts")
    if receipt.get("binding") != binding(manifest):
        raise RCError("receipt belongs to a different frozen candidate/configuration")
    outcome = receipt.get("outcome")
    if (outcome not in ("passed", "failed", "unknown")
            or not isinstance(receipt.get("observed"), dict) or not receipt["observed"]):
        raise RCError("receipt must contain actual observations and an explicit outcome")
    evidence = receipt.get("evidence")
    if not isinstance(evidence, list) or not evidence or len(evidence) > 32:
        raise RCError("receipt needs bounded original evidence references")
    for row in evidence:
        if not isinstance(row, dict) or not SHA.fullmatch(row.get("sha256", "")) or digest(plain(row["path"])) != row["sha256"]:
            raise RCError("original execution evidence differs or is missing")
    value = {"passed": True, "failed": False, "unknown": None}[outcome]
    if gate["rc_checks"].get(kind) is False and value is True:
        raise RCError("a failed frozen candidate cannot become passed by selecting another unchanged run")
    if kind == "target_preflight":
        if not args.profile:
            raise RCError("a real target receipt requires --profile")
        profile = load(args.profile)
        for key in ("application", "deployment_target", "business_contract_sha256", "config_sha256", "rollback_object"):
            if not profile.get(key):
                raise RCError(f"real profile missing {key}")
        if (profile.get("candidate_binding") != binding(manifest)
                or receipt.get("profile_sha256") != digest(plain(args.profile))):
            raise RCError("target receipt profile or candidate identity differs")
        real = receipt.get("checks", {})
        if set(real) != set(REAL_CHECKS) or any(real[key] is not None and type(real[key]) is not bool for key in real):
            raise RCError("target checks must explicitly enumerate booleans/null; no missing field is passed")
        checked_outcome = "failed" if any(real[key] is False for key in REAL_CHECKS) else "passed" if all(real[key] is True for key in REAL_CHECKS) else "unknown"
        if outcome != checked_outcome:
            raise RCError("target outcome must agree with its explicit true/false/null checks")
        if any(gate["checks"].get(key) is False and real[key] is True for key in REAL_CHECKS):
            raise RCError("a failed frozen target check needs a corrected candidate, not another selected pass")
        gate["real_profile_sha256"] = receipt["profile_sha256"]
        gate["scope"].update({key: profile[key] for key in ("application", "deployment_target", "business_contract_sha256")})
        gate["checks"].update({key: False if gate["checks"].get(key) is False else real[key] for key in REAL_CHECKS})
    else:
        if gate["rc_checks"].get(kind) is not False:
            gate["rc_checks"][kind] = value
    # Invalid target/profile input must not create orphan receipt files.
    destination = folder / "evidence"; destination.mkdir(exist_ok=True)
    raw = encoded(receipt); hashed = hashlib.sha256(raw).hexdigest(); path = destination / f"{kind}-{hashed[:16]}.json"
    if path.exists() and path.read_bytes() != raw:
        raise RCError("receipt filename collision")
    if not path.exists():
        path.write_bytes(raw)
    row = {"kind": kind, "path": path.relative_to(folder).as_posix(), "sha256": hashed, "outcome": outcome}
    if row not in gate["new_evidence"]:
        gate["new_evidence"].append(row)
    save(folder / "production-gate.json", derive_gate(gate))
    return gate


def inspect(args):
    folder, manifest, gate = state(args.state)
    for row in gate["new_evidence"]:
        exporter().checked_relative(row["path"])
        if digest(plain(folder / row["path"])) != row["sha256"]:
            raise RCError("saved execution receipt changed")
    gate["installation_status_scope"] = "Saved isolated installation evidence only; status/resume do not assert a host is live or access a prefix recorded on another machine. Canary rechecks installed bytes on its bound target."
    derive_gate(gate)
    if args.command == "resume":
        save(folder / "production-gate.json", gate)
    return gate


def observer_verified(profile, manifest, gate):
    observer = profile.get("observer", {})
    if not isinstance(observer, dict) or not observer.get("receipt") or not isinstance(observer.get("identity"), dict):
        raise RCError("an existing continuous observer identity/receipt is required; this command starts no observer")
    receipt_path = plain(observer["receipt"])
    if not SHA.fullmatch(observer.get("sha256", "")) or digest(receipt_path) != observer["sha256"]:
        raise RCError("existing observer receipt differs")
    receipt = load(receipt_path); identity = receipt.get("identity", {})
    if (receipt.get("schema") != "ku-production-observer-v1" or receipt.get("binding") != binding(manifest)
            or receipt.get("target_machine_id_sha256") != profile["deployment_target"]["machine_id_sha256"]
            or receipt.get("config_sha256") != profile["config_sha256"]
            or receipt.get("continuous_collection_running") is not True or identity != observer.get("identity")):
        raise RCError("observer receipt does not bind this candidate/target/live collector")
    sampled = receipt.get("last_sample_clock_monotonic_ns")
    now = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
    if type(sampled) is not int or not 0 <= now - sampled <= 60_000_000_000:
        raise RCError("observer has no verified sample in the last60s")
    pid = identity.get("pid")
    if type(pid) is not int or pid <= 0:
        raise RCError("observer PID is invalid")
    process = Path("/proc") / str(pid)
    started = int((process / "stat").read_bytes().rsplit(b") ", 1)[1].split()[19])
    command = (process / "cmdline").read_bytes()
    if (started != identity.get("start_ticks") or hashlib.sha256(command).hexdigest() != identity.get("cmdline_sha256")
            or digest(Path(os.readlink(process / "exe"))) != identity.get("executable_sha256")):
        raise RCError("observer process identity changed or is no longer running")
    installed = installed_matches(gate["installed_prefix"], manifest, gate)
    monitor = installed / "tools/monitor.py"
    if ("tools/monitor.py" not in manifest["files"] or str(monitor).encode() not in command.split(b"\0")
            or digest(plain(monitor)) != manifest["files"]["tools/monitor.py"]["sha256"]):
        raise RCError("observer is not the manifest-bound installed monitor")
    if int((process / "stat").read_bytes().rsplit(b") ", 1)[1].split()[19]) != started:
        raise RCError("observer identity changed during its readback")


def live_action(args):
    folder, manifest, gate = state(args.state)
    if gate.get("live_action_pending"):
        raise RCError("previous live action outcome is unresolved; no automatic replay")
    profile = load(args.profile); no_secrets(profile)
    for key in ("application", "deployment_target", "business_contract_sha256", "config_sha256", "rollback_object"):
        if not profile.get(key):
            raise RCError(f"real deployment profile is missing {key}; remain rc_prepared")
    config = plain(args.config)
    expected_config = profile["config_sha256"]
    if args.command == "rollback" and gate.get("last_live_action", {}).get("action") == "canary":
        expected_config = gate["last_live_action"]["config_sha256_after"]
    if digest(config) != expected_config:
        raise RCError("target configuration differs from the approved profile")
    if profile.get("candidate_binding") != binding(manifest):
        raise RCError("real deployment profile identifies another candidate")
    approval = hashlib.sha256(encoded({"action": args.command, "binding": binding(manifest),
        "profile_sha256": digest(plain(args.profile)), "config_sha256": digest(config)})).hexdigest()
    if args.execution_approval != approval:
        raise RCError("explicit reviewed action approval is required; no deployment was performed; action SHA=" + approval)
    if profile.get("authorization", {}).get(args.command) is not True:
        raise RCError("human authorization for this exact action is not recorded")
    target = profile["deployment_target"]
    if not isinstance(target, dict) or not SHA.fullmatch(target.get("machine_id_sha256", "")):
        raise RCError("deployment target must identify one machine fingerprint")
    machine = Path("/etc/machine-id")
    if not machine.is_file() or target.get("machine_id_sha256") != digest(machine):
        raise RCError("this host is not the bound deployment target")
    if (gate["overall"] not in ("ready_for_canary", "canary_observing")
            or not all(gate["checks"].get(key) is True for key in REAL_CHECKS)
            or gate.get("real_profile_sha256") != digest(plain(args.profile))):
        raise RCError("bound real-target/business preflight has not completed")
    if args.command == "canary":
        if profile.get("traffic_mode") != "entire_slot":
            raise RCError("this operation replaces an entire slot; percentage-weight canaries are not implemented")
        observer_verified(profile, manifest, gate)
    module = manager()
    action = profile.get("canary_action", {})
    with module.locked(config):
        if digest(config) != expected_config:
            raise RCError("deployment configuration changed before lock acquisition")
        cfg = module.load(config)
        installed = installed_matches(gate["installed_prefix"], manifest, gate)
        configured_cli(cfg, installed, manifest)
        gate["live_action_pending"] = {"action": args.command, "approval_sha256": approval,
            "config_sha256_before": expected_config, "started_unix_seconds": time.time(),
            "status": "in_progress_or_unknown_after_interruption"}
        if args.command == "canary":
            program = slot_program(installed, action["program"], action["new_slot"], manifest["application_sha256"])
            slot = module.slot_named(cfg, action["new_slot"])
            if Path(slot["program"]).resolve().parent != program.parent:
                raise RCError("configuration points outside the bound runtime slot")
            save(folder / "production-gate.json", gate)
            result = module.roll(cfg, action["old_slot"], action["new_slot"], program)
            gate["overall"] = "canary_observing"; gate["checks"].update(canary_authorized=True, canary_actually_deployed=True)
        else:
            prior = cfg["last_deployment"]
            old = plain(prior["old_program"]); fallback = profile["rollback_object"]
            if (not isinstance(fallback, dict) or fallback.get("kind") != "same_cli_application"
                    or fallback.get("cli_sha256") != manifest["cli_sha256"]
                    or str(old) != fallback.get("program") or digest(old) != fallback.get("program_sha256")):
                raise RCError("rollback needs a separately identified old application; CLI rollback needs a separate verified deployment state")
            save(folder / "production-gate.json", gate)
            result = module.roll(cfg, prior["new_slot"], prior["old_slot"], prior["old_program"], rollback=True)
            gate["overall"] = "ready_for_canary"; gate["checks"]["canary_actually_deployed"] = False
    # Starting traffic does not create an observer or certify its completion.
    gate["checks"]["observation_complete"] = None
    gate.pop("live_action_pending", None)
    gate["last_live_action"] = {"action": args.command, "approval_sha256": approval, "result": result,
                                "unix_seconds": time.time(), "config_sha256_after": digest(config),
                                "automatic_observer_created": False}
    save(folder / "production-gate.json", gate)
    return gate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("prepare")
    for key in ("ku", "app", "source", "config", "build-record", "cargo-metadata", "rust-sysroot", "runbook", "output"):
        p.add_argument("--" + key, required=True, type=Path)
    p.add_argument("--repo", type=Path, default=REPO)
    p.add_argument("--name", default="v0.0.18-rc.1")
    p.add_argument("--extra-file", action="append", default=[], help="Explicit ARCHIVE_RELATIVE=REGULAR_FILE; no recursive runtime copy")
    for key in ("candidate-commit", "ku-sha256", "app-sha256"):
        p.add_argument("--" + key, required=True)
    p = sub.add_parser("preflight"); p.add_argument("--state", type=Path, required=True); p.add_argument("--install-root", type=Path, required=True)
    p.add_argument("--correction-reason", default="")
    for name in ("verify", "rehearse"):
        p = sub.add_parser(name); p.add_argument("--state", type=Path, required=True); p.add_argument("--receipt", type=Path, required=True)
        p.add_argument("--profile", type=Path)
    for name in ("status", "resume"):
        p = sub.add_parser(name); p.add_argument("--state", type=Path, required=True)
    for name in ("canary", "rollback"):
        p = sub.add_parser(name)
        for key in ("state", "profile", "config"):
            p.add_argument("--" + key, required=True, type=Path)
        p.add_argument("--execution-approval", default="")
    args = parser.parse_args()
    try:
        def dispatch():
            return (prepare(args) if args.command == "prepare" else preflight(args) if args.command == "preflight"
                    else attach(args) if args.command in ("verify", "rehearse") else inspect(args)
                    if args.command in ("status", "resume") else live_action(args))
        if args.command in ("prepare", "status"):
            result = dispatch()
        else:
            with manager().locked(plain(args.state, directory=True) / "production-gate.json"):
                result = dispatch()
    except (RCError, OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"production RC failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
