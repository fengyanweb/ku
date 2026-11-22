"""Ku-owned static Python discovery, wheel selection and verified extraction."""
import ast
import email.parser
import hashlib
import json
import os
import platform
import re
import stat
import sys
import sysconfig
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

MAX_SOURCE = 4 * 1024 * 1024
MAX_INDEX = 12 * 1024 * 1024
MAX_WHEEL = 128 * 1024 * 1024
MAX_EXPANDED = 256 * 1024 * 1024
MAX_FILES = 25_000
MAX_DISTRIBUTIONS = 64
MAX_CHOICES = 256
SUPPORTED = {"str": "str", "int": "int", "bool": "bool", "None": "null"}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def request(url, host):
    parts = urllib.parse.urlsplit(url)
    if (parts.scheme != "https" or parts.netloc != host or parts.query
            or parts.fragment or not parts.path.startswith("/")):
        raise ValueError(f"Unexpected Python artifact URL for {host}")
    return OPENER.open(urllib.request.Request(url, headers={"User-Agent": "Ku Python installer"}), timeout=20)


def target():
    if sys.implementation.name != "cpython" or not (3, 9) <= sys.version_info[:2] < (3, 15):
        raise ValueError("Ku Python requires a CPython 3.9 through 3.14 installation")
    if sysconfig.get_config_var("Py_GIL_DISABLED"):
        raise ValueError("The free-threaded CPython ABI is not supported")
    return {
        "version": ".".join(map(str, sys.version_info[:2])),
        "platform": sysconfig.get_platform(),
        "soabi": sysconfig.get_config_var("SOABI") or sys.implementation.cache_tag,
        "machine": platform.machine().lower(),
    }


def probe():
    """Find the actual CPython library providing the bootstrap interpreter."""
    import ctypes
    from ctypes import c_void_p

    if os.name == "nt":
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        filename = kernel.GetModuleFileNameW
        filename.argtypes = [c_void_p, ctypes.c_wchar_p, ctypes.c_uint32]
        filename.restype = ctypes.c_uint32
        file = ctypes.create_unicode_buffer(32768)
        n = filename(ctypes.pythonapi._handle, file, len(file))
        if not 0 < n < len(file):
            raise ValueError("Cannot identify the running CPython DLL")
        library = Path(file.value)
    else:
        class DlInfo(ctypes.Structure):
            _fields_ = [("file", ctypes.c_char_p), ("base", c_void_p),
                        ("name", ctypes.c_char_p), ("address", c_void_p)]
        dladdr = ctypes.CDLL(None).dladdr
        dladdr.argtypes = [c_void_p, ctypes.POINTER(DlInfo)]
        dladdr.restype = ctypes.c_int
        info = DlInfo()
        if not dladdr(ctypes.cast(ctypes.pythonapi.Py_GetVersion, c_void_p), ctypes.byref(info)):
            raise ValueError("Cannot identify the running shared CPython library")
        library = Path(os.fsdecode(info.file))
        if library.resolve() == Path(sys.executable).resolve():
            # Some relocatable distributions run a statically linked launcher
            # and distribute the corresponding shared embedding ABI separately.
            filename = sysconfig.get_config_var("LDLIBRARY") or ""
            home = Path(sys.base_prefix).resolve()
            if (not sysconfig.get_config_var("Py_ENABLE_SHARED") or not filename
                    or Path(filename).name != filename):
                raise ValueError("Selected CPython does not include a shared embedding library")
            library = (home / "lib" / filename).resolve()
            library.relative_to(home)
            if not library.is_file():
                raise ValueError(f"Shared CPython runtime {filename} is missing from its base prefix")
            shared = ctypes.CDLL(str(library))
            shared.Py_GetVersion.restype = ctypes.c_char_p
            if shared.Py_GetVersion().decode("ascii").split(" ", 1)[0] != platform.python_version():
                raise ValueError("Shared CPython runtime release differs from the selected executable")

    include = Path(sysconfig.get_path("include"))
    headers = Path(sysconfig.get_config_var("INCLUDEPY") or include)
    if not (headers / "Python.h").is_file():
        if not (include / "Python.h").is_file():
            raise ValueError("Selected CPython has no Python.h for its embedding ABI")
        headers = include
    if not library.is_file():
        raise ValueError("Selected CPython library does not exist")
    release = ".".join(map(str, sys.version_info[:3]))
    return {
        "target": target(), "release": release, "home": str(Path(sys.base_prefix).resolve()),
        "include": str(headers.resolve()), "library": str(library.resolve()),
        "executable": str(Path(sys.executable).resolve()),
    }


