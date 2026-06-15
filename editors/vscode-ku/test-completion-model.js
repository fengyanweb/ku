const assert = require("assert");
const model = require("./out/completionModel");

assert.deepStrictEqual(model.memberCompletionLabels("http", "s"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "se"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "ser"), ["service", "server"]);
assert.deepStrictEqual(model.memberCompletionLabels("http", "serve"), ["server"]);
assert(!model.memberCompletionLabels("http", "s").some((label) => label.startsWith("std.")));
assert(!model.memberCompletionLabels("http", "s").some((label) => label.startsWith("http.")));

assert.deepStrictEqual(model.stdImportPathLabels("std.h"), ["std.http"]);
assert.deepStrictEqual(model.stdImportPathLabels("std."), [
  "std.fs",
  "std.http",
  "std.string",
  "std.array",
  "std.json",
  "std.time",
]);

assert(model.globalCompletionLabels().includes("http.server"));
assert(model.globalCompletionLabels().includes("std.http"));

console.log("ku completion model ok");
