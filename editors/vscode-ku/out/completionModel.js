"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.namespaceMembers = exports.stdFunctions = exports.stdModules = exports.builtins = exports.types = exports.keywords = void 0;
exports.stdImportPathLabels = stdImportPathLabels;
exports.memberCompletionLabels = memberCompletionLabels;
exports.globalCompletionLabels = globalCompletionLabels;
exports.keywords = [
    "async",
    "await",
    "fn",
    "struct",
    "enum",
    "module",
    "import",
    "from",
    "if",
    "else",
    "while",
    "for",
    "in",
    "break",
    "continue",
    "match",
    "try",
    "catch",
    "finally",
    "fail",
    "panic",
    "return",
    "print",
    "println",
    "true",
    "false",
    "null",
];
exports.types = ["int", "float", "bool", "str", "null"];
exports.builtins = ["len", "str", "ok", "err", "println"];
exports.stdModules = ["std.fs", "std.http", "std.string", "std.array", "std.json", "std.config", "std.time"];
exports.stdFunctions = [
    "fs.read",
    "fs.try_read",
    "fs.write",
    "fs.try_write",
    "http.get",
    "http.post",
    "http.request",
    "http.client",
    "http.text",
    "http.json",
    "http.service",
    "http.server",
    "string.len",
    "string.trim",
    "string.lower",
    "string.upper",
    "string.slice",
    "array.len",
    "array.try_get",
    "array.push",
    "array.concat",
    "json.parse",
    "json.try_parse",
    "json.stringify",
    "config.env",
    "config.env_file",
    "config.yaml",
    "time.now",
    "time.unix",
    "time.millis",
];
exports.namespaceMembers = {
    fs: ["read", "try_read", "write", "try_write"],
    http: ["get", "post", "request", "client", "text", "json", "service", "server"],
    string: ["len", "trim", "lower", "upper", "slice"],
    array: ["len", "try_get", "push", "concat"],
    json: ["parse", "try_parse", "stringify"],
    config: ["env", "env_file", "yaml"],
    time: ["now", "unix", "millis"],
    task: ["status", "cancel", "await_timeout"],
};
function stdImportPathLabels(current) {
    if (!current.startsWith("std.")) {
        return [];
    }
    return exports.stdModules.filter((module) => module.startsWith(current));
}
function memberCompletionLabels(receiver, prefix = "") {
    const members = exports.namespaceMembers[receiver] ?? [];
    return members.filter((member) => member.startsWith(prefix));
}
function globalCompletionLabels() {
    return [...exports.keywords, ...exports.types, ...exports.builtins, ...exports.stdModules, ...exports.stdFunctions];
}
