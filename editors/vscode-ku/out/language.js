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
exports.activate = activate;
exports.deactivate = deactivate;
const cp = __importStar(require("child_process"));
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const vscode = __importStar(require("vscode"));
const imports_1 = require("./imports");
const KU_VERSION = "0.0.12";
const KU_MODE = [{ language: "ku", scheme: "file" }];
const diagnosticCollection = vscode.languages.createDiagnosticCollection("ku");
const output = vscode.window.createOutputChannel("Ku");
let status;
const checkTimers = new Map();
const keywords = [
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
const types = ["int", "float", "bool", "str", "null"];
const builtins = ["len", "str", "ok", "err", "println"];
const stdModules = ["std.fs", "std.http", "std.string", "std.array", "std.json", "std.time"];
const stdFunctions = [
    "fs.read",
    "fs.try_read",
    "fs.write",
    "fs.try_write",
    "http.get",
    "http.post",
    "http.request",
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
    "time.now",
    "time.unix",
    "time.millis",
];
function activate(context) {
    context.subscriptions.push(diagnosticCollection, output);
    status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 90);
    status.command = "ku.showVersion";
    context.subscriptions.push(status);
    context.subscriptions.push(vscode.commands.registerCommand("ku.runCurrentFile", () => runKuCommand("run")), vscode.commands.registerCommand("ku.checkCurrentFile", () => checkActiveFile(true)), vscode.commands.registerCommand("ku.showIr", () => runKuCommand("ir")), vscode.commands.registerCommand("ku.buildCurrentFile", () => runKuCommand("build")), vscode.commands.registerCommand("ku.buildNativeC", () => buildNativeC()), vscode.commands.registerCommand("ku.packageGc", () => runKuCommand("package gc")), vscode.commands.registerCommand("ku.showVersion", () => showVersion()), vscode.workspace.onDidOpenTextDocument((doc) => {
        if (isKu(doc) && config().get("checkOnOpen", true)) {
            void scheduleCheck(doc, 0);
        }
        void refreshEditorContext(doc);
        if (doc.fileName.endsWith("ku.lock")) {
            void vscode.window.showInformationMessage("ku.lock 是生成文件，通常不建议手动编辑。");
        }
    }), vscode.workspace.onDidSaveTextDocument((doc) => {
        if (isKu(doc) && config().get("checkOnSave", true)) {
            void scheduleCheck(doc, 0);
        }
        void refreshEditorContext(doc);
    }), vscode.workspace.onDidChangeTextDocument((event) => {
        if (isKu(event.document) && config().get("checkOnChange", false)) {
            void scheduleCheck(event.document, 500);
        }
        void refreshEditorContext(event.document);
    }), vscode.window.onDidChangeActiveTextEditor(() => {
        void refreshStatus();
        void refreshEditorContext();
    }), vscode.languages.registerCompletionItemProvider(KU_MODE, new KuCompletionProvider(), ".", "\"", "'", "/", "{", "@"), vscode.languages.registerHoverProvider(KU_MODE, new KuHoverProvider()), vscode.languages.registerDefinitionProvider(KU_MODE, new KuDefinitionProvider()), vscode.languages.registerDocumentSymbolProvider(KU_MODE, new KuSymbolProvider()), vscode.languages.registerCodeActionsProvider(KU_MODE, new KuCodeActionProvider(), {
        providedCodeActionKinds: [vscode.CodeActionKind.QuickFix],
    }), vscode.languages.registerDocumentFormattingEditProvider(KU_MODE, new KuFormatter()));
    for (const doc of vscode.workspace.textDocuments) {
        if (isKu(doc) && config().get("checkOnOpen", true)) {
            void scheduleCheck(doc, 0);
        }
    }
    void refreshStatus();
    void refreshEditorContext();
}
function deactivate() {
    diagnosticCollection.clear();
}
function config() {
    return vscode.workspace.getConfiguration("ku");
}
function isKu(document) {
    return document.languageId === "ku" && document.uri.scheme === "file";
}
async function refreshEditorContext(document = vscode.window.activeTextEditor?.document) {
    const hasMain = !!document && isKu(document) && documentHasMain(document);
    await vscode.commands.executeCommand("setContext", "ku.hasMain", hasMain);
}
function documentHasMain(document) {
    return /^\s*fn\s+main\s*\(/m.test(document.getText());
}
async function scheduleCheck(document, delayMs) {
    const key = document.uri.toString();
    const existing = checkTimers.get(key);
    if (existing) {
        clearTimeout(existing);
    }
    checkTimers.set(key, setTimeout(() => {
        checkTimers.delete(key);
        void runCheck(document, false);
    }, delayMs));
}
async function checkActiveFile(reveal) {
    const editor = vscode.window.activeTextEditor;
    if (!editor || !isKu(editor.document)) {
        void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
        return;
    }
    await runCheck(editor.document, reveal);
}
async function runCheck(document, reveal) {
    const exe = await findKuExecutable(document.uri);
    if (!exe) {
        setStatus("Ku: missing", true);
        return;
    }
    const result = await execFile(exe, ["check", document.uri.fsPath], workspaceFolder(document.uri));
    output.clear();
    output.appendLine(`> ${exe} check ${document.uri.fsPath}`);
    output.append(result.stdout);
    output.append(result.stderr);
    diagnosticCollection.set(document.uri, parseDiagnostics(result.stdout + result.stderr, document));
    if (reveal) {
        output.show(true);
    }
    setStatus(result.code === 0 ? `Ku ${KU_VERSION}: check ok` : `Ku ${KU_VERSION}: check failed`, result.code !== 0);
}
function parseDiagnostics(text, document) {
    const diagnostics = [];
    const lines = text.split(/\r?\n/);
    for (let i = 0; i < lines.length; i++) {
        const location = /^\s*-->\s+(.+):(\d+):(\d+)\s*$/.exec(lines[i]);
        if (!location) {
            continue;
        }
        const message = cleanupMessage(lines.slice(0, i).reverse().find((line) => line.trim()) ?? "Ku check failed");
        const line = Math.max(0, Number(location[2]) - 1);
        const col = Math.max(0, Number(location[3]) - 1);
        let endCol = col + 1;
        const marker = lines.slice(i + 1, i + 5).find((lineText) => lineText.includes("^"));
        if (marker) {
            const first = marker.indexOf("^");
            const last = marker.lastIndexOf("^");
            if (first >= 0 && last >= first) {
                endCol = col + Math.max(1, last - first + 1);
            }
        }
        const range = new vscode.Range(line, col, line, endCol);
        const diagnostic = new vscode.Diagnostic(range, `${message}${hintFor(message)}`, vscode.DiagnosticSeverity.Error);
        diagnostic.source = "ku check";
        diagnostics.push(diagnostic);
    }
    return diagnostics;
}
function cleanupMessage(message) {
    return message.replace(/^error:\s+/, "").trim();
}
function hintFor(message) {
    if (message.includes("std module 'http' must be imported")) {
        return "\nhelp: add import \"std.http\"";
    }
    if (message.includes("std module 'fs' must be imported")) {
        return "\nhelp: add import \"std.fs\"";
    }
    if (message.includes("expected numbers")) {
        return "\nhint: 普通表达式不允许 str 和数字混合运算；模板字符串内才允许拼接。";
    }
    if (message.includes("switch is not supported")) {
        return "\nhelp: use match instead.";
    }
    if (message.includes("let syntax is not supported")) {
        return "\nhelp: Ku uses name = value, not let.";
    }
    return "";
}
async function runKuCommand(command) {
    const editor = vscode.window.activeTextEditor;
    if (!editor || !isKu(editor.document)) {
        void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
        return;
    }
    if (command === "run" && !documentHasMain(editor.document)) {
        void vscode.window.showWarningMessage("当前 Ku 文件没有 fn main()，不能运行。");
        return;
    }
    await editor.document.save();
    const exe = await findKuExecutable(editor.document.uri);
    if (!exe) {
        return;
    }
    const cwd = terminalCwd(editor.document.uri);
    const terminal = vscode.window.createTerminal({ name: `Ku ${command}`, cwd });
    terminal.show();
    terminal.sendText(terminalCommand(exe, [...command.split(" "), editor.document.uri.fsPath], cwd));
}
async function buildNativeC() {
    const editor = vscode.window.activeTextEditor;
    if (!editor || !isKu(editor.document)) {
        void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
        return;
    }
    const unsupported = detectNativeUnsupported(editor.document.getText());
    if (unsupported.length > 0) {
        const answer = await vscode.window.showWarningMessage(`当前 native C prototype 不支持：${unsupported.join(", ")}。是否仍然继续构建？`, "继续构建", "取消");
        if (answer !== "继续构建") {
            return;
        }
    }
    const exe = await findKuExecutable(editor.document.uri);
    if (!exe) {
        return;
    }
    const cwd = terminalCwd(editor.document.uri);
    const terminal = vscode.window.createTerminal({ name: "Ku Native C", cwd });
    terminal.show();
    terminal.sendText(terminalCommand(exe, ["build", "--native", editor.document.uri.fsPath], cwd));
}
function detectNativeUnsupported(source) {
    const checks = [
        ["array", /\[[^\]\n,]+(?:,[^\]\n]+)+\]/],
        ["struct", /\bstruct\b|\b[A-Z][A-Za-z0-9_]*\s*\{/],
        ["enum", /\benum\b|[A-Z][A-Za-z0-9_]*\.[A-Z][A-Za-z0-9_]*\(/],
        ["closure", /=>/],
        ["match", /\bmatch\b/],
        ["try/catch", /\btry\b|\bcatch\b|\bfinally\b/],
    ];
    return checks.filter(([, re]) => re.test(source)).map(([name]) => name);
}
async function showVersion() {
    const exe = await findKuExecutable(vscode.window.activeTextEditor?.document.uri);
    if (!exe) {
        return;
    }
    const result = await execFile(exe, ["version"], workspaceFolder(vscode.window.activeTextEditor?.document.uri));
    const version = (result.stdout || result.stderr).trim();
    void vscode.window.showInformationMessage(`${version} | plugin ${KU_VERSION}`);
    await refreshStatus();
}
async function refreshStatus() {
    const exe = await findKuExecutable(vscode.window.activeTextEditor?.document.uri, false);
    if (!exe) {
        setStatus("Ku: executable missing", true);
        return;
    }
    const result = await execFile(exe, ["version"], workspaceFolder(vscode.window.activeTextEditor?.document.uri));
    const actual = /ku\s+([0-9]+\.[0-9]+\.[0-9]+)/.exec(result.stdout + result.stderr)?.[1] ?? "unknown";
    setStatus(actual === KU_VERSION ? `Ku ${actual}` : `Ku ${actual} / plugin ${KU_VERSION}`, actual !== KU_VERSION);
}
function setStatus(text, warn) {
    status.text = warn ? `$(warning) ${text}` : `$(check) ${text}`;
    status.tooltip = "Ku interpreter and extension version";
    status.show();
}
async function findKuExecutable(uri, notify = true) {
    const configured = config().get("executablePath", "").trim();
    const candidates = [];
    if (configured) {
        candidates.push(configured);
    }
    const folder = workspaceFolder(uri);
    if (folder) {
        candidates.push(path.join(folder, "release", exeName()), path.join(folder, "target", "release", exeName()), path.join(folder, "target", "debug", exeName()));
    }
    candidates.push("ku");
    for (const candidate of candidates) {
        const result = await execFile(candidate, ["version"], folder, 3000);
        if (result.code === 0) {
            return candidate;
        }
    }
    if (notify) {
        void vscode.window.showErrorMessage("找不到 ku 解释器。请设置 ku.executablePath，或把 ku.exe 加入 PATH。");
    }
    return undefined;
}
function exeName() {
    return process.platform === "win32" ? "ku.exe" : "ku";
}
function workspaceFolder(uri) {
    const folder = uri ? vscode.workspace.getWorkspaceFolder(uri) : vscode.workspace.workspaceFolders?.[0];
    return folder?.uri.fsPath;
}
function terminalCwd(uri) {
    return workspaceFolder(uri) ?? path.dirname(uri.fsPath);
}
function execFile(file, args, cwd, timeoutMs = 15000) {
    return new Promise((resolve) => {
        cp.execFile(file, args, { cwd, timeout: timeoutMs, windowsHide: true }, (error, stdout, stderr) => {
            const code = typeof error?.code === "number" ? error.code : 0;
            resolve({ code, stdout: stdout.toString(), stderr: stderr.toString() });
        });
    });
}
function terminalCommand(exe, args, cwd) {
    const quoted = [shortPath(exe, cwd), ...args.map((arg) => shortPath(arg, cwd))].map(shellQuote).join(" ");
    return process.platform === "win32" ? `& ${quoted}` : quoted;
}
function shellQuote(value) {
    if (/^[A-Za-z0-9_./\\:-]+$/.test(value)) {
        return value;
    }
    if (process.platform === "win32") {
        return `"${value.replace(/"/g, '`"')}"`;
    }
    return `"${value.replace(/(["\\$`])/g, "\\$1")}"`;
}
function shortPath(value, cwd) {
    if (!path.isAbsolute(value)) {
        return value;
    }
    const relative = path.relative(cwd, value);
    if (!relative || relative.startsWith("..") || path.isAbsolute(relative)) {
        return value;
    }
    const normalized = relative.replace(/\//g, "\\");
    return normalized.startsWith(".") ? normalized : `.${path.sep}${normalized}`;
}
class KuCompletionProvider {
    async provideCompletionItems(document, position) {
        const linePrefix = document.lineAt(position).text.slice(0, position.character);
        const items = [];
        if (isImportPathContext(linePrefix)) {
            return await importPathCompletions(document, linePrefix);
        }
        if (isNamedImportContext(document, position)) {
            return exportNameCompletions(document, position);
        }
        if (/@dep\/?$/.test(linePrefix)) {
            return dependencyCompletions(document);
        }
        if (/\berr\.$/.test(linePrefix)) {
            return fieldCompletions(["domain", "code", "message"], "Error field");
        }
        if (/\b(res|response)\.$/.test(linePrefix)) {
            return fieldCompletions(["status", "headers", "body"], "HttpResponse field");
        }
        if (/["'`][^"'`]*\.$/.test(linePrefix) || /\btext\.$/.test(linePrefix)) {
            return methodCompletions(["trim", "lower", "upper", "len", "slice"]);
        }
        if (/\b(values|items|nums)\.$/.test(linePrefix)) {
            return methodCompletions(["len", "is_empty", "first", "last", "try_get", "push", "concat", "map"]);
        }
        for (const value of keywords) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.Keyword));
        }
        for (const value of types) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.TypeParameter));
        }
        for (const value of builtins) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.Function));
        }
        for (const value of stdModules) {
            const item = new vscode.CompletionItem(value, vscode.CompletionItemKind.Module);
            item.insertText = `import "${value}"`;
            items.push(item);
        }
        for (const value of stdFunctions) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.Function));
        }
        return items;
    }
}
function isImportPathContext(linePrefix) {
    return /^\s*import\b.*["'][^"']*$/.test(linePrefix);
}
async function importPathCompletions(document, linePrefix) {
    const quoteMatch = /["']([^"']*)$/.exec(linePrefix);
    const current = quoteMatch?.[1] ?? "";
    if (current.startsWith("std.")) {
        return stdModules.map((module) => new vscode.CompletionItem(module, vscode.CompletionItemKind.Module));
    }
    if (current.startsWith("@")) {
        return dependencyCompletions(document);
    }
    const base = path.dirname(document.uri.fsPath);
    const prefix = current.startsWith("/") ? current : path.resolve(base, current || ".");
    const dir = fs.existsSync(prefix) && fs.statSync(prefix).isDirectory() ? prefix : path.dirname(prefix);
    if (!fs.existsSync(dir)) {
        return [];
    }
    return fs
        .readdirSync(dir, { withFileTypes: true })
        .filter((entry) => entry.isDirectory() || entry.name.endsWith(".ku"))
        .map((entry) => {
        const item = new vscode.CompletionItem(entry.name.replace(/\.ku$/, ""), entry.isDirectory() ? vscode.CompletionItemKind.Folder : vscode.CompletionItemKind.File);
        if (entry.isDirectory()) {
            item.insertText = `${entry.name}/`;
        }
        return item;
    });
}
function dependencyCompletions(document) {
    const deps = readKuModDependencies(document);
    return deps.map((dep) => {
        const item = new vscode.CompletionItem(`@${dep}/`, vscode.CompletionItemKind.Module);
        item.detail = "ku.mod dependency";
        return item;
    });
}
function isNamedImportContext(document, position) {
    const before = document.getText(new vscode.Range(new vscode.Position(position.line, 0), position));
    return /^\s*import\s+\{[^}]*$/.test(before);
}
function exportNameCompletions(document, position) {
    const line = document.lineAt(position).text;
    const importPath = /from\s+["']([^"']+)["']/.exec(line)?.[1];
    if (!importPath) {
        return [];
    }
    const uri = (0, imports_1.resolveImportUri)(document, importPath);
    if (!uri || !fs.existsSync(uri.fsPath)) {
        return [];
    }
    return exportedNames(fs.readFileSync(uri.fsPath, "utf8")).map((name) => new vscode.CompletionItem(name, vscode.CompletionItemKind.Reference));
}
function exportedNames(source) {
    const names = new Set();
    for (const match of source.matchAll(/^\s*(?:fn|struct|enum)\s+([A-Z][A-Za-z0-9_]*)/gm)) {
        names.add(match[1]);
    }
    for (const match of source.matchAll(/^\s*([A-Z][A-Za-z0-9_]*)\s*=/gm)) {
        names.add(match[1]);
    }
    return [...names].sort();
}
function fieldCompletions(fields, detail) {
    return fields.map((field) => {
        const item = new vscode.CompletionItem(field, vscode.CompletionItemKind.Field);
        item.detail = detail;
        return item;
    });
}
function methodCompletions(methods) {
    return methods.map((method) => new vscode.CompletionItem(method, vscode.CompletionItemKind.Method));
}
class KuHoverProvider {
    provideHover(document, position) {
        const word = document.getText(document.getWordRangeAtPosition(position));
        const docs = {
            "catch": "`catch (err)` 中 `err` 是结构化 Error 对象：`err.domain`、`err.code`、`err.message`。",
            "err": "`err(message)` 返回 `Unknown!`，失败 payload 是 `{ domain, code, message }`。",
            "fail": "`fail` 主动返回可恢复错误；字符串会包装为 `{ domain: \"ku\", code: \"fail\", message }`。",
            "http": "`import \"std.http\"` 后使用。`http.get/post/request` 返回 `{ status, headers, body }!`。",
            "fs": "`import \"std.fs\"` 后使用。支持 `fs.read/write/try_read/try_write`。",
            "match": "Ku 0.0.12 保留 `match`，不再支持 `switch`。",
            "try_get": "`values.try_get(index)?` 越界时返回结构化 Error。",
            "trim": "`text.trim()` 是 string 实例方法。",
        };
        const text = docs[word];
        return text ? new vscode.Hover(new vscode.MarkdownString(text)) : undefined;
    }
}
class KuDefinitionProvider {
    provideDefinition(document, position) {
        const range = document.getWordRangeAtPosition(position);
        if (!range) {
            return undefined;
        }
        const word = document.getText(range);
        const importDef = importDefinition(document, position);
        if (importDef) {
            return importDef;
        }
        const sameFile = findDefinitionInDocument(document, word);
        if (sameFile) {
            return sameFile;
        }
        for (const imp of (0, imports_1.parseImports)(document)) {
            const uri = (0, imports_1.resolveImportUri)(document, imp.path);
            if (!uri || !fs.existsSync(uri.fsPath)) {
                continue;
            }
            const source = fs.readFileSync(uri.fsPath, "utf8");
            const target = findDefinitionInText(uri, source, word);
            if (target) {
                return target;
            }
        }
        return undefined;
    }
}
function importDefinition(document, position) {
    for (const imp of (0, imports_1.parseImports)(document)) {
        if (imp.pathRange.contains(position)) {
            const uri = (0, imports_1.resolveImportUri)(document, imp.path);
            return uri && fs.existsSync(uri.fsPath) ? new vscode.Location(uri, new vscode.Position(0, 0)) : undefined;
        }
    }
    return undefined;
}
function findDefinitionInDocument(document, word) {
    return findDefinitionInText(document.uri, document.getText(), word);
}
function findDefinitionInText(uri, source, word) {
    const escaped = word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const patterns = [
        new RegExp(`^\\s*fn\\s+${escaped}\\b`, "m"),
        new RegExp(`^\\s*struct\\s+${escaped}\\b`, "m"),
        new RegExp(`^\\s*enum\\s+${escaped}\\b`, "m"),
        new RegExp(`^\\s*${escaped}\\s*=`, "m"),
        new RegExp(`^\\s*${escaped}(?:\\(|$)`, "m"),
    ];
    for (const pattern of patterns) {
        const match = pattern.exec(source);
        if (match) {
            const before = source.slice(0, match.index);
            const line = before.split(/\r?\n/).length - 1;
            const col = match[0].search(/\S/);
            return new vscode.Location(uri, new vscode.Position(line, Math.max(0, col)));
        }
    }
    return undefined;
}
class KuSymbolProvider {
    provideDocumentSymbols(document) {
        const symbols = [];
        const stack = [];
        for (let line = 0; line < document.lineCount; line++) {
            const text = document.lineAt(line).text;
            const match = /^\s*(module|fn|struct|enum)\s+([A-Za-z_][A-Za-z0-9_]*)/.exec(text);
            if (!match) {
                continue;
            }
            const kind = match[1] === "fn" ? vscode.SymbolKind.Function : match[1] === "struct" ? vscode.SymbolKind.Struct : match[1] === "enum" ? vscode.SymbolKind.Enum : vscode.SymbolKind.Module;
            const range = new vscode.Range(line, 0, line, text.length);
            const symbol = new vscode.DocumentSymbol(match[2], match[1], kind, range, range);
            if (/^\s+fn\b/.test(text) && stack.length > 0) {
                stack[stack.length - 1].children.push(symbol);
            }
            else {
                symbols.push(symbol);
            }
            if (match[1] !== "fn" || !/^\s+fn\b/.test(text)) {
                stack[0] = symbol;
            }
        }
        return symbols;
    }
}
class KuCodeActionProvider {
    provideCodeActions(document, _range, context) {
        const actions = [];
        for (const diagnostic of context.diagnostics) {
            if (diagnostic.message.includes("std module 'http' must be imported")) {
                actions.push(insertImportAction(document, "std.http"));
            }
            if (diagnostic.message.includes("std module 'fs' must be imported")) {
                actions.push(insertImportAction(document, "std.fs"));
            }
            if (diagnostic.message.includes("let")) {
                const action = new vscode.CodeAction("Ku: remove let keyword", vscode.CodeActionKind.QuickFix);
                action.edit = new vscode.WorkspaceEdit();
                const line = document.lineAt(diagnostic.range.start.line);
                const idx = line.text.indexOf("let ");
                if (idx >= 0) {
                    action.edit.delete(document.uri, new vscode.Range(line.lineNumber, idx, line.lineNumber, idx + 4));
                    actions.push(action);
                }
            }
            if (diagnostic.message.includes("switch")) {
                const action = new vscode.CodeAction("Ku: replace switch with match", vscode.CodeActionKind.QuickFix);
                action.edit = new vscode.WorkspaceEdit();
                action.edit.replace(document.uri, diagnostic.range, "match");
                actions.push(action);
            }
        }
        return actions;
    }
}
function insertImportAction(document, module) {
    const action = new vscode.CodeAction(`Ku: add import "${module}"`, vscode.CodeActionKind.QuickFix);
    action.edit = new vscode.WorkspaceEdit();
    action.edit.insert(document.uri, new vscode.Position(0, 0), `import "${module}"\n`);
    return action;
}
class KuFormatter {
    provideDocumentFormattingEdits(document) {
        const text = document.getText();
        const formatted = formatKu(text);
        if (formatted === text) {
            return [];
        }
        const full = new vscode.Range(document.positionAt(0), document.positionAt(text.length));
        return [vscode.TextEdit.replace(full, formatted)];
    }
}
function formatKu(source) {
    const lines = source.replace(/\r\n/g, "\n").split("\n");
    let indent = 0;
    const out = [];
    for (const raw of lines) {
        const trimmed = raw.trim();
        if (!trimmed) {
            out.push("");
            continue;
        }
        if (/^[}\])]/.test(trimmed)) {
            indent = Math.max(0, indent - 1);
        }
        out.push(`${"    ".repeat(indent)}${trimmed}`);
        const opens = (trimmed.match(/[{\[(]/g) ?? []).length;
        const closes = (trimmed.match(/[}\])]/g) ?? []).length;
        indent = Math.max(0, indent + opens - closes);
    }
    return out.join("\n");
}
function readKuModDependencies(document) {
    const root = workspaceFolder(document.uri);
    if (!root) {
        return [];
    }
    const manifest = path.join(root, "ku.mod");
    if (!fs.existsSync(manifest)) {
        return [];
    }
    const text = fs.readFileSync(manifest, "utf8");
    return [...text.matchAll(/^\s*([A-Za-z_][A-Za-z0-9_-]*)\.(?:version|source|checksum)\s*=/gm)].map((m) => m[1]);
}