def annotation(node):
    if isinstance(node, ast.Constant):
        value = "None" if node.value is None else node.value
        return SUPPORTED.get(value) if isinstance(value, str) else None
    return SUPPORTED.get(node.id) if isinstance(node, ast.Name) else None


def discover_text(content, label):
    if len(content) > MAX_SOURCE:
        raise ValueError(f"Python source exceeds {MAX_SOURCE} bytes")
    functions = []
    declarations = ast.parse(content, filename=label).body
    counts = {}
    for declaration in declarations:
        if not isinstance(declaration, ast.FunctionDef) or declaration.name.startswith("_"):
            continue
        counts[declaration.name] = counts.get(declaration.name, 0) + 1
        args = declaration.args
        returns = annotation(declaration.returns)
        if declaration.decorator_list or args.kwonlyargs or args.kwarg:
            continue
        parameters = [[arg.arg, annotation(arg.annotation)] for arg in args.posonlyargs + args.args]
        dynamic = not returns or args.vararg or any(not ty or ty == "null" for _, ty in parameters)
        if dynamic:
            parameters, returns = [["json_args", "str"]], "str"
        elif len(parameters) > 32:
            continue
        functions.append({"name": declaration.name, "callable": declaration.name,
                          "parameters": parameters, "returns": returns, "dynamic": bool(dynamic)})
    return [f for f in functions if counts[f["name"]] == 1]


def read_bounded(path, bound):
    with path.open("rb") as stream:
        data = stream.read(bound + 1)
    if len(data) > bound:
        raise ValueError(f"{path.name} exceeds the Python installation size limit")
    return data


def json_at(url, host):
    with request(url, host) as stream:
        data = stream.read(MAX_INDEX + 1)
    if len(data) > MAX_INDEX:
        raise ValueError("PyPI response exceeded the index size limit")
    return json.loads(data)


def package_api():
    # pip's standards library is part of an explicitly selected interpreter's
    # installer environment. Ku never loads a target wheel into this process.
    prefix = Path(sys.base_prefix)
    minor = f"python{sys.version_info.major}.{sys.version_info.minor}"
    candidates = [prefix / "lib" / minor / "site-packages", prefix / "Lib" / "site-packages"]
    for key in ("purelib", "platlib"):
        candidates.append(Path(sysconfig.get_path(key)))
    for candidate in candidates:
        if (candidate / "pip" / "_vendor" / "packaging" / "__init__.py").is_file():
            sys.path.append(str(candidate))
            break
    else:
        raise ValueError("Selected CPython needs its trusted pip installer for PyPI wheel tags")
    from pip._vendor.packaging import tags, utils
    from pip._vendor.packaging.requirements import Requirement
    from pip._vendor.packaging.version import Version
    return {tag: i for i, tag in enumerate(tags.sys_tags())}, utils, Requirement, Version


