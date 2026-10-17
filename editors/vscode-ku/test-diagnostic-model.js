// Loaded by test-formatter.js, which supplies the minimal VS Code adapter.
const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { parseDiagnostics, loadDiagnosticSources } = require("./out/language");
const { scalarColumnToUtf16, sourceLine, readDiagnosticSource,
  MAX_DIAGNOSTIC_SOURCE_BYTES, MAX_DIAGNOSTIC_SOURCE_FILES } = require("./out/diagnosticModel");

const key = (file) => process.platform === "win32" ? path.resolve(file).toLowerCase() : path.resolve(file);
const uri = (file) => ({ fsPath: file, toString: () => `file://${file}` });
const record = (file, line, column, endLine, endColumn) => JSON.stringify({
  level: "error", code: "E0911", message: "borrowed value cannot escape", file,
  line, column, endLine, endColumn, notes: [], helps: ["clone explicitly"],
});

async function run() {
  assert.strictEqual(scalarColumnToUtf16("中😀x", 3), 3);
  assert.strictEqual(scalarColumnToUtf16("😀😀x", 3), 4);
  assert.strictEqual(scalarColumnToUtf16("e\u0301x", 3), 2);
  assert.strictEqual(scalarColumnToUtf16("\ufeff😀x", 3), 3);
  assert.strictEqual(scalarColumnToUtf16("😀", Number.MAX_SAFE_INTEGER), 2);
  assert.strictEqual(sourceLine("😀\r\nx\r\n", 2), "x");
  assert.strictEqual(sourceLine("😀\r\nx\r\n", 4), undefined);

  const source = 'first\r\nvalue = "中😀" + missing\r\n';
  const line = sourceLine(source, 2);
  const start = line.indexOf("missing");
  const column = [...line.slice(0, start)].length + 1;
  const document = { fileName: "root.ku", uri: uri("root.ku"), getText: () => source };
  const [diagnostic] = parseDiagnostics(record("root.ku", 2, column, 2, column + 7), document);
  assert.strictEqual(diagnostic.range.start.character, start);
  assert.strictEqual(diagnostic.range.end.character, start + 7);
  assert.strictEqual(diagnostic.code, "E0911");
  assert(!diagnostic.message.includes("source unavailable"));
  assert.strictEqual(parseDiagnostics("", document).length, 0);
  assert.strictEqual(parseDiagnostics(record("root.ku", 2, 1e100, 2, 1e100), document).length, 0);
  const [closedDocument] = parseDiagnostics(record("root.ku", 1, 1, 1, 2), {
    ...document, getText: () => { throw new Error("document was closed"); },
  });
  assert(closedDocument.message.includes("source unavailable"));

  const imported = path.resolve("imported.ku");
  const importedSource = 'x = "😀😀" + missing\r\n';
  const importedStart = importedSource.indexOf("missing");
  const importedColumn = [...importedSource.slice(0, importedStart)].length + 1;
  const [target] = parseDiagnostics(record(imported, 1, importedColumn, 1, importedColumn + 7),
    document, new Map([[key(imported), importedSource]]));
  assert.strictEqual(target.range.start.character, importedStart);
  assert.strictEqual(target.range.end.character, importedStart + 7);
  const [missing] = parseDiagnostics(record(imported, 1, importedColumn, 1, importedColumn + 7), document,
    new Map([[key(imported), undefined]]));
  assert.strictEqual(missing.range.start.character, 0);
  assert.strictEqual(missing.range.end.character, 0);
  assert(missing.message.includes("source unavailable"));
  const [multiline] = parseDiagnostics(record(imported, 1, 2, 2, 2), document,
    new Map([[key(imported), "😀x\r\n😀y"]]));
  assert.strictEqual(multiline.range.start.character, 2);
  assert.strictEqual(multiline.range.end.character, 2);
  const [text] = parseDiagnostics(`error: invalid value\n  --> ${imported}:1:${importedColumn}\n   | ^^^^^^^`,
    document, new Map([[key(imported), importedSource]]));
  assert.strictEqual(text.range.start.character, importedStart);
  assert.strictEqual(text.range.end.character, importedStart + 7);

  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "ku-editor-diagnostic-"));
  const files = [];
  const create = (name, content) => {
    const file = path.join(directory, name);
    fs.writeFileSync(file, content);
    files.push(file);
    return file;
  };
  try {
    const actual = create("imported.ku", importedSource);
    assert.strictEqual(await readDiagnosticSource(actual), importedSource);
    assert.strictEqual(await readDiagnosticSource(create("bom.ku", "\ufefffn main() {}")), "\ufefffn main() {}");
    assert.strictEqual(await readDiagnosticSource(create("empty.ku", "")), "");
    assert.strictEqual(await readDiagnosticSource(create("bad-utf8.ku", Buffer.from([0xff]))), undefined);
    assert.strictEqual(await readDiagnosticSource(create("large.ku", Buffer.alloc(MAX_DIAGNOSTIC_SOURCE_BYTES + 1))), undefined);
    assert.strictEqual(await readDiagnosticSource(directory), undefined);
    assert.strictEqual(await readDiagnosticSource(path.join(directory, "absent.ku")), undefined);
    const loaded = await loadDiagnosticSources([{ uri: uri(actual) }], document);
    assert.strictEqual(loaded.get(key(actual)), importedSource);
    const entries = [];
    for (let i = 0; i < MAX_DIAGNOSTIC_SOURCE_FILES + 1; i++) {
      entries.push({ uri: uri(create(`file${i}.ku`, "😀x")) });
    }
    const bounded = await loadDiagnosticSources(entries, document);
    assert.strictEqual(bounded.size, MAX_DIAGNOSTIC_SOURCE_FILES);
    assert(!bounded.has(key(entries.at(-1).uri.fsPath)));

    const originalOpen = fs.promises.open;
    const completions = [];
    let closed = 0;
    try {
      fs.promises.open = () => new Promise((resolve) => completions.push(resolve));
      assert.deepStrictEqual(await Promise.all([0, 1, 2, 3].map(() => readDiagnosticSource(actual))),
        [undefined, undefined, undefined, undefined]);
      assert.strictEqual(await readDiagnosticSource(actual), undefined);
      assert.strictEqual(completions.length, 4, "timed-out reads must retain the global admission slot until their handle closes");
      for (const complete of completions) complete({ close: async () => { closed++; } });
      await new Promise(setImmediate);
      assert.strictEqual(closed, 4, "late filesystem completion must close every handle");
    } finally {
      fs.promises.open = originalOpen;
    }
    assert.strictEqual(await readDiagnosticSource(actual), importedSource, "completed reads must release admission slots");
  } finally {
    for (const file of files) fs.unlinkSync(file);
    fs.rmdirSync(directory);
  }
  console.log("ku diagnostic Unicode, imported-source and bounded-read contracts ok");
}

run().catch((error) => { console.error(error); process.exitCode = 1; });
