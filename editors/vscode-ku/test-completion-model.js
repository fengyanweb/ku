const assert = require("assert");
const model = require("./out/completionModel");

assert.deepStrictEqual(model.memberCompletionLabels("http", "s"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "se"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "ser"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "serve"), ["server"]);
assert(!model.memberCompletionLabels("http", "s").some((label) => label.startsWith("std.")));
assert(!model.memberCompletionLabels("http", "s").some((label) => label.startsWith("http.")));
assert.deepStrictEqual(model.memberCompletionLabels("task", ""), ["status", "cancel", "await_timeout", "stats", "stress"]);
assert.deepStrictEqual(model.memberCompletionLabels("task", "a"), ["await_timeout"]);
assert.deepStrictEqual(model.memberCompletionLabels("task", "st"), ["status", "stats", "stress"]);
assert.deepStrictEqual(model.memberCompletionLabels("time", "d"), ["date", "datetime", "duration", "diff", "days_in_month"]);
assert.deepStrictEqual(model.stdRootModules, ["fs", "http", "string", "array", "json", "config", "time", "task"]);
assert(!model.keywords.includes("println"));
assert(model.builtins.includes("println"));
assert.strictEqual(model.globalCompletionLabels().filter((label) => label === "println").length, 1);

assert.deepStrictEqual(model.stdImportPathLabels("std.h"), ["std.http"]);
assert.deepStrictEqual(model.stdImportPathLabels("std."), [
  "std.fs",
  "std.http",
  "std.string",
  "std.array",
  "std.json",
  "std.config",
  "std.time",
  "std.task",
]);

assert(model.globalCompletionLabels().includes("http.server"));
assert(model.globalCompletionLabels().includes("config.yaml"));
assert(model.globalCompletionLabels().includes("std.http"));
assert(model.globalCompletionLabels().includes("task.stress"));
assert(model.globalCompletionLabels().includes("async"));
assert(model.globalCompletionLabels().includes("await"));

console.log("ku completion model ok");