def valid_wheel(row):
    filename = row["filename"]
    if (not isinstance(filename, str) or not isinstance(row["sha256"], str)
            or not re.fullmatch(r"[A-Za-z0-9_.+!-]{1,240}\.whl", filename)
            or not re.fullmatch(r"[0-9a-f]{64}", row["sha256"])
            or not isinstance(row["size"], int) or not 1 <= row["size"] <= MAX_WHEEL):
        raise ValueError("Invalid locked Python wheel identity")
    parts = urllib.parse.urlsplit(row["url"])
    if (parts.scheme != "https" or parts.netloc != "files.pythonhosted.org"
            or parts.query or parts.fragment or not parts.path.startswith("/packages/")
            or urllib.parse.unquote(parts.path.rsplit("/", 1)[-1]) != filename):
        raise ValueError("Locked Python wheel must identify its immutable PyPI file")


def verify_wheel(path, row):
    if not path.is_file() or path.is_symlink() or path.stat().st_size != row["size"]:
        return False
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest() == row["sha256"]


def wheel_path(cache, row, offline):
    valid_wheel(row)
    path = cache / row["sha256"] / row["filename"]
    if path.parent.is_symlink():
        raise ValueError("Python wheel cache contains an unexpected symlink")
    if verify_wheel(path, row):
        return path
    if offline:
        raise ValueError(f"Offline Python wheel cache misses or cannot verify {row['filename']}")
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temp = tempfile.mkstemp(prefix=".wheel-", dir=path.parent)
    try:
        size = 0
        digest = hashlib.sha256()
        with os.fdopen(fd, "wb") as out, request(row["url"], "files.pythonhosted.org") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                size += len(chunk)
                if size > row["size"]:
                    raise ValueError(f"Wheel {row['filename']} exceeds its locked size")
                digest.update(chunk)
                out.write(chunk)
        if size != row["size"] or digest.hexdigest() != row["sha256"]:
            raise ValueError(f"Wheel {row['filename']} failed SHA-256 or size verification")
        os.replace(temp, path)
    finally:
        if os.path.exists(temp):
            os.unlink(temp)
    return path


def wheel_metadata(archive):
    names = [name for name in archive.namelist()
             if name.count("/") == 1 and name.endswith(".dist-info/METADATA")]
    if len(names) != 1:
        raise ValueError("Python wheel must include exactly one dist-info/METADATA")
    with archive.open(names[0]) as stream:
        payload = stream.read(MAX_SOURCE + 1)
    if len(payload) > MAX_SOURCE:
        raise ValueError("Python wheel metadata exceeds size limit")
    return email.parser.BytesParser().parsebytes(payload)


