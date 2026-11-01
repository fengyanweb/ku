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
PATCH_SHA = "fdf5defb9e3c26b5b6ca4740c7f6cd4d364eed811f906bb0923d9bd3ccf349b9"


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
    parser.add_argument("--reuse-previous-gate", type=Path)
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
    base_env.pop("GH_TOKEN", None)
    base_env.pop("GITHUB_TOKEN", None)
    identity.update({"configure_command_identical_A_B": configure,
                     "CFLAGS": base_env["CFLAGS"], "CPPFLAGS": base_env.get("CPPFLAGS", ""),
                     "LDFLAGS": base_env.get("LDFLAGS", ""),
                     "pkg_config_path": base_env.get("PKG_CONFIG_PATH", "")})
    write_json(output / "identities.json", identity)
    prior = None
    reuse_checks = {}
    if args.reuse_previous_gate:
        previous = args.reuse_previous_gate.resolve()
        old_identity = json.loads((previous / "identities.json").read_text())
        assert old_identity["tools_commit"] == "2fd62cda3d0c44e499bc66e1157ecd012f8d213d"
        assert old_identity["patch_sha256"] == "7af0d6cfe39c55e5a8d0d56123a74ebfeb12e677a83ffdc5d9a277daa81d927f"
        assert json.loads((previous / "result.json").read_text())["status"] == "PASS_SCOPED_SHARED_LOADER_GATE"
        for key in ("source_sha256", "compiler_sha256", "distro_packages", "CFLAGS", "CPPFLAGS", "LDFLAGS", "configure_command_identical_A_B"):
            reuse_checks[key] = identity[key] == old_identity[key]
        old_a = json.loads((previous / "A/record.json").read_text())
        for name, sha in old_a["runtime_module_sha256"].items():
            if not name.startswith(str(work)):
                reuse_checks["module:" + name] = Path(name).is_file() and digest(name) == sha
        if all(reuse_checks.values()):
            prior = previous
    write_json(output / "reuse-checks.json", reuse_checks)
    records = {}
    if prior:
        dest = output / "A"
        dest.mkdir()
        for name in ("libpq.so.5.18", "libpq.a", "fe-connect-static.o", "fe-connect-shared.o", "libpq.pc", "configure.status", "explicit-ffi-smoke"):
            shutil.copy2(prior / "A" / name, dest / name)
        (dest / "explicit-ffi-smoke").chmod(0o755)
        record = json.loads((prior / "A/record.json").read_text())
        assert digest(dest / "libpq.so.5.18") == record["shared_sha256"]
        assert digest(dest / "libpq.a") == record["static_sha256"]
        record["build_provenance"] = {"reused_from_run": 41286409116,
            "actual_source_patch_sha256": old_identity["patch_sha256"],
            "actual_tools_commit": old_identity["tools_commit"],
            "ordinary_FFI_evidence": "unchanged prior gate; interposition is tested again here"}
        records["A"] = record
    for label, dynamic in ((("B", "1"),) if prior else (("A", "0"), ("B", "1"))):
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
    if prior:
        # Reconstruct the exact old source, then compare real preprocessor output
        # with DYN disabled using the same current headers/configuration/flags.
        current_text = (source / "src/interfaces/libpq/fe-connect.c").read_text()
        old_text = current_text.replace("dlsym(RTLD_DEFAULT, symbol)", "dlsym(module, symbol)")
        old_text = old_text.replace("/* Keep the original global lookup priority for the complete paired API. */",
                                    "/* Keep declarations and every matching release operation in one library. */")
        old_sources = json.loads((prior / "source-files.json").read_text())
        assert hashlib.sha256(old_text.encode()).hexdigest() == old_sources["src/interfaces/libpq/fe-connect.c"]["after_sha256"]
        argv = shlex.split(records["A"]["fe_connect_compile_commands"][0])
        flags = []
        skip = False
        for token in argv[1:]:
            if skip:
                skip = False
                continue
            if token == "-o":
                skip = True
            elif token != "-c" and not token.endswith("/fe-connect.c"):
                flags.append(token)
        assert not any("USE_DYNAMIC_LDAP" in x for x in flags)
        preprocess = ["clang"] + flags + ["-E", "-P", "-x", "c", "-"]
        hashes = [hashlib.sha256(subprocess.check_output(preprocess, input=text.encode(),
            cwd=work / "build-B/src/interfaces/libpq", env=base_env, timeout=60)).hexdigest()
            for text in (old_text, current_text)]
        assert hashes[0] == hashes[1], "eager A preprocessor input is not equivalent"
        write_json(output / "A-preprocessor-equivalence.json", {"old_source_sha256": hashlib.sha256(old_text.encode()).hexdigest(),
            "current_source_sha256": digest(source / "src/interfaces/libpq/fe-connect.c"),
            "DYN_enabled": False, "preprocessor_sha256_old_new": hashes, "command": preprocess,
            "reused_A_binary_SHA_unchanged": records["A"]["shared_sha256"]})
    helpers = output / "upstream-helper-archives"
    helpers.mkdir()
    helper_ids = {}
    for name in ("libpgcommon.a", "libpgcommon_shlib.a", "libpgport.a", "libpgport_shlib.a"):
        path = prefix / "lib" / name
        shutil.copy2(path, helpers / name)
        helper_ids[name] = {"sha256": digest(path), "origin": "unaltered upstream source; current B build, same configure/toolchain/flags"}
        run(["ar", "t", str(path)], helpers / (name + "-members.log"))
        run(["nm", "--defined-only", str(path)], helpers / (name + "-symbols.log"))
    write_json(helpers / "identities.json", helper_ids)
    for label, record in records.items():
        dest = output / label
        static_probe = dest / "static-consumer-smoke"
        static_libs = [token for token in record["pkg_config_static_tokens"] if token != "-lpq"]
        static_command = ["clang", "-std=c11", "-O2", "-pthread", str(fixture / "explicit-ffi-smoke.c"), str(dest / "libpq.a")]
        env = dict(base_env, LD_LIBRARY_PATH=str(prefix / "lib"))
        try:
            run(static_command + static_libs + ["-ldl", "-o", str(static_probe)], dest / "static-consumer-literal-pc.log")
            record["original_pc_static_consumer"] = "PASS"
        except RuntimeError:
            failure = (dest / "static-consumer-literal-pc.log").read_text()
            unresolved = set(re.findall(r"undefined reference to [`']([^`']+)[`']", failure))
            if not unresolved or not unresolved.issubset({"pg_encoding_to_char", "pg_char_to_encoding"}):
                raise
            record["original_pc_static_consumer"] = "FAIL: literal Make pc archive-selection recipe"
        # Maintainer's actual recipe: use the already built shlib archives at
        # the same positions, not a new helper or changed generated pc flags.
        correct_libs = [str(prefix / "lib/libpgcommon_shlib.a") if x == "-lpgcommon" else
                        str(prefix / "lib/libpgport_shlib.a") if x == "-lpgport" else x for x in static_libs]
        assert "-lpgcommon" in static_libs and "-lpgport" in static_libs
        run(static_command + correct_libs + ["-ldl", "-o", str(static_probe)], dest / "static-consumer-official-shlib-recipe.log")
        assert "libpq.so.5" not in needed(static_probe)
        text = run([str(static_probe), "plain"], dest / "static-consumer-plain.log", env=env, timeout=20)
        assert json.loads(next(line for line in text.splitlines() if line.startswith("{")))["ambient_ldap"]
        record["official_shlib_static_consumer"] = "PASS"
        record.pop("supplemented_static_consumer", None)
        record.pop("static_consumer_supplement", None)
        record["static_consumer_recipe"] = "unaltered upstream libpgcommon_shlib.a and libpgport_shlib.a at original positions"
        write_json(dest / "record.json", record)
    interposer = work / "paired-interposer.so"
    run(["clang", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
         str(fixture / "paired-interposer.c"), "-ldl", "-o", str(interposer)], output / "interposer-build.log")
    assert "libldap.so.2" not in needed(interposer)
    interposition = {}
    variants = [("A", output / "A"), ("B", output / "B")]
    if prior:
        handle = output / "H-prior-handle"
        handle.mkdir()
        for name in ("libpq.so.5.18", "explicit-ffi-smoke"):
            shutil.copy2(prior / "B" / name, handle / name)
        (handle / "explicit-ffi-smoke").chmod(0o755)
        assert digest(handle / "libpq.so.5.18") == json.loads((prior / "B/record.json").read_text())["shared_sha256"]
        variants.append(("H-prior-handle", handle))
    for label, dest in variants:
        (dest / "libpq.so.5").symlink_to("libpq.so.5.18")
        env = dict(base_env, LD_LIBRARY_PATH=str(dest), LD_PRELOAD=str(interposer),
                   PGSERVICEFILE=str(work / "pg_service.conf"))
        text = run([str(dest / "explicit-ffi-smoke"), "parallel-first"],
                   dest / "paired-interposer.log", env=env, timeout=20)
        lines = re.findall(r"^KU_LDAP_INTERPOSE=(\{.*\})$", text, re.M)
        assert len(lines) == 1
        interposition[label] = json.loads(lines[0])
    assert interposition["A"]["ldap_init"] == interposition["B"]["ldap_init"] == 8
    assert interposition["A"]["ldap_unbind"] == interposition["B"]["ldap_unbind"] == 8
    if "H-prior-handle" in interposition:
        assert interposition["H-prior-handle"]["ldap_init"] == 0
    write_json(output / "paired-interposition.json", {"counts": interposition,
        "all12_functions_forward_to_real_library": True,
        "finite_exercised_contract": "8 real init/bind-failure/unbind clients; no successful search/value allocations claimed",
        "prior_handle_expected_negative": "ldap_init wrapper not selected" if prior else "not run"})
    a, b = records["A"], records["B"]
    assert a["static_object_undefined"] == b["static_object_undefined"]
    assert a["original_pc_static_consumer"] == b["original_pc_static_consumer"]
    assert a["official_shlib_static_consumer"] == b["official_shlib_static_consumer"]
    assert a["pkg_config_static_tokens"] == b["pkg_config_static_tokens"]
    assert a["evaluated_Make_variables"]["pc"] == b["evaluated_Make_variables"]["pc"]
    assert a["configure_flags"] == b["configure_flags"]
    assert a["exports"] == b["exports"]
    assert [x for x in a["DT_NEEDED"] if x != "libldap.so.2"] == b["DT_NEEDED"]
    write_json(output / "runtime-module-sha256.json",
               {label: record["runtime_module_sha256"] for label, record in records.items()})
    write_json(output / "result.json", {"status": "PASS_SCOPED_SHARED_LOADER_GATE", "Make_libraries_built": True,
               "original_pc_static_consumer": {k: v["original_pc_static_consumer"] for k, v in records.items()},
               "official_shlib_static_consumer": {k: v["official_shlib_static_consumer"] for k, v in records.items()},
               "static_flags_original_order": True, "real_FFI_both_orders": True,
               "parallel_first_and_partial_failure_recovery": True,
               "paired_global_interposition": True, "eager_A_reused": bool(prior),
               "Meson_real_build": "NOT RUN", "LDAP_successful_search_and_TLS_handshake": "NOT RUN",
               "servers_started": 0, "SQL_queries": 0, "performance_measured": False,
               "whole_service_memory_improvement": "UNMEASURED"})
    print("BUILD_FFI_GATE_PASS; Meson and performance are not claimed", flush=True)


if __name__ == "__main__":
    main()
