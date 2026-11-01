#!/usr/bin/env python3
"""One serial Linux libpq build/FFI gate; never starts a server or benchmark."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import tarfile
import urllib.request

SOURCE_URL = "https://ftp.postgresql.org/pub/source/v18.6/postgresql-18.6.tar.bz2"
SOURCE_SHA = "555610c24d53e4316da5b7d3fc25c279d96856d5e0e23ee308c328c5fa881d9f"
PATCH_SHA = "7af0d6cfe39c55e5a8d0d56123a74ebfeb12e677a83ffdc5d9a277daa81d927f"


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def write_json(path, data):
    Path(path).write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def run(command, log, cwd=None, env=None, timeout=360):
    print("RUN " + shlex.join(str(x) for x in command), flush=True)
    with Path(log).open("w") as output:
        output.write("COMMAND " + json.dumps([str(x) for x in command]) + "\n")
        output.flush()
        result = subprocess.run(command, cwd=cwd, env=env, stdout=output,
                                stderr=subprocess.STDOUT, timeout=timeout)
    if result.returncode:
        print(Path(log).read_text()[-16000:], flush=True)
        raise RuntimeError("command failed: " + str(log))
    return Path(log).read_text()


def capture(command, cwd=None, env=None):
    return subprocess.check_output(command, cwd=cwd, env=env, text=True, timeout=60)


def needed(path):
    return re.findall(r"Shared library: \[(.*?)\]", capture(["readelf", "-d", str(path)]))


def undefined(path):
    return sorted(re.findall(r"\bU\s+(\S+)", capture(["nm", "-u", str(path)])))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux" or platform.machine() not in ("aarch64", "arm64"):
        raise RuntimeError("this gate is Linux AArch64 only")
    if "GNUTLS_SKIP_GLOBAL_INIT" in os.environ:
        raise RuntimeError("GnuTLS initialization bypass must be absent")
    repo = Path(__file__).resolve().parents[2]
    fixture = repo / ".github/fixtures/libpq-lazy-ldap"
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    work = output.parent / "libpq-loader-private-build"
    work.mkdir(exist_ok=False)
    identity = {
        "tools_commit": capture(["git", "rev-parse", "HEAD"], cwd=repo).strip(),
        "source_url": SOURCE_URL, "source_sha256": SOURCE_SHA,
        "patch_sha256": PATCH_SHA, "platform": platform.platform(),
        "scope": "Make shared/static and real FFI; no server or performance",
        "meson_real_build": "NOT RUN; custom-prefix partial dependency remains unverified",
        "compiler": capture(["clang", "--version"]),
        "compiler_path": shutil.which("clang"),
        "compiler_sha256": digest(Path(shutil.which("clang")).resolve()),
        "make": capture(["make", "--version"]),
        "pkg_config": capture(["pkg-config", "--version"]),
        "distro_pg_version": capture(["pg_config", "--version"]),
        "distro_pg_configure": capture(["pg_config", "--configure"]),
        "distro_packages": capture(["dpkg-query", "-W", "libpq5", "libpq-dev", "libldap2-dev",
                                      "libssl-dev", "libkrb5-dev", "libcurl4-openssl-dev"]),
        "historical_distro_D_sha256": "b59de287c314a78cb70e7e0bb31fb2be34b961ccc90beff405d8c48412ace2df",
        "comparison_rule": "Only same-source A/B is causal. Current or historical distro D is separate.",
    }
    distro = Path(capture(["pg_config", "--libdir"]).strip()) / "libpq.so"
    identity["current_distro_D"] = {"path": str(distro.resolve()), "sha256": digest(distro),
                                     "DT_NEEDED": needed(distro)}
    write_json(output / "identities.json", identity)
    archive = work / "postgresql-18.6.tar.bz2"
    urllib.request.urlretrieve(SOURCE_URL, archive)
    assert digest(archive) == SOURCE_SHA, "official source checksum mismatch"
    with tarfile.open(archive) as source:
        source.extractall(work, filter="data")
    source = work / "postgresql-18.6"
    expected = json.loads((fixture / "source-identities.json").read_text())["files"]
    for name, hashes in expected.items():
        assert digest(source / name) == hashes["before_sha256"], name
    patch = fixture / "libpq-18.6-lazy-ldap.patch"
    assert digest(patch) == PATCH_SHA
    run(["patch", "-p1", "-i", str(patch)], output / "patch.log", cwd=source)
    for name, hashes in expected.items():
        assert digest(source / name) == hashes["after_sha256"], name
    write_json(output / "source-files.json", expected)
    prefix = work / "common-install"
    configure = [str(source / "configure"), "--prefix=" + str(prefix),
                 "--with-ssl=openssl", "--with-gssapi", "--with-ldap", "--with-libcurl"]
    base_env = dict(os.environ, CC="clang", CFLAGS="-O2 -g -fno-omit-frame-pointer")
    identity.update({"configure_command_identical_A_B": configure,
                     "CFLAGS": base_env["CFLAGS"], "CPPFLAGS": base_env.get("CPPFLAGS", ""),
                     "LDFLAGS": base_env.get("LDFLAGS", ""),
                     "pkg_config_path": base_env.get("PKG_CONFIG_PATH", "")})
    write_json(output / "identities.json", identity)
    records = {}
    for label, dynamic in (("A", "0"), ("B", "1")):
        dest = output / label
        dest.mkdir()
        build = work / ("build-" + label)
        build.mkdir()
        run(configure, dest / "configure.log", cwd=build, env=base_env)
        command = ["make", "-j2", "-C", "src/interfaces/libpq", "DYNAMIC_LDAP=" + dynamic, "all"]
        build_log = run(command, dest / "make.log", cwd=build, env=base_env)
        variable_records = {}
        for suffix in ("shared", "pc"):
            target = "ku-loader-vars." + suffix
            rule = target + ":\n\t@printf '%s\\n' 'flavor=$(flavor SHLIB_LINK)' 'link=$(SHLIB_LINK)'\n"
            text = run(["make", "--no-print-directory", "-C", "src/interfaces/libpq",
                        "DYNAMIC_LDAP=" + dynamic, "--eval=" + rule, target],
                       dest / ("make-vars-" + suffix + ".log"), cwd=build, env=base_env)
            variable_records[suffix] = [line for line in text.splitlines() if line.startswith(("flavor=", "link="))]
            assert "flavor=recursive" in variable_records[suffix]
        for subdir in ("src/common", "src/port", "src/interfaces/libpq", "src/interfaces/libpq-oauth"):
            run(["make", "-j2", "-C", subdir, "DYNAMIC_LDAP=" + dynamic, "install"],
                dest / (subdir.replace("/", "-") + "-install.log"), cwd=build, env=base_env)
        shared = prefix / "lib/libpq.so.5.18"
        static = prefix / "lib/libpq.a"
        ordinary = build / "src/interfaces/libpq/fe-connect.o"
        shared_object = build / ("src/interfaces/libpq/fe-connect_shlib.o" if dynamic == "1"
                                else "src/interfaces/libpq/fe-connect.o")
        for name, path in (("libpq.so.5.18", shared), ("libpq.a", static),
                           ("fe-connect-static.o", ordinary), ("fe-connect-shared.o", shared_object)):
            shutil.copy2(path, dest / name)
        shutil.copy2(prefix / "lib/pkgconfig/libpq.pc", dest / "libpq.pc")
        env = dict(base_env, LD_LIBRARY_PATH=str(prefix / "lib"),
                   PKG_CONFIG_PATH=str(prefix / "lib/pkgconfig") + os.pathsep + base_env.get("PKG_CONFIG_PATH", ""))
        pc = capture(["pkg-config", "--static", "--libs", "libpq"], env=env)
        (dest / "pkg-config-static.txt").write_text(pc)
        (dest / "configure.status").write_text(capture(["./config.status", "--config"], cwd=build))
        run(["readelf", "-d", str(shared)], dest / "DT_NEEDED.log")
        run(["nm", "-u", str(static)], dest / "static-undefined.log")
        members = run(["ar", "t", str(static)], dest / "static-members.log")
        assert "fe-connect_shlib.o" not in members
        archived_connect = subprocess.check_output(["ar", "p", str(static), "fe-connect.o"], timeout=60)
        assert hashlib.sha256(archived_connect).hexdigest() == digest(ordinary)
        exports = sorted(line.split()[-1] for line in capture(["nm", "-D", "--defined-only", str(shared)]).splitlines())
        commands = [line for line in build_log.splitlines() if "fe-connect.c" in line and " -c " in line]
        assert commands, "no actual fe-connect compile command retained"
        ordinary_commands = [line for line in commands if not re.search(r"-o\s+\S*fe-connect_shlib.o", line)]
        assert ordinary_commands and all("USE_DYNAMIC_LDAP" not in x for x in ordinary_commands)
        if dynamic == "1":
            assert any("fe-connect_shlib.o" in x and "-DUSE_DYNAMIC_LDAP" in x for x in commands)
        else:
            assert all("USE_DYNAMIC_LDAP" not in x for x in commands)
        record = {"shared_sha256": digest(shared), "static_sha256": digest(static),
                  "DT_NEEDED": needed(shared), "static_object_undefined": undefined(ordinary),
                  "shared_object_undefined": undefined(shared_object), "exports": exports,
                  "fe_connect_compile_commands": commands, "pkg_config_static_tokens": shlex.split(pc),
                  "evaluated_Make_variables": variable_records,
                  "configure_flags": capture(["./config.status", "--config"], cwd=build).strip()}
        assert "ldap_init" in record["static_object_undefined"]
        assert "dlopen" not in record["static_object_undefined"]
        assert ("libldap.so.2" in record["DT_NEEDED"]) == (dynamic == "0")
        assert ("ldap_init" in record["shared_object_undefined"]) == (dynamic == "0")
        if dynamic == "1":
            assert "dlopen" in record["shared_object_undefined"]
        probe = dest / "explicit-ffi-smoke"
        run(["clang", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread",
             str(fixture / "explicit-ffi-smoke.c"), str(shared), "-ldl", "-o", str(probe)], dest / "probe-build.log")
        static_probe = dest / "static-consumer-smoke"
        static_libs = [token for token in shlex.split(pc) if token != "-lpq"]
        run(["clang", "-std=c11", "-O2", "-pthread", str(fixture / "explicit-ffi-smoke.c"),
             str(static)] + static_libs + ["-ldl", "-o", str(static_probe)], dest / "static-consumer-build.log")
        assert "libpq.so.5" not in needed(static_probe)
        static_text = run([str(static_probe), "plain"], dest / "static-consumer-plain.log", env=env, timeout=20)
        assert json.loads(next(line for line in static_text.splitlines() if line.startswith("{")))["ambient_ldap"]
        for mode in ("plain", "ldap-first", "gnutls-first", "parallel-first"):
            maps = dest / (mode + "-maps")
            maps.mkdir()
            probe_env = dict(env, KU_FFI_MAPS_DIR=str(maps))
            service = work / "pg_service.conf"
            service.write_text("[ku-loader]\nldap://127.0.0.1:1/dc=ku?description?base?(objectClass=*)\n")
            probe_env["PGSERVICEFILE"] = str(service)
            text = run([str(probe), mode], dest / (mode + ".log"), env=probe_env, timeout=20)
            first = json.loads(next(line for line in text.splitlines() if line.startswith("{")))
            assert first["ambient_ldap"] == (dynamic == "0")
        if dynamic == "1":
            shim = work / "loader-fault.so"
            run(["clang", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                 str(fixture / "loader-fault.c"), "-ldl", "-o", str(shim)], dest / "fault-build.log")
            for mode in ("missing-dso", "missing-symbol"):
                fault_env = dict(env, LD_PRELOAD=str(shim), KU_LDAP_FAULT=mode,
                                 PGSERVICEFILE=str(work / "pg_service.conf"))
                text = run([str(probe), mode], dest / (mode + ".log"), env=fault_env, timeout=20)
                assert re.findall(r"^KU_LDAP_FAULT_HITS=(\d+)$", text, re.M) == ["1"]
        module_hashes = {}
        for maps in dest.glob("*-maps/*.maps"):
            for line in maps.read_text().splitlines():
                parts = line.split(None, 5)
                if len(parts) == 6 and parts[5].startswith("/"):
                    path = Path(parts[5])
                    if path.is_file() and ".so" in path.name and str(path) not in module_hashes:
                        module_hashes[str(path)] = digest(path)
        # Capture A before the common installation is replaced by B.
        record["runtime_module_sha256"] = module_hashes
        write_json(dest / "record.json", record)
        records[label] = record
    a, b = records["A"], records["B"]
    assert a["static_object_undefined"] == b["static_object_undefined"]
    assert a["pkg_config_static_tokens"] == b["pkg_config_static_tokens"]
    assert a["evaluated_Make_variables"]["pc"] == b["evaluated_Make_variables"]["pc"]
    assert a["configure_flags"] == b["configure_flags"]
    assert a["exports"] == b["exports"]
    assert [x for x in a["DT_NEEDED"] if x != "libldap.so.2"] == b["DT_NEEDED"]
    write_json(output / "runtime-module-sha256.json",
               {label: record["runtime_module_sha256"] for label, record in records.items()})
    write_json(output / "result.json", {"status": "PASS", "Make_shared_static": True,
               "static_flags_original_order": True, "real_FFI_both_orders": True,
               "parallel_first_and_partial_failure_recovery": True,
               "Meson_real_build": "NOT RUN", "LDAP_successful_search_and_TLS_handshake": "NOT RUN",
               "servers_started": 0, "SQL_queries": 0, "performance_measured": False,
               "whole_service_memory_improvement": "UNMEASURED"})
    print("BUILD_FFI_GATE_PASS; Meson and performance are not claimed", flush=True)


if __name__ == "__main__":
    main()