def select(requirement_text, cache, module, fixed):
    tags, utils, Requirement, Version = package_api()
    from pip._vendor.packaging.specifiers import SpecifierSet
    root = Requirement(requirement_text)
    if root.url or root.marker or root.extras:
        raise ValueError("Use pypi:distribution or pypi:distribution==version")
    canonical = utils.canonicalize_name
    root_name = canonical(root.name)
    indices = {}
    loaded = {}
    choices = 0
    full_python = ".".join(map(str, sys.version_info[:3]))

    def releases(name, requirements, exact=None):
        if name not in indices:
            indices[name] = json_at(f"https://pypi.org/pypi/{name}/json", "pypi.org")["releases"]
        ranked = []
        for version_text, files in indices[name].items():
            version = Version(version_text)
            selected = fixed.get(name)
            if selected is not None and version != Version(selected):
                continue
            if (exact is not None and version != exact) or not all(
                requirement.specifier.contains(version) for requirement in requirements
            ):
                continue
            compatible = []
            for item in files:
                if item.get("yanked") or item.get("packagetype") != "bdist_wheel":
                    continue
                if item.get("requires_python") and full_python not in SpecifierSet(item["requires_python"]):
                    continue
                wheel_name, wheel_version, _, wheel_tags = utils.parse_wheel_filename(item["filename"])
                matching = [tags[tag] for tag in wheel_tags if tag in tags]
                if matching and canonical(wheel_name) == name and wheel_version == version:
                    path = urllib.parse.urlsplit(item["url"])
                    compatible.append((min(matching), path.path, item))
            for preference, path, item in compatible:
                ranked.append((version, -preference, path, item))
        for _, _, _, item in sorted(ranked, key=lambda item: item[:3], reverse=True):
            yield item

    def metadata(name, item):
        by_hash = item["digests"]["sha256"]
        if by_hash in loaded:
            return loaded[by_hash]
        row = {"filename": item["filename"], "url": item["url"],
               "sha256": by_hash, "size": item["size"]}
        path = wheel_path(cache, row, False)
        with zipfile.ZipFile(path) as archive:
            meta = wheel_metadata(archive)
        installed_name, version = canonical(meta["Name"]), Version(meta["Version"])
        if installed_name != name:
            raise ValueError(f"Wheel {row['filename']} declares the wrong distribution")
        row["name"] = name
        row["version"] = str(version)
        row["requires"] = meta.get_all("Requires-Dist", [])
        loaded[by_hash] = row
        return row

    def constraints(selected):
        values = {root_name: [root]}
        while True:
            added = False
            for name, row in selected.items():
                if name not in values:
                    continue
                extras = set().union(*(requirement.extras for requirement in values[name]))
                for raw in row["requires"]:
                    req = Requirement(raw)
                    if req.marker and not any(req.marker.evaluate({"extra": extra}) for extra in extras | {""}):
                        continue
                    if req.url:
                        raise ValueError(f"Dependency of {name} uses a direct URL outside PyPI")
                    bucket = values.setdefault(canonical(req.name), [])
                    if req not in bucket:
                        bucket.append(req)
                        added = True
                    if len(values) > MAX_DISTRIBUTIONS:
                        raise ValueError("Python dependency graph exceeds the distribution limit")
            if not added:
                return values

    def solve(selected):
        nonlocal choices
        required = constraints(selected)
        for name, row in selected.items():
            if not all(req.specifier.contains(row["version"]) for req in required[name]):
                return None
        pending = next((name for name in sorted(required) if name not in selected), None)
        if pending is None:
            return selected
        for item in releases(pending, required[pending]):
            choices += 1
            if choices > MAX_CHOICES:
                raise ValueError("Python resolution budget exhausted; pin a compatible version")
            candidate = metadata(pending, item)
            if not all(req.specifier.contains(candidate["version"]) for req in required[pending]):
                continue
            found = solve({**selected, pending: candidate})
            if found is not None:
                return found
        return None

    solution = solve({})
    if solution is None:
        raise ValueError(f"No supported wheel set for {root} on {sysconfig.get_platform()} "
                         f"(CPython {target()['version']}); source builds are disabled")
    exports = []
    if module:
        base = module.replace(".", "/")
        files = (f"{base}.pyi", f"{base}.py", f"{base}/__init__.pyi", f"{base}/__init__.py")
        with zipfile.ZipFile(wheel_path(cache, solution[root_name], True)) as archive:
            found = next((file for file in files if file in archive.namelist()), None)
            if found:
                with archive.open(found) as stream:
                    exports = discover_text(stream.read(MAX_SOURCE + 1), found)
            elif not any(name.startswith(base + ".") or name.startswith(base + "/")
                         for name in archive.namelist()):
                raise ValueError(f"The selected wheel does not provide Python module {module!r}")
    return {"distribution": root_name, "version": solution[root_name]["version"],
            "target": target(), "artifacts": [solution[name] for name in sorted(solution)],
            "exports": exports}


