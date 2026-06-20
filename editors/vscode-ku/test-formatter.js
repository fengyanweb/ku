const assert = require("assert");
const Module = require("module");

const originalLoad = Module._load;
class Position {
  constructor(line, character) {
    this.line = line;
    this.character = character;
  }
}
class Range {
  constructor(startLine, startColumn, endLine, endColumn) {
    this.start = new Position(startLine, startColumn);
    this.end = new Position(endLine, endColumn);
  }
}
class Diagnostic {
  constructor(range, message, severity) {
    this.range = range;
    this.message = message;
    this.severity = severity;
  }
}
Module._load = function (request, parent, isMain) {
  if (request === "vscode") {
    return {
      languages: {
        createDiagnosticCollection: () => ({ clear() {}, set() {} }),
      },
      window: {
        createOutputChannel: () => ({ append() {}, appendLine() {}, clear() {}, show() {} }),
      },
      workspace: {},
      commands: {},
      StatusBarAlignment: { Left: 1 },
      Uri: { file: (fsPath) => ({ fsPath, toString: () => `file://${fsPath}` }) },
      Range,
      Diagnostic,
      DiagnosticSeverity: { Error: 0, Warning: 1, Information: 2, Hint: 3 },
    };
  }
  return originalLoad.call(this, request, parent, isMain);
};

const { formatKu, parseDiagnostics } = require("./out/language");

const source = [
  "import   \"std.http\"   ",
  "",
  "",
  "",
  "fn main(){",
  "fn id<T>(value:T):T{",
  "return value",
  "}",
  "text=\"a  b {x}\"",
  "tpl=`keep  {a=1+2}`",
  "cfg=config.yaml(\"app.yaml\")?",
  "/* async await in block comment */",
  "/*",
  "{ keep braces out of indent }",
  "*/",
  "x=1+2",
  "route={path:\"/user/{id}\",method:\"GET\"}",
  "match x{",
  "1=>\"one\",",
  "_=>\"other\"",
  "}",
  "if(x>0){",
  "print(\"yes\")",
  "}else{",
  "print(\"done\")   ",
  "}",
  "try{",
  "value=1",
  "}catch(err){",
  "print(err.message)",
  "}finally{",
  "print(\"cleanup\")",
  "}",
  "}",
  "",
].join("\n");

const expected = [
  "import \"std.http\"",
  "",
  "fn main() {",
  "    fn id<T>(value: T): T {",
  "        return value",
  "    }",
  "    text = \"a  b {x}\"",
  "    tpl = `keep  {a=1+2}`",
  "    cfg = config.yaml(\"app.yaml\")?",
  "    /* async await in block comment */",
  "    /*",
  "    { keep braces out of indent }",
  "    */",
  "    x = 1 + 2",
  "    route = {path: \"/user/{id}\", method: \"GET\"}",
  "    match x {",
  "        1 => \"one\",",
  "        _ => \"other\"",
  "    }",
  "    if(x > 0) {",
  "        print(\"yes\")",
  "    } else {",
  "        print(\"done\")",
  "    }",
  "    try {",
  "        value = 1",
  "    } catch(err) {",
  "        print(err.message)",
  "    } finally {",
  "        print(\"cleanup\")",
  "    }",
  "}",
  "",
].join("\n");

assert.strictEqual(formatKu(source), expected);
assert.strictEqual(formatKu(expected), expected);

const document = {
  fileName: "sample.ku",
  uri: { fsPath: "sample.ku", toString: () => "file://sample.ku" },
};
const jsonDiagnostics = parseDiagnostics(
  [
    JSON.stringify({
      level: "error",
      code: "E0302",
      message: "condition must be bool",
      file: "sample.ku",
      line: 2,
      column: 5,
      endLine: 2,
      endColumn: 12,
      notes: ["Ku does not use truthy/falsy conditions"],
      helps: ["compare explicitly"],
    }),
    "",
  ].join("\n"),
  document,
);
assert.strictEqual(jsonDiagnostics.length, 1);
assert.strictEqual(jsonDiagnostics[0].code, "E0302");
assert.strictEqual(jsonDiagnostics[0].range.start.line, 1);
assert.strictEqual(jsonDiagnostics[0].range.start.character, 4);
assert.strictEqual(jsonDiagnostics[0].range.end.character, 11);
assert(jsonDiagnostics[0].message.includes("note: Ku does not use truthy/falsy conditions"));
assert(jsonDiagnostics[0].message.includes("help: compare explicitly"));

const textDiagnostics = parseDiagnostics(
  [
    "error[E0105]: error: 'let' is not supported",
    "  --> sample.ku:3:2",
    "   |",
    "  3 | let value = 1",
    "   | ^^^",
  ].join("\n"),
  document,
);
assert.strictEqual(textDiagnostics.length, 1);
assert.strictEqual(textDiagnostics[0].range.start.line, 2);
assert(textDiagnostics[0].message.includes("'let' is not supported"));

console.log("ku formatter ok");
