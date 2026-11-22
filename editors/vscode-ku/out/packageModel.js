"use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
Object.defineProperty(exports, "__esModule", { value: true });
exports.MAX_INVENTORY_ENTRIES = exports.MAX_MODULE_BYTES = void 0;
exports.readPackageText = readPackageText;
exports.discoverProject = discoverProject;
exports.listProjectDependencies = listProjectDependencies;
exports.safePackagePath = safePackagePath;
exports.resolvePackageImport = resolvePackageImport;
const crypto = __importStar(require("crypto"));
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const MAX_MANIFEST = 256000;
const MAX_LOCK = 20000000;
exports.MAX_MODULE_BYTES = 1000000;
exports.MAX_INVENTORY_ENTRIES = 512;
const INSTALL_NEXT_STEP = "在当前项目目录手动运行 ku i；插件不会自动安装或联网。";
const PACKAGE_NAME = /^[a-z][a-z0-9_-]{0,63}$/;
const VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
function issue(error) {
    return { reason: error instanceof Error ? error.message : String(error), nextStep: INSTALL_NEXT_STEP };
}
/** Bounded, non-writing reads. Never follow a final symlink or special file. */
function readPackageText(file, limit = exports.MAX_MODULE_BYTES) {
    const before = fs.lstatSync(file);
    if (!before.isFile() || before.isSymbolicLink() || before.size > limit)
        throw new Error(`无法读取普通文件或文件超过 ${limit} 字节：${file}`);
    const fd = fs.openSync(file, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW ?? 0));
    try {
        const opened = fs.fstatSync(fd);
        if (!opened.isFile() || opened.size > limit || opened.ino !== before.ino || opened.dev !== before.dev)
            throw new Error(`文件在读取前发生变化：${file}`);
        const data = Buffer.alloc(opened.size + 1);
        let length = 0;
        while (length < data.length) {
            const count = fs.readSync(fd, data, length, data.length - length, null);
            if (count === 0)
                break;
            length += count;
        }
        const after = fs.fstatSync(fd);
        if (length !== opened.size || after.size !== opened.size || after.mtimeMs !== opened.mtimeMs || after.ctimeMs !== opened.ctimeMs)
            throw new Error(`文件在读取时发生变化：${file}`);
        const text = data.subarray(0, length).toString("utf8");
        if (!Buffer.from(text, "utf8").equals(data.subarray(0, length)))
            throw new Error(`文件不是有效 UTF-8：${file}`);
        return text;
    }
    finally {
        fs.closeSync(fd);
    }
}
function withoutComment(line) {
    let quoted = false;
    for (let i = 0; i < line.length; i++) {
        if (line[i] === '"')
            quoted = !quoted;
        else if (line[i] === "#" && !quoted)
            return line.slice(0, i).trim();
    }
    return line.trim();
}
// ku.mod is a small line protocol, not TOML. Its parser strips quotes without
// interpreting JSON escapes; preserve that behavior rather than using JSON.parse.
function field(line) {
    const split = line.indexOf("=");
    const value = line.slice(split + 1).trim();
    if (split < 1 || value.length < 2 || value[0] !== '"' || !value.endsWith('"'))
        throw new Error("ku.mod/ku.lock 存在无效的 key = \"value\" 行。");
    return [line.slice(0, split).trim(), value.slice(1, -1)];
}
function put(fields, key, value) {
    if (fields.has(key))
        throw new Error(`ku.mod/ku.lock 存在重复字段：${key}`);
    fields.set(key, value);
}
function portableRelative(value) {
    return !!value && !path.isAbsolute(value) && value.split("/").every((part) => !!part && part !== "." && part !== ".."
        && !/[\\:<>"|?*\x00-\x1f\x7f]/.test(part) && !/[ .]$/.test(part) && !/^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(part));
}
function versionParts(value) {
    return VERSION.test(value) ? value.split(".").map(BigInt) : undefined;
}
function validSource(source) {
    if (Buffer.byteLength(source, "utf8") > 2048)
        return false;
    if (source.startsWith("path:"))
        return portableRelative(source.slice(5)) && source.slice(5).split("/").length <= 32;
    if (source.startsWith("file://")) {
        try {
            const value = decodeURIComponent(source.slice(7));
            return !value.startsWith("//") && !/[\\\x00-\x1f\x7f]/.test(value) && (path.isAbsolute(value) || /^\/?[A-Za-z]:\//.test(value));
        }
        catch {
            return false;
        }
    }
    if (source.startsWith("git+https://")) {
        try {
            const url = new URL(source.slice(4));
            const revision = /^#[a-fA-F0-9]{40}(?::(.+))?$/.exec(url.hash);
            return !!revision && !url.username && !url.password && !url.search && (!revision[1] || portableRelative(revision[1]));
        }
        catch {
            return false;
        }
    }
    return false;
}
function matches(requirement, version) {
    const minimum = versionParts(requirement.replace(/^\^/, ""));
    const selected = versionParts(version);
    if (!minimum || !selected || [...minimum, ...selected].some((part) => part > 18446744073709551615n))
        return false;
    if (!requirement.startsWith("^"))
        return requirement === version;
    const comparison = selected[0] - minimum[0] || selected[1] - minimum[1] || selected[2] - minimum[2];
    return comparison >= 0n && (minimum[0] > 0n ? selected[0] === minimum[0] : minimum[1] > 0n
        ? selected[0] === 0n && selected[1] === minimum[1] : selected.every((part, i) => part === minimum[i]));
}
function manifest(source) {
    const fields = new Map();
    const drafts = new Map();
    for (const raw of source.split(/\r?\n/)) {
        const line = withoutComment(raw);
        if (!line)
            continue;
        const [key, value] = field(line);
        put(fields, key, value);
        const dep = /^(dep|dev)\.([a-z][a-z0-9_-]{0,63})(?:\.(source|checksum))?$/.exec(key);
        if (dep) {
            const name = dep[2];
            let draft = drafts.get(name);
            if (draft && draft.development !== (dep[1] === "dev"))
                throw new Error(`依赖 ${name} 同时出现在 dep 和 dev。`);
            if (!draft) {
                draft = { name, development: dep[1] === "dev" };
                drafts.set(name, draft);
            }
            if (dep[3] === "source")
                draft.source = value;
            else if (dep[3] === "checksum")
                draft.checksum = value;
            else
                draft.requirement = value;
        }
        else if (!["name", "version", "root", "main", "out", "cache", "template", "type", "interop.version", "registry.url", "registry.public_key"].includes(key))
            throw new Error(`不支持的 ku.mod 字段：${key}`);
    }
    if (!PACKAGE_NAME.test(fields.get("name") ?? "") || !portableRelative(fields.get("name") ?? ""))
        throw new Error("ku.mod 缺少有效 name。");
    if (fields.has("version") && !matches(fields.get("version"), fields.get("version")))
        throw new Error("ku.mod version 无效。");
    for (const key of ["root", "main", "out", "cache"])
        if (fields.has(key) && !portableRelative(fields.get(key)))
            throw new Error(`ku.mod ${key} 不是安全的相对路径。`);
    if (fields.has("registry.url") !== fields.has("registry.public_key"))
        throw new Error("ku.mod registry 配置不完整。");
    if (drafts.size > 256)
        throw new Error("ku.mod 依赖数量超过 256。");
    const dependencies = [...drafts.values()].map((dep) => {
        if (!dep.requirement || !matches(dep.requirement, dep.requirement.replace(/^\^/, "")))
            throw new Error(`依赖 ${dep.name} 缺少有效版本要求。`);
        if (dep.source && !validSource(dep.source))
            throw new Error(`依赖 ${dep.name} 的来源不是有效的 path:、file:// 或已固定 Git 提交。`);
        if (dep.checksum && (!dep.source || !(dep.source.startsWith("file://") ? /^ku-fnv64-[a-fA-F0-9]{16}$/ : /^sha256-[a-fA-F0-9]{64}$/).test(dep.checksum)))
            throw new Error(`依赖 ${dep.name} 的 checksum 与来源不一致。`);
        return { ...dep, requirement: dep.requirement };
    }).sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
    return { fields, dependencies };
}
function discoverProject(filePath) {
    try {
        let directory = fs.realpathSync(path.dirname(path.resolve(filePath)));
        for (let count = 0; count < 256; count++) {
            const manifestPath = path.join(directory, "ku.mod");
            if (fs.existsSync(manifestPath)) {
                if (fs.existsSync(path.join(directory, "ku.lock.install-transaction")))
                    throw new Error("当前项目有未完成的依赖安装事务，请先由 Ku CLI 恢复。");
                const parsed = manifest(readPackageText(manifestPath, MAX_MANIFEST));
                return { project: { directory, manifestPath, fields: parsed.fields, dependencies: parsed.dependencies,
                        importRoot: path.join(directory, parsed.fields.get("root") ?? "src"), cacheDir: path.join(directory, parsed.fields.get("cache") ?? ".ku/cache") } };
            }
            const parent = path.dirname(directory);
            if (parent === directory)
                break;
            directory = parent;
        }
        return { reason: "当前文件没有上级 ku.mod，无法选择项目依赖。", nextStep: "打开包含 ku.mod 的项目，或先创建 Ku 项目。" };
    }
    catch (error) {
        return issue(error);
    }
}
function manifestFingerprint(project) {
    const hash = crypto.createHash("sha256").update("ku-package-install-manifest-v1\0");
    const add = (value) => { const bytes = Buffer.from(value, "utf8"); const size = Buffer.alloc(8); size.writeBigUInt64LE(BigInt(bytes.length)); hash.update(size).update(bytes); };
    const f = project.fields;
    for (const value of [f.get("name"), f.get("interop.version") ?? "", f.get("version") ?? "0.0.0", f.get("root") ?? "src", f.get("main") ?? "main.ku", f.get("out") ?? ".ku/build", f.get("cache") ?? ".ku/cache", f.get("registry.url") ?? "", f.get("registry.public_key") ?? ""])
        add(value);
    for (const development of [false, true]) {
        const group = project.dependencies.filter((dep) => dep.development === development);
        add(development ? "dev" : "dep");
        add(String(group.length));
        for (const dep of group)
            for (const value of [dep.name, dep.requirement, dep.source ?? "", dep.checksum ?? ""])
                add(value);
    }
    return `sha256-${hash.digest("hex")}`;
}
function readLock(project) {
    const source = readPackageText(path.join(project.directory, "ku.lock"), MAX_LOCK);
    const header = new Map();
    const records = [];
    let current = header;
    for (const raw of source.split(/\r?\n/)) {
        if (Buffer.byteLength(raw, "utf8") > 8192)
            throw new Error("ku.lock 存在超长行。");
        const line = withoutComment(raw);
        if (!line)
            continue;
        if (line.startsWith("[[") && line.endsWith("]]")) {
            current = new Map();
            if (line === "[[package_dependency]]")
                records.push(current);
            if (records.length > 256)
                throw new Error("ku.lock 包依赖超过 256。");
        }
        else {
            const [key, value] = field(line);
            put(current, key, value);
        }
    }
    const fingerprint = header.get("manifest_sha256");
    if (fingerprint ? fingerprint !== manifestFingerprint(project)
        : project.dependencies.some((dep) => dep.development || (dep.source && !dep.source.startsWith("file://"))))
        throw new Error("当前 ku.mod 与 ku.lock 指纹不一致或锁文件缺少安装指纹。");
    if (header.get("package") !== project.fields.get("name"))
        throw new Error("ku.lock 不属于当前项目。");
    for (const [key, fallback] of [["version", "0.0.0"], ["root", "src"], ["main", "main.ku"], ["out", ".ku/build"], ["cache", ".ku/cache"]]) {
        if (header.get(key) !== (project.fields.get(key) ?? fallback))
            throw new Error(`ku.lock ${key} 与当前项目不一致。`);
    }
    if (header.has("scope") && header.get("scope") !== "production")
        throw new Error("ku.lock 安装范围无效。");
    const entries = new Map();
    for (const record of records) {
        const name = record.get("name") ?? "";
        const version = record.get("version") ?? "";
        const source = record.get("source") ?? "";
        const checksum = record.get("checksum") ?? "";
        const requirement = record.get("requirement") ?? version;
        if (!PACKAGE_NAME.test(name) || !matches(requirement, version) || entries.has(name))
            throw new Error("ku.lock 包身份、版本或重复依赖无效。");
        const strong = /^sha256-([a-fA-F0-9]{64})$/.exec(checksum);
        const legacy = /^ku-fnv64-([a-fA-F0-9]{16})$/.exec(checksum);
        const expected = source === "registry" && strong ? `${name}-${version}-sha256-${strong[1]}`
            : strong ? `${name}-${version}-tree-sha256-${strong[1]}` : source.startsWith("file://") && legacy ? `${name}-${version}-fnv64-${legacy[1]}` : undefined;
        if (!expected || (record.has("cache_key") && record.get("cache_key") !== expected))
            throw new Error(`依赖 ${name} 的锁定 checksum/cache_key 无效。`);
        entries.set(name, { name, version, source, checksum, requirement, cacheKey: expected, registry: record.get("registry"), url: record.get("url") });
    }
    return { entries, production: header.get("scope") === "production" };
}
/** Resolve exact cache paths only; never enumerate another version or project. */
function dependencyStatus(project, declared, lock) {
    const result = { name: declared.name, requirement: declared.requirement, development: declared.development };
    try {
        if (lock.production && declared.development)
            throw new Error(`依赖 ${declared.name} 未包含在 production 锁中。`);
        const locked = lock.entries.get(declared.name);
        if (!locked || locked.requirement !== declared.requirement || !matches(declared.requirement, locked.version)
            || (declared.source ? declared.source !== locked.source : locked.source !== "registry")
            || (declared.checksum && declared.checksum !== locked.checksum)
            || (locked.source === "registry" && locked.registry !== project.fields.get("registry.url")))
            throw new Error(`依赖 ${declared.name} 缺少与当前声明一致的锁定记录。`);
        result.locked = locked;
        result.version = locked.version;
        const slot = path.join(project.cacheDir, "packages", locked.name, locked.cacheKey);
        const packageRoot = locked.source === "registry" ? path.join(slot, "package") : slot;
        safePackagePath(project.directory, packageRoot);
        if (locked.source === "registry" && readPackageText(path.join(slot, ".sha256"), 128).trim() !== locked.checksum)
            throw new Error(`依赖 ${declared.name} 的安装 checksum 回执与锁不符。`);
        const installed = manifest(readPackageText(path.join(packageRoot, "ku.mod"), MAX_MANIFEST));
        if (installed.fields.get("name") !== locked.name || installed.fields.get("version") !== locked.version || (installed.fields.get("root") ?? "src") !== "src")
            throw new Error(`依赖 ${declared.name} 安装目录身份与锁不符。`);
        const importRoot = path.join(packageRoot, "src");
        if (!fs.lstatSync(importRoot).isDirectory())
            throw new Error(`依赖 ${declared.name} 缺少 src/。`);
        safePackagePath(packageRoot, importRoot);
        result.packageRoot = packageRoot;
        result.importRoot = importRoot;
    }
    catch (error) {
        Object.assign(result, issue(error));
    }
    return result;
}
function listProjectDependencies(filePath, name) {
    const discovered = discoverProject(filePath);
    if (!discovered.project)
        return { ...discovered, dependencies: [] };
    const declarations = discovered.project.dependencies.filter((dep) => !name || dep.name === name);
    try {
        const lock = readLock(discovered.project);
        return { ...discovered, dependencies: declarations.map((dep) => dependencyStatus(discovered.project, dep, lock)) };
    }
    catch (error) {
        return { ...discovered, ...issue(error), dependencies: declarations.map((dep) => ({ name: dep.name, requirement: dep.requirement, development: dep.development, ...issue(error) })) };
    }
}
/** Reject links and traversal anywhere below the selected project's root. */
function safePackagePath(root, target) {
    const relative = path.relative(root, target);
    if (relative.startsWith(`..${path.sep}`) || relative === ".." || path.isAbsolute(relative))
        throw new Error("依赖路径超出当前项目锁定目录。");
    let current = root;
    for (const component of relative.split(path.sep).filter(Boolean)) {
        current = path.join(current, component);
        const stat = fs.lstatSync(current);
        if (stat.isSymbolicLink() || (!stat.isDirectory() && !stat.isFile()))
            throw new Error(`依赖路径含链接或特殊文件：${current}`);
    }
    return target;
}
function resolvePackageImport(filePath, importPath) {
    const match = /^@([a-z][a-z0-9_-]{0,63})\/(.+)$/.exec(importPath);
    if (!match || !portableRelative(match[2]))
        return { reason: "依赖导入必须使用安全的 @name/path，不能包含 ..、反斜杠或绝对路径。" };
    const selected = listProjectDependencies(filePath, match[1]);
    if (!selected.project)
        return selected;
    const dependency = selected.dependencies.find((dep) => dep.name === match[1]);
    if (!dependency)
        return { reason: `当前项目没有声明依赖 ${match[1]}。`, nextStep: `先在当前项目手动运行 ku i ${match[1]}。` };
    if (!dependency.importRoot)
        return { dependency, reason: dependency.reason, nextStep: dependency.nextStep };
    try {
        let relative = match[2];
        if (!path.extname(relative))
            relative += ".ku";
        if (path.extname(relative).toLowerCase() !== ".ku")
            throw new Error("依赖导入必须指向 .ku 源码或绑定声明。");
        const target = safePackagePath(dependency.importRoot, path.join(dependency.importRoot, relative));
        if (!fs.lstatSync(target).isFile())
            throw new Error(`导入目标不是 Ku 文件：${importPath}`);
        return { filePath: target, dependency, projectDir: selected.project.directory, packageRoot: dependency.packageRoot, importRoot: dependency.importRoot };
    }
    catch (error) {
        return { dependency, ...issue(error) };
    }
}