def unpack(descriptor, cache, output, offline):
    artifacts = descriptor.get("artifacts", [])
    if not isinstance(artifacts, list):
        raise ValueError("Invalid pinned Python dependencies")
    if artifacts and target() != descriptor.get("target"):
        for selection in descriptor.get("additional_targets", []):
            if selection["target"] == target():
                artifacts = selection["artifacts"]
                break
        else:
            raise ValueError(f"Python wheel lock has no assembly for {target()}; "
                             "rerun ku i for the same PyPI package using this interpreter")
    if not isinstance(artifacts, list) or len(artifacts) > MAX_DISTRIBUTIONS:
        raise ValueError("Too many pinned Python dependencies")
    if artifacts:
        wheel_tags, utils, _, Version = package_api()
    total = 0
    count = 0
    seen = set()
    for row in artifacts:
        path = wheel_path(cache, row, offline)
        name, version, _, encoded = utils.parse_wheel_filename(path.name)
        if (utils.canonicalize_name(name) != row["name"] or str(version) != row["version"]
                or not encoded.intersection(wheel_tags)):
            raise ValueError(f"Wheel identity or CPython tag mismatch for {path.name}")
        with zipfile.ZipFile(path) as archive:
            meta = wheel_metadata(archive)
            if (utils.canonicalize_name(meta["Name"]) != row["name"]
                    or str(Version(meta["Version"])) != row["version"]
                    or meta.get_all("Requires-Dist", []) != row["requires"]):
                raise ValueError(f"Locked metadata differs for {path.name}")
            for member in archive.infolist():
                count += 1
                total += member.file_size
                if count > MAX_FILES or total > MAX_EXPANDED:
                    raise ValueError("Expanded Python wheel environment exceeds safety limits")
                raw = member.filename.rstrip("/")
                parts = raw.split("/")
                mode = member.external_attr >> 16
                if (not raw or len(raw) > 600 or any(
                    not re.fullmatch(r"[A-Za-z0-9_.+()@ -]{1,200}", piece)
                    or piece in (".", "..") or piece.endswith((".", " "))
                    or re.fullmatch(r"(?i:con|prn|aux|nul|com[1-9]|lpt[1-9])", piece.split(".")[0])
                    for piece in parts
                ) or stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR)):
                    raise ValueError(f"Unsafe Python wheel path: {member.orig_filename!r}")
                # Wheels can relocate purelib/platlib data. Other wheel schemes
                # are not importable and require an author-provided deployment.
                if parts[0].endswith(".data"):
                    if len(parts) >= 3 and parts[1] in ("purelib", "platlib"):
                        parts = parts[2:]
                    elif member.is_dir():
                        continue
                    else:
                        raise ValueError(f"Unsupported Python wheel install scheme: {raw}")
                identity = str(PurePosixPath(*parts)).casefold()
                destination = output.joinpath(*parts)
                if member.is_dir():
                    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
                    continue
                if identity in seen:
                    raise ValueError(f"Python wheel contents conflict at {identity}")
                seen.add(identity)
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                with archive.open(member) as source, destination.open("xb") as out:
                    copied = 0
                    for chunk in iter(lambda: source.read(1024 * 1024), b""):
                        copied += len(chunk)
                        if copied > member.file_size:
                            raise ValueError(f"Invalid Python wheel member length: {raw}")
                        out.write(chunk)
                    if copied != member.file_size:
                        raise ValueError(f"Invalid Python wheel member length: {raw}")


def main():
    command, *args = sys.argv[1:]
    if command == "probe":
        result = probe()
    elif command == "target":
        result = target()
    elif command == "discover":
        path = Path(args[0])
        result = {"exports": discover_text(read_bounded(path, MAX_SOURCE), path.name)}
    elif command == "select":
        result = select(args[0], Path(args[1]), args[2] or None, json.loads(args[3]) if len(args) > 3 else {})
    elif command == "unpack":
        descriptor = json.loads(read_bounded(Path(args[0]), MAX_INDEX))
        unpack(descriptor, Path(args[1]), Path(args[2]), args[3] == "offline")
        result = {"ok": True}
    else:
        raise ValueError("Unknown Ku Python preparation request")
    return result


if __name__ == "__main__":
    try:
        print(json.dumps(main(), ensure_ascii=True, sort_keys=True, separators=(",", ":")))
    except (OSError, ValueError, KeyError, RuntimeError, AttributeError, ImportError, SyntaxError,
            zipfile.BadZipFile) as error:
        print(json.dumps({"error": str(error)}, ensure_ascii=True))
        sys.exit(1)
