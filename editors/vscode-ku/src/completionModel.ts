export const keywords = [
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
  "true",
  "false",
  "null",
];

export const types = ["int", "float", "bool", "str", "null"];
export const builtins = ["len", "str", "ok", "err", "println"];
export const stdModules = ["std.fs", "std.http", "std.string", "std.array", "std.json", "std.config", "std.time", "std.task"];
export const stdRootModules = ["fs", "http", "string", "array", "json", "config", "time", "task"];

export const stdFunctions = [
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
  "time.from_unix",
  "time.from_millis",
  "time.date",
  "time.datetime",
  "time.format",
  "time.parse",
  "time.duration",
  "time.add",
  "time.sub",
  "time.diff",
  "time.compare",
  "time.parts",
  "time.weekday",
  "time.is_leap",
  "time.days_in_month",
  "time.sleep",
  "task.stats",
  "task.stress",
];

export const namespaceMembers: Record<string, string[]> = {
  fs: ["read", "try_read", "write", "try_write"],
  http: ["get", "post", "request", "client", "text", "json", "service", "server"],
  string: ["len", "trim", "lower", "upper", "slice"],
  array: ["len", "try_get", "push", "concat"],
  json: ["parse", "try_parse", "stringify"],
  config: ["env", "env_file", "yaml"],
  time: [
    "now",
    "unix",
    "millis",
    "from_unix",
    "from_millis",
    "date",
    "datetime",
    "format",
    "parse",
    "duration",
    "add",
    "sub",
    "diff",
    "compare",
    "parts",
    "weekday",
    "is_leap",
    "days_in_month",
    "sleep",
  ],
  task: ["stats", "stress"],
};

export function stdImportPathLabels(current: string): string[] {
  if (!current.startsWith("std.")) {
    return [];
  }
  return stdModules.filter((module) => module.startsWith(current));
}

export function memberCompletionLabels(receiver: string, prefix = ""): string[] {
  const members = namespaceMembers[receiver] ?? [];
  return members.filter((member) => member.startsWith(prefix));
}

export function globalCompletionLabels(): string[] {
  return [...keywords, ...types, ...builtins, ...stdModules, ...stdFunctions];
}
