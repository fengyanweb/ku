const assert = require("assert");
const fs = require("fs");
const model = require("./out/completionModel");

assert.deepStrictEqual(model.memberCompletionLabels("http", "s"), ["status", "statusText", "service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "se"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "ser"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "serve"), ["server"]);
assert(!model.memberCompletionLabels("http", "s").some((label) => label.startsWith("std.")));
assert(!model.memberCompletionLabels("http", "s").some((label) => label.startsWith("http.")));
assert.deepStrictEqual(model.memberCompletionLabels("task", ""), ["stats", "stress"]);
assert.deepStrictEqual(model.memberCompletionLabels("task", "a"), []);
assert.deepStrictEqual(model.memberCompletionLabels("task", "st"), ["stats", "stress"]);
for (const label of ["status", "cancel", "await_timeout"]) {
  assert(!model.memberCompletionLabels("task", "").includes(label));
}
assert.deepStrictEqual(model.memberCompletionLabels("req", ""), ["method", "path", "params", "query", "headers", "body"]);
assert.deepStrictEqual(model.memberCompletionLabels("req", "p"), ["path", "params"]);
assert.deepStrictEqual(model.memberCompletionLabels("res", ""), []);
assert(model.memberCompletionLabels("http", "").includes("html"));
assert.deepStrictEqual(model.memberCompletionLabels("status", "not"), ["notModified", "notFound", "notAcceptable", "notImplemented"]);
assert.deepStrictEqual(model.memberCompletionLabels("code", "SUCCESS"), ["SUCCESS"]);
assert.deepStrictEqual(model.memberCompletionLabels("router", "g"), ["get"]);
assert.deepStrictEqual(model.memberCompletionLabels("app", "listen"), ["listen"]);
assert.deepStrictEqual(model.memberCompletionLabels("values", ""), ["len", "try_get", "push", "concat", "map"]);
assert.deepStrictEqual(model.memberCompletionLabels("time", "d"), ["date", "datetime", "duration", "diff", "days_in_month"]);
assert.deepStrictEqual(model.memberCompletionLabels("object", ""), ["get_or"]);
assert.deepStrictEqual(model.stdRootModules, ["fs", "http", "string", "array", "object", "json", "config", "time", "task"]);
assert(!model.keywords.includes("println"));
assert(model.builtins.includes("println"));
assert.strictEqual(model.globalCompletionLabels().filter((label) => label === "println").length, 1);

assert.deepStrictEqual(model.stdImportPathLabels("std.h"), ["std.http"]);
assert.deepStrictEqual(model.stdImportPathLabels("std."), [
  "std.fs",
  "std.http",
  "std.string",
  "std.array",
  "std.object",
  "std.json",
  "std.config",
  "std.time",
  "std.task",
]);

assert(!model.globalCompletionLabels().includes("http.service"));
assert(!model.globalCompletionLabels().includes("http.server"));
assert(model.globalCompletionLabels().includes("http.statusText"));
assert(model.globalCompletionLabels().includes("object.get_or"));
assert(model.globalCompletionLabels().includes("config.yaml"));
assert(model.globalCompletionLabels().includes("std.http"));
assert(model.globalCompletionLabels().includes("task.stress"));
assert(model.globalCompletionLabels().includes("async"));
assert(model.globalCompletionLabels().includes("await"));

const snippets = fs.readFileSync("snippets/ku.json", "utf8");
const httpSnippet = JSON.parse(snippets)["http server app"].body.join("\n");
for (const forbidden of ["req, res", "(req, res)", "res.write", "res.end", "reply.send", "writer"]) {
  assert(!httpSnippet.includes(forbidden), `http snippet must not include ${forbidden}`);
}
assert(httpSnippet.includes("http.service()"));
assert(httpSnippet.includes("fn health()"));
assert(httpSnippet.includes("app.get(\"/json\", fn(req)"));

console.log("ku completion model ok");
