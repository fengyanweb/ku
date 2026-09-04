const assert = require("assert");
const { execFileExitCode, firstWorkingExecutable } = require("./out/executableModel");

assert.strictEqual(execFileExitCode(null), 0);
assert.strictEqual(execFileExitCode({ code: 7 }), 7);
assert.notStrictEqual(execFileExitCode({ code: 0 }), 0);
assert.notStrictEqual(execFileExitCode({ code: "ENOENT" }), 0);
assert.notStrictEqual(execFileExitCode({}), 0);

async function select(candidates, outcomes) {
  const visited = [];
  const selected = await firstWorkingExecutable(candidates, async (candidate) => {
    visited.push(candidate);
    return { code: outcomes.get(candidate) ?? 1 };
  });
  return { selected, visited };
}

(async () => {
  const bundle = "workspace/release/x86_64-pc-windows-msvc/ku.exe";
  assert.deepStrictEqual(
    await select(["ku", bundle], new Map([[bundle, 0]])),
    { selected: bundle, visited: ["ku", bundle] },
  );

  assert.deepStrictEqual(
    await select(["ku", bundle], new Map()),
    { selected: undefined, visited: ["ku", bundle] },
  );

  assert.deepStrictEqual(
    await select(["missing-configured-ku", "ku", bundle], new Map([[bundle, 0]])),
    { selected: bundle, visited: ["missing-configured-ku", "ku", bundle] },
  );

  assert.deepStrictEqual(
    await select(["ku", "ku", bundle], new Map([[bundle, 0]])),
    { selected: bundle, visited: ["ku", bundle] },
  );

  console.log("ku executable discovery model ok");
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
