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
exports.formatKu = void 0;
exports.activate = activate;
exports.deactivate = deactivate;
exports.parseDiagnostics = parseDiagnostics;
exports.loadDiagnosticSources = loadDiagnosticSources;
const cp = __importStar(require("child_process"));
const fs = __importStar(require("fs"));
const os = __importStar(require("os"));
const path = __importStar(require("path"));
const vscode = __importStar(require("vscode"));
const completionModel_1 = require("./completionModel");
const executableModel_1 = require("./executableModel");
const imports_1 = require("./imports");
const formatter_1 = require("./formatter");
var formatter_2 = require("./formatter");
Object.defineProperty(exports, "formatKu", { enumerable: true, get: function () { return formatter_2.formatKu; } });
const diagnosticModel_1 = require("./diagnosticModel");
const KU_VERSION = require("../package.json").version;
const KU_MODE = [{ language: "ku", scheme: "file" }];
const diagnosticCollection = vscode.languages.createDiagnosticCollection("ku");
const output = vscode.window.createOutputChannel("Ku");
let status;
const checkTimers = new Map();
const checkGenerations = new Map();
const diagnosticsByRoot = new Map();
const pendingChecks = new Map();
const activeChecks = new Map();
const MAX_ACTIVE_CHECKS = 2;
let diagnosticSession = 0;
let diagnosticsEnabled = false;
function activate(context) {
    diagnosticSession++;
    diagnosticsEnabled = true;
    context.subscriptions.push(diagnosticCollection, output);
    status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 90);
    status.command = "ku.showVersion";
    context.subscriptions.push(status);
    context.subscriptions.push(vscode.commands.registerCommand("ku.runCurrentFile", (uri) => runKuCommand("run", uri)), vscode.commands.registerCommand("ku.checkCurrentFile", (uri) => checkActiveFile(true, uri)), vscode.commands.registerCommand("ku.showIr", (uri) => runKuCommand("ir", uri)), vscode.commands.registerCommand("ku.buildCurrentFile", (uri) => runKuCommand("build", uri)), vscode.commands.registerCommand("ku.buildNativeC", (uri) => buildNativeC(uri)), vscode.commands.registerCommand("ku.packageGc", (uri) => runKuCommand("package gc", uri)), vscode.commands.registerCommand("ku.showVersion", () => showVersion()), vscode.workspace.onDidOpenTextDocument((doc) => {
        if (isKu(doc) && config(doc.uri).get("checkOnOpen", true)) {
            void scheduleCheck(doc, 0);
        }
        void refreshEditorContext(doc);
        if (doc.fileName.endsWith("ku.lock")) {
            void vscode.window.showInformationMessage("ku.lock 是生成文件，通常不建议手动编辑。");
        }
    }), vscode.workspace.onDidSaveTextDocument((doc) => {
        if (isKu(doc)) {
            invalidateDocumentDiagnostics(doc);
            if (config(doc.uri).get("checkOnSave", true) || config(doc.uri).get("checkOnChange", false)) {
                void scheduleCheck(doc, 0);
            }
        }
        void refreshEditorContext(doc);
    }), vscode.workspace.onDidChangeTextDocument((event) => {
        if (isKu(event.document)) {
            invalidateDocumentDiagnostics(event.document);
            if (!event.document.isDirty && config(event.document.uri).get("checkOnChange", false)) {
                void scheduleCheck(event.document, 500);
            }
        }
        void refreshEditorContext(event.document);
    }), vscode.workspace.onDidCloseTextDocument((doc) => {
        if (!isKu(doc))
            return;
        const key = doc.uri.toString();
        cancelRootCheck(key);
        checkGenerations.delete(key);
        replaceDiagnostics(key, []);
    }), vscode.workspace.onDidChangeConfiguration((event) => {
        if (!event.affectsConfiguration("ku"))
            return;
        resetChecks();
        for (const doc of vscode.workspace.textDocuments) {
            if (isKu(doc) && (config(doc.uri).get("checkOnOpen", true) || config(doc.uri).get("checkOnSave", true))) {
                void scheduleCheck(doc, 0);
            }
        }
        void refreshStatus();
    }), vscode.window.onDidChangeActiveTextEditor(() => {
        void refreshStatus();
        void refreshEditorContext();
    }), vscode.languages.registerCompletionItemProvider(KU_MODE, new KuCompletionProvider(), ".", "\"", "'", "/", "@"), vscode.languages.registerHoverProvider(KU_MODE, new KuHoverProvider()), vscode.languages.registerDefinitionProvider(KU_MODE, new KuDefinitionProvider()), vscode.languages.registerDocumentSymbolProvider(KU_MODE, new KuSymbolProvider()), vscode.languages.registerCodeActionsProvider(KU_MODE, new KuCodeActionProvider(), {
        providedCodeActionKinds: [vscode.CodeActionKind.QuickFix],
    }), vscode.languages.registerDocumentFormattingEditProvider(KU_MODE, new KuFormatter()));
    for (const doc of vscode.workspace.textDocuments) {
        if (isKu(doc) && config(doc.uri).get("checkOnOpen", true)) {
            void scheduleCheck(doc, 0);
        }
    }
    void refreshStatus();
    void refreshEditorContext();
}
function deactivate() {
    diagnosticsEnabled = false;
    statusGeneration++;
    resetChecks();
}
function resetChecks() {
    diagnosticSession++;
    for (const timer of checkTimers.values())
        clearTimeout(timer);
    checkTimers.clear();
    for (const request of pendingChecks.values())
        request.resolve();
    pendingChecks.clear();
    for (const { controller } of activeChecks.values())
        controller.abort();
    checkGenerations.clear();
    diagnosticsByRoot.clear();
    diagnosticCollection.clear();
}
function cancelRootCheck(key) {
    const timer = checkTimers.get(key);
    if (timer)
        clearTimeout(timer);
    checkTimers.delete(key);
    pendingChecks.get(key)?.resolve();
    pendingChecks.delete(key);
    activeChecks.get(key)?.controller.abort();
}
function invalidateDocumentDiagnostics(document) {
    // An in-flight check can import this document even before it reports a location.
    diagnosticSession++;
    for (const [root, request] of pendingChecks) {
        if (request.document.isDirty || request.document.isClosed) {
            pendingChecks.delete(root);
            request.resolve();
        }
    }
    for (const [root, { controller, request }] of activeChecks) {
        // Retain the original waiter and reveal intent until a fresh check runs.
        // Never revive a superseded/cancelled request on a later edit or save.
        if (!controller.signal.aborted && !request.document.isDirty && !request.document.isClosed
            && checkGenerations.get(root) === request.generation && !pendingChecks.has(root)) {
            pendingChecks.set(root, request);
        }
        controller.abort();
    }
    const key = document.uri.toString();
    if (document.isDirty || document.isClosed)
        cancelRootCheck(key);
    for (const [root, entries] of [...diagnosticsByRoot]) {
        if (root === key || entries.some((entry) => entry.uri.toString() === key)) {
            replaceDiagnostics(root, []);
        }
    }
    drainChecks();
}
function config(uri) {
    return vscode.workspace.getConfiguration("ku", uri ?? vscode.workspace.workspaceFolders?.[0]?.uri ?? null);
}
function isKu(document) {
    return document.languageId === "ku" && document.uri.scheme === "file";
}
async function refreshEditorContext(document = vscode.window.activeTextEditor?.document) {
    const hasMain = !!document && isKu(document) && documentHasMain(document);
    await vscode.commands.executeCommand("setContext", "ku.hasMain", hasMain);
}
function documentHasMain(document) {
    return /^\s*(?:async\s+)?fn\s+main\s*\(/m.test(document.getText());
}
async function scheduleCheck(document, delayMs) {
    if (!diagnosticsEnabled || document.isClosed || document.isDirty)
        return;
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
async function checkActiveFile(reveal, uri) {
    const document = await kuDocumentFromCommand(uri);
    if (!document || !isKu(document)) {
        void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
        return;
    }
    if (document.isDirty) {
        try {
            if (!await document.save()) {
                void vscode.window.showErrorMessage("保存 Ku 文件失败，未执行检查。");
                return;
            }
        }
        catch (error) {
            void vscode.window.showErrorMessage(`保存 Ku 文件失败，未执行检查：${String(error)}`);
            return;
        }
    }
    await runCheck(document, reveal);
}
async function runCheck(document, reveal) {
    if (!diagnosticsEnabled || document.isClosed || document.isDirty)
        return;
    const rootKey = document.uri.toString();
    cancelRootCheck(rootKey);
    const generation = (checkGenerations.get(rootKey) ?? 0) + 1;
    checkGenerations.set(rootKey, generation);
    await new Promise((resolve) => {
        pendingChecks.set(rootKey, { document, reveal, generation, resolve });
        drainChecks();
    });
}
function drainChecks() {
    if (!diagnosticsEnabled)
        return;
    for (const [key, request] of pendingChecks) {
        if (activeChecks.size >= MAX_ACTIVE_CHECKS)
            break;
        if (activeChecks.has(key))
            continue;
        pendingChecks.delete(key);
        const controller = new AbortController();
        const active = { controller, request };
        activeChecks.set(key, active);
        void runCheckJob(request, controller.signal).catch((error) => {
            if (!controller.signal.aborted && diagnosticsEnabled)
                output.appendLine(`Ku check failed: ${String(error)}`);
        }).finally(() => {
            if (activeChecks.get(key) === active)
                activeChecks.delete(key);
            if (pendingChecks.get(key) !== request)
                request.resolve();
            drainChecks();
        });
    }
}
async function runCheckJob(request, signal) {
    const { document, reveal, generation } = request;
    const rootKey = document.uri.toString();
    const documentVersion = document.version;
    const session = diagnosticSession;
    const current = () => diagnosticsEnabled && !signal.aborted && !document.isDirty && !document.isClosed && diagnosticSession === session
        && document.version === documentVersion && checkGenerations.get(rootKey) === generation;
    if (!current())
        return;
    const exe = await findKuExecutable(document.uri, reveal, signal);
    if (!current())
        return;
    if (!exe) {
        return;
    }
    let result = await execFile(exe, ["check", "--json", document.uri.fsPath], workspaceFolder(document.uri), 15000, signal);
    if (!current())
        return;
    let diagnostics = parseJsonDiagnosticEntries(result.stdout + result.stderr, document);
    let command = `${exe} check --json ${document.uri.fsPath}`;
    if (result.code !== 0 && diagnostics === undefined && /\b(?:unknown|unrecognized|unexpected|unsupported|invalid)\b[^\r\n]*--json/i.test(result.stdout + result.stderr)) {
        result = await execFile(exe, ["check", document.uri.fsPath], workspaceFolder(document.uri), 15000, signal);
        if (!current())
            return;
        diagnostics = parseTextDiagnosticEntries(result.stdout + result.stderr, document);
        command = `${exe} check ${document.uri.fsPath}`;
    }
    if (!current())
        return;
    const sources = await loadDiagnosticSources(diagnostics ?? [], document);
    diagnostics = parseJsonDiagnosticEntries(result.stdout + result.stderr, document, sources)
        ?? parseTextDiagnosticEntries(result.stdout + result.stderr, document, sources);
    if (!current()) {
        return;
    }
    diagnostics = diagnostics.filter((entry) => !vscode.workspace.textDocuments.some((doc) => doc.isDirty && doc.uri.toString() === entry.uri.toString()));
    output.clear();
    output.appendLine(`> ${command}`);
    output.append(result.stdout);
    output.append(result.stderr);
    replaceDiagnostics(rootKey, diagnostics ?? []);
    if (reveal) {
        output.show(true);
    }
}
function parseDiagnostics(text, document, sources) {
    return (parseJsonDiagnosticEntries(text, document, sources) ?? parseTextDiagnosticEntries(text, document, sources))
        .map((entry) => entry.diagnostic);
}
function parseJsonDiagnosticEntries(text, document, sources) {
    const records = [];
    for (const line of text.split(/\r?\n/)) {
        if (!line.trim()) {
            continue;
        }
        try {
            const value = JSON.parse(line);
            if (typeof value.level !== "string" ||
                typeof value.code !== "string" ||
                typeof value.message !== "string" ||
                typeof value.file !== "string" ||
                !isPositiveInteger(value.line) ||
                !isPositiveInteger(value.column) ||
                !isPositiveInteger(value.endLine) ||
                !isPositiveInteger(value.endColumn) ||
                !Array.isArray(value.notes) ||
                !value.notes.every((note) => typeof note === "string") ||
                !Array.isArray(value.helps) ||
                !value.helps.every((help) => typeof help === "string")) {
                continue;
            }
            records.push(value);
        }
        catch {
            // Older Ku executables emit human-readable diagnostics.
        }
    }
    if (records.length === 0) {
        return undefined;
    }
    return records.map((record) => {
        const uri = diagnosticUri(record.file, document);
        const startLine = Math.max(0, record.line - 1);
        const endLine = Math.max(startLine, record.endLine - 1);
        const startSource = diagnosticLine(uri, document, record.line, sources);
        const endSource = diagnosticLine(uri, document, endLine + 1, sources);
        const startColumn = startSource === undefined ? 0 : (0, diagnosticModel_1.scalarColumnToUtf16)(startSource, record.column);
        const endColumn = endSource === undefined ? 0 : (0, diagnosticModel_1.scalarColumnToUtf16)(endSource, record.endColumn);
        const details = [
            ...record.notes.map((note) => `note: ${note}`),
            ...record.helps.map((help) => `help: ${help}`),
        ];
        if (startSource === undefined || endSource === undefined)
            details.push("note: source unavailable; diagnostic column could not be mapped to UTF-16");
        const message = details.length > 0 ? `${record.message}\n${details.join("\n")}` : record.message;
        const diagnostic = new vscode.Diagnostic(new vscode.Range(startLine, startColumn, endLine, endLine === startLine ? Math.max(startColumn, endColumn) : endColumn), message, diagnosticSeverity(record.level));
        diagnostic.source = "ku check";
        diagnostic.code = record.code;
        return {
            uri,
            diagnostic,
        };
    });
}
function isPositiveInteger(value) {
    return typeof value === "number" && Number.isSafeInteger(value) && value >= 1;
}
function diagnosticSourceKey(file) {
    const absolute = path.resolve(file);
    return process.platform === "win32" ? absolute.toLowerCase() : absolute;
}
function openDiagnosticSource(document) {
    try {
        if (typeof document.positionAt === "function" && typeof document.offsetAt === "function"
            && document.offsetAt(document.positionAt(diagnosticModel_1.MAX_DIAGNOSTIC_SOURCE_BYTES + 1)) > diagnosticModel_1.MAX_DIAGNOSTIC_SOURCE_BYTES)
            return undefined;
        const text = document.getText();
        return Buffer.byteLength(text, "utf8") <= diagnosticModel_1.MAX_DIAGNOSTIC_SOURCE_BYTES ? text : undefined;
    }
    catch {
        return undefined;
    }
}
function diagnosticLine(uri, document, line, sources) {
    const key = diagnosticSourceKey(uri.fsPath);
    if (sources?.has(key)) {
        const source = sources.get(key);
        return source === undefined ? undefined : (0, diagnosticModel_1.sourceLine)(source, line);
    }
    if (key === diagnosticSourceKey(document.uri.fsPath) && typeof document.getText === "function") {
        if (typeof document.lineAt === "function") {
            try {
                const text = document.lineAt(line - 1).text;
                return text.length <= diagnosticModel_1.MAX_DIAGNOSTIC_SOURCE_BYTES ? text : undefined;
            }
            catch {
                return undefined;
            }
        }
        const source = openDiagnosticSource(document);
        if (source !== undefined)
            return (0, diagnosticModel_1.sourceLine)(source, line);
    }
    return undefined;
}
async function loadDiagnosticSources(entries, document) {
    const sources = new Map();
    const files = new Map();
    for (const entry of entries) {
        const key = diagnosticSourceKey(entry.uri.fsPath);
        if (!files.has(key) && files.size < diagnosticModel_1.MAX_DIAGNOSTIC_SOURCE_FILES)
            files.set(key, entry.uri.fsPath);
    }
    const pending = [];
    for (const [key, file] of files) {
        const opened = [document, ...(vscode.workspace.textDocuments ?? [])].find((candidate) => !candidate.isDirty && diagnosticSourceKey(candidate.uri.fsPath) === key);
        const text = opened ? openDiagnosticSource(opened) : undefined;
        if (text !== undefined)
            sources.set(key, text);
        else
            pending.push([key, file]);
    }
    for (let offset = 0; offset < pending.length; offset += 4) {
        await Promise.all(pending.slice(offset, offset + 4).map(async ([key, file]) => {
            sources.set(key, await (0, diagnosticModel_1.readDiagnosticSource)(file));
        }));
    }
    return sources;
}
function diagnosticSeverity(level) {
    switch (level.toLowerCase()) {
        case "warning":
            return vscode.DiagnosticSeverity.Warning;
        case "info":
            return vscode.DiagnosticSeverity.Information;
        case "hint":
            return vscode.DiagnosticSeverity.Hint;
        default:
            return vscode.DiagnosticSeverity.Error;
    }
}
function parseTextDiagnosticEntries(text, document, sources) {
    const diagnostics = [];
    const lines = text.split(/\r?\n/);
    for (let i = 0; i < lines.length; i++) {
        const location = /^\s*-->\s+(.+):(\d+):(\d+)\s*$/.exec(lines[i]);
        if (!location) {
            continue;
        }
        const message = cleanupMessage(lines.slice(0, i).reverse().find((line) => line.trim()) ?? "Ku check failed");
        const sourceLineNumber = Number(location[2]);
        const sourceColumn = Number(location[3]);
        if (!isPositiveInteger(sourceLineNumber) || !isPositiveInteger(sourceColumn))
            continue;
        const line = sourceLineNumber - 1;
        const uri = diagnosticUri(location[1], document);
        const source = diagnosticLine(uri, document, sourceLineNumber, sources);
        const col = source === undefined ? 0 : (0, diagnosticModel_1.scalarColumnToUtf16)(source, sourceColumn);
        let width = 1;
        const marker = lines.slice(i + 1, i + 5).find((lineText) => lineText.includes("^"));
        if (marker) {
            const first = marker.indexOf("^");
            const last = marker.lastIndexOf("^");
            if (first >= 0 && last >= first) {
                width = Math.max(1, last - first + 1);
            }
        }
        const endCol = source === undefined ? 0 : (0, diagnosticModel_1.scalarColumnToUtf16)(source, sourceColumn + width);
        const range = new vscode.Range(line, col, line, endCol);
        const fallback = source === undefined ? "\nnote: source unavailable; diagnostic column could not be mapped to UTF-16" : "";
        const diagnostic = new vscode.Diagnostic(range, `${message}${hintFor(message)}${fallback}`, vscode.DiagnosticSeverity.Error);
        diagnostic.source = "ku check";
        diagnostics.push({
            uri,
            diagnostic,
        });
    }
    return diagnostics;
}
function diagnosticUri(file, document) {
    if (!file || file === document.fileName || file === document.uri.fsPath) {
        return document.uri;
    }
    const absolute = path.isAbsolute(file)
        ? file
        : path.resolve(workspaceFolder(document.uri) ?? path.dirname(document.uri.fsPath), file);
    return vscode.Uri.file(absolute);
}
function replaceDiagnostics(rootKey, entries) {
    const affected = new Map();
    for (const entry of [...(diagnosticsByRoot.get(rootKey) ?? []), ...entries]) {
        affected.set(entry.uri.toString(), entry.uri);
    }
    if (entries.length > 0)
        diagnosticsByRoot.set(rootKey, entries);
    else
        diagnosticsByRoot.delete(rootKey);
    for (const [key, uri] of affected) {
        const unique = new Map();
        for (const contribution of diagnosticsByRoot.values()) {
            for (const entry of contribution) {
                if (entry.uri.toString() !== key)
                    continue;
                const diagnostic = entry.diagnostic;
                const identity = JSON.stringify([diagnostic.range.start.line, diagnostic.range.start.character,
                    diagnostic.range.end.line, diagnostic.range.end.character, diagnostic.severity,
                    diagnostic.code, diagnostic.message]);
                unique.set(identity, diagnostic);
            }
        }
        if (unique.size > 0)
            diagnosticCollection.set(uri, [...unique.values()]);
        else
            diagnosticCollection.delete(uri);
    }
}
function cleanupMessage(message) {
    return message.replace(/^error(?:\[[A-Z]\d+\])?:\s+/, "").replace(/^error:\s+/, "").trim();
}
function hintFor(message) {
    if (message.includes("std module 'http' must be imported")) {
        return "\nhelp: add import \"std.http\"";
    }
    if (message.includes("std module 'fs' must be imported")) {
        return "\nhelp: add import \"std.fs\"";
    }
    if (message.includes("std module 'config' must be imported")) {
        return "\nhelp: add import \"std.config\"";
    }
    if (message.includes("std module 'task' must be imported")) {
        return "\nhelp: add import \"std.task\"";
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
async function kuDocumentFromCommand(uri) {
    if (uri?.scheme === "file" && uri.fsPath.endsWith(".ku")) {
        return await vscode.workspace.openTextDocument(uri);
    }
    const editor = vscode.window.activeTextEditor;
    return editor && isKu(editor.document) ? editor.document : undefined;
}
async function runKuCommand(command, uri) {
    const document = await kuDocumentFromCommand(uri);
    if (!document || !isKu(document)) {
        void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
        return;
    }
    if (command === "run" && !documentHasMain(document)) {
        void vscode.window.showWarningMessage("当前 Ku 文件没有 fn main()，不能运行。");
        return;
    }
    if (!await saveForCommand(document))
        return;
    const exe = await findKuExecutable(document.uri);
    if (!exe) {
        return;
    }
    const cwd = terminalCwd(document.uri);
    await executeKuTask(document.uri, `Ku ${command}`, exe, [...command.split(" "), document.uri.fsPath], cwd);
}
async function buildNativeC(uri) {
    const document = await kuDocumentFromCommand(uri);
    if (!document || !isKu(document)) {
        void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
        return;
    }
    if (!await saveForCommand(document))
        return;
    const exe = await findKuExecutable(document.uri);
    if (!exe) {
        return;
    }
    const cwd = terminalCwd(document.uri);
    await executeKuTask(document.uri, "Ku Emit Native C", exe, ["build", "--native", document.uri.fsPath], cwd);
}
async function saveForCommand(document) {
    try {
        if (document.isClosed)
            return false;
        if (!document.isDirty || (await document.save() && !document.isDirty))
            return true;
        void vscode.window.showWarningMessage("文件未保存，Ku 命令已取消。");
    }
    catch (error) {
        void vscode.window.showErrorMessage(`保存失败，Ku 命令已取消：${String(error)}`);
    }
    return false;
}
async function executeKuTask(uri, name, exe, args, cwd) {
    // VS Code's task process launcher needs a workspace. A loose file can still
    // launch the CLI as the terminal executable, with the same argument array.
    if (!vscode.workspace.workspaceFolders?.length) {
        const terminal = vscode.window.createTerminal({ name, cwd, shellPath: exe, shellArgs: args });
        terminal.show();
        return;
    }
    const task = new vscode.Task({ type: "ku", command: args[0], args: [...args], file: uri.fsPath }, vscode.workspace.getWorkspaceFolder(uri) ?? vscode.TaskScope.Workspace, name, "Ku", new vscode.ProcessExecution(exe, args, { cwd }), []);
    task.presentationOptions = { reveal: vscode.TaskRevealKind.Always, focus: true, panel: vscode.TaskPanelKind.Dedicated };
    try {
        await vscode.tasks.executeTask(task);
    }
    catch (error) {
        void vscode.window.showErrorMessage(`无法启动 Ku 命令：${String(error)}`);
    }
}
async function showVersion() {
    const exe = await findKuExecutable(vscode.window.activeTextEditor?.document.uri);
    if (!exe) {
        return;
    }
    const result = await execFile(exe, ["version"], workspaceFolder(vscode.window.activeTextEditor?.document.uri));
    if (result.code !== 0) {
        void vscode.window.showErrorMessage(`无法读取 Ku 版本：${result.stderr}`);
        return;
    }
    const version = (result.stdout || result.stderr).trim();
    void vscode.window.showInformationMessage(`${version} | plugin ${KU_VERSION}`);
    await refreshStatus();
}
let statusGeneration = 0;
async function refreshStatus() {
    const generation = ++statusGeneration;
    const uri = vscode.window.activeTextEditor?.document.uri;
    const exe = await findKuExecutable(uri, false);
    if (generation !== statusGeneration)
        return;
    if (!exe) {
        setStatus("Ku: executable missing", true);
        return;
    }
    const result = await execFile(exe, ["version"], workspaceFolder(uri));
    if (generation !== statusGeneration)
        return;
    const actual = /ku\s+([0-9]+\.[0-9]+\.[0-9]+)/.exec(result.stdout + result.stderr)?.[1] ?? "unknown";
    setStatus(actual === KU_VERSION ? `Ku ${actual}` : `Ku ${actual} / plugin ${KU_VERSION}`, actual !== KU_VERSION);
}
function setStatus(text, warn) {
    status.text = warn ? `$(warning) ${text}` : `$(check) ${text}`;
    status.tooltip = "Ku interpreter and extension version";
    status.show();
}
async function findKuExecutable(uri, notify = true, signal) {
    if (signal?.aborted || vscode.workspace.isTrusted === false)
        return undefined;
    const configured = config(uri).get("executablePath", "").trim();
    const candidates = [];
    const folder = workspaceFolder(uri);
    if (configured) {
        if (configured.includes("${workspaceFolder}") && !folder) {
            if (notify)
                void vscode.window.showErrorMessage("ku.executablePath 使用了 ${workspaceFolder}，但当前文件不在工作区中。");
            return undefined;
        }
        let expanded = configured.replace(/\$\{workspaceFolder\}/g, () => folder ?? "").replace(/\$\{userHome\}/g, () => os.homedir());
        if (/^~[/\\]/.test(expanded))
            expanded = path.join(os.homedir(), expanded.slice(2));
        if (!path.isAbsolute(expanded) && /[/\\]/.test(expanded)) {
            expanded = path.resolve(folder ?? (uri ? path.dirname(uri.fsPath) : process.cwd()), expanded);
        }
        candidates.push(expanded);
    }
    else {
        candidates.push("ku");
        if (folder) {
            const releaseTarget = hostReleaseTarget();
            if (releaseTarget)
                candidates.push(path.join(folder, "release", releaseTarget, exeName()));
            candidates.push(path.join(folder, "target", "release", exeName()), path.join(folder, "target", "debug", exeName()));
        }
    }
    const found = await (0, executableModel_1.firstWorkingExecutable)(candidates, async (candidate) => {
        if (signal?.aborted)
            return { code: 1 };
        const result = await execFile(candidate, ["version"], folder, 3000, signal);
        return { code: result.code === 0 && /\bku\s+\d+\.\d+\.\d+\b/i.test(result.stdout + result.stderr) ? 0 : 1 };
    });
    if (signal?.aborted)
        return undefined;
    if (found) {
        return found;
    }
    if (notify) {
        void vscode.window.showErrorMessage(configured
            ? `ku.executablePath 无法执行 Ku version：${configured}。请填写可执行文件路径，不附带命令参数或 shell 引号。`
            : "找不到 Ku CLI。请安装 CLI 并设置 ku.executablePath，或确认 VS Code 进程的 PATH 包含 ku。");
    }
    return undefined;
}
function exeName() {
    return process.platform === "win32" ? "ku.exe" : "ku";
}
function hostReleaseTarget() {
    if (process.platform === "win32" && process.arch === "x64") {
        return "x86_64-pc-windows-msvc";
    }
    if (process.platform === "linux" && process.arch === "x64") {
        return "x86_64-unknown-linux-gnu";
    }
    if (process.platform === "darwin" && process.arch === "arm64") {
        return "aarch64-apple-darwin";
    }
    return undefined;
}
function workspaceFolder(uri) {
    const folder = uri ? vscode.workspace.getWorkspaceFolder(uri) : vscode.workspace.workspaceFolders?.[0];
    return folder?.uri.fsPath;
}
function terminalCwd(uri) {
    return workspaceFolder(uri) ?? path.dirname(uri.fsPath);
}
function execFile(file, args, cwd, timeoutMs = 15000, signal) {
    if (signal?.aborted)
        return Promise.resolve({ code: 1, stdout: "", stderr: "Ku command cancelled.\n" });
    return new Promise((resolve) => {
        let closed = false;
        let result;
        const finish = () => { if (closed && result)
            resolve(result); };
        const child = cp.execFile(file, args, { cwd, timeout: timeoutMs, windowsHide: true, signal, killSignal: "SIGKILL" }, (error, stdout, stderr) => {
            const code = (0, executableModel_1.execFileExitCode)(error);
            let message = stderr.toString();
            // Compiler diagnostics already describe ordinary nonzero exits. Spawn,
            // timeout and buffer errors otherwise have no useful stderr to display.
            if (error && (typeof error.code !== "number" || error.killed)) {
                message += `${message && !message.endsWith("\n") ? "\n" : ""}${error.killed && !signal?.aborted
                    ? `Ku command exceeded ${timeoutMs} ms or was terminated: ` : ""}${error.message}\n`;
            }
            result = { code, stdout: stdout.toString(), stderr: message };
            finish();
        });
        // AbortError may call back before the process exits. Retain its queue slot
        // until close so cancelling a check cannot exceed the process limit.
        child.once("close", () => { closed = true; finish(); });
    });
}
class KuCompletionProvider {
    async provideCompletionItems(document, position) {
        const linePrefix = document.lineAt(position).text.slice(0, position.character);
        const items = [];
        const importContext = (0, imports_1.importPathContext)(linePrefix);
        if (importContext && !importContext.closed && completionIsCode(document, new vscode.Position(position.line, importContext.start - 1))) {
            return await importPathCompletions(document, position, linePrefix);
        }
        if (!completionIsCode(document, position))
            return [];
        if (isNamedImportContext(document, position)) {
            return exportNameCompletions(document, position);
        }
        const member = memberAccessContext(linePrefix, position);
        if (member) {
            const labels = (0, completionModel_1.memberCompletionLabels)(member.receiver, member.prefix);
            if (labels.length > 0) {
                return labels.map((label) => memberCompletionItem(label, member.receiver, member.range));
            }
            return [];
        }
        if (/\berr\.$/.test(linePrefix)) {
            return fieldCompletions(["domain", "code", "message"], "Error field");
        }
        if (/\bresponse\.$/.test(linePrefix)) {
            return fieldCompletions(["status", "headers", "body"], "HttpResponse field");
        }
        if (/["'`][^"'`]*\.$/.test(linePrefix) || /\btext\.$/.test(linePrefix)) {
            return methodCompletions(["trim", "lower", "upper", "len", "slice"]);
        }
        for (const value of completionModel_1.keywords) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.Keyword));
        }
        for (const value of completionModel_1.types) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.TypeParameter));
        }
        for (const value of completionModel_1.builtins) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.Function));
        }
        for (const value of completionModel_1.stdModules) {
            const item = new vscode.CompletionItem(value, vscode.CompletionItemKind.Module);
            item.insertText = `import "${value}"`;
            items.push(item);
        }
        for (const value of completionModel_1.stdFunctions) {
            items.push(new vscode.CompletionItem(value, vscode.CompletionItemKind.Function));
        }
        return items;
    }
}
function memberAccessContext(linePrefix, position) {
    const match = /\b([A-Za-z_][A-Za-z0-9_]*)\.([A-Za-z_][A-Za-z0-9_]*)?$/.exec(linePrefix);
    if (!match) {
        return undefined;
    }
    const receiver = match[1];
    const prefix = match[2] ?? "";
    const start = position.character - prefix.length;
    return {
        receiver,
        prefix,
        range: new vscode.Range(position.line, start, position.line, position.character),
    };
}
function completionIsCode(document, position) {
    const source = document.getText(new vscode.Range(new vscode.Position(0, 0), position));
    let state = "code";
    for (let i = 0; i < source.length; i++) {
        const ch = source[i];
        if (state === "line") {
            if (ch === "\n")
                state = "code";
        }
        else if (state === "block") {
            if (ch === "*" && source[i + 1] === "/") {
                state = "code";
                i++;
            }
        }
        else if (state !== "code") {
            if (ch === "\\")
                i++;
            else if (ch === state)
                state = "code";
        }
        else if (ch === "/" && source[i + 1] === "/") {
            state = "line";
            i++;
        }
        else if (ch === "/" && source[i + 1] === "*") {
            state = "block";
            i++;
        }
        else if (ch === "\"" || ch === "'" || ch === "`")
            state = ch;
    }
    return state === "code";
}
async function importPathCompletions(document, position, linePrefix) {
    const context = (0, imports_1.importPathContext)(linePrefix);
    if (!context || context.closed)
        return [];
    const decoded = (0, imports_1.decodeImportPath)(context.raw, context.quote);
    if (decoded === undefined)
        return [];
    const current = process.platform === "win32" ? decoded.replace(/\\/g, "/") : decoded;
    const replaceRange = new vscode.Range(position.line, context.start, position.line, position.character);
    if (current.startsWith("std.")) {
        return (0, completionModel_1.stdImportPathLabels)(current).map((module) => importPathItem(module, replaceRange, vscode.CompletionItemKind.Module));
    }
    if (current.startsWith("@")) {
        return dependencyCompletions(document, current, replaceRange);
    }
    const items = [];
    if ("std".startsWith(current)) {
        items.push(importPathItem("std", replaceRange, vscode.CompletionItemKind.Module), ...(0, completionModel_1.stdImportPathLabels)("std.").map((module) => importPathItem(module, replaceRange, vscode.CompletionItemKind.Module)));
    }
    if (!current)
        items.push(...dependencyCompletions(document, current, replaceRange));
    const slash = current.lastIndexOf("/");
    const directory = current.slice(0, slash + 1);
    const prefix = current.slice(slash + 1).replace(/\.ku$/, "");
    const dir = path.resolve(path.dirname(document.uri.fsPath), directory || ".");
    try {
        for (const entry of await fs.promises.readdir(dir, { withFileTypes: true })) {
            const label = entry.isDirectory() ? `${entry.name}/` : entry.name.replace(/\.ku$/, "");
            const startsWith = process.platform === "win32" ? label.toLowerCase().startsWith(prefix.toLowerCase()) : label.startsWith(prefix);
            if (!startsWith || (!entry.isDirectory() && !entry.name.endsWith(".ku")))
                continue;
            const item = new vscode.CompletionItem(label, entry.isDirectory() ? vscode.CompletionItemKind.Folder : vscode.CompletionItemKind.File);
            const inserted = `${directory}${label}`.replace(/\\/g, "\\\\").replace(new RegExp(context.quote, "g"), `\\${context.quote}`);
            item.range = replaceRange;
            item.insertText = inserted;
            item.filterText = inserted;
            items.push(item);
        }
    }
    catch {
        // A directory may disappear, or lack permission, while the completion request is running.
    }
    return items;
}
function memberCompletionItem(label, receiver, range) {
    const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Method);
    if (receiver === "http" && (label === "service" || label === "server")) {
        item.insertText = new vscode.SnippetString(`${label}($0)`);
    }
    else {
        item.insertText = label;
    }
    item.range = range;
    item.detail = `${receiver} member`;
    return item;
}
function importPathItem(label, range, kind) {
    const item = new vscode.CompletionItem(label, kind);
    item.insertText = label;
    item.range = range;
    return item;
}
function dependencyCompletions(document, current, range) {
    try {
        return [...new Set(readKuModDependencies(document))].map((dep) => `@${dep}/`)
            .filter((dep) => dep.startsWith(current))
            .map((dep) => {
            const item = importPathItem(dep, range, vscode.CompletionItemKind.Module);
            item.detail = "ku.mod dependency prefix; dependency-file completion is not available";
            return item;
        });
    }
    catch {
        return [];
    }
}
function isNamedImportContext(document, position) {
    const before = document.getText(new vscode.Range(new vscode.Position(position.line, 0), position));
    return /^\s*import\s+\{[^}]*$/.test(before);
}
function exportNameCompletions(document, position) {
    const line = document.lineAt(position).text;
    const context = (0, imports_1.importPathContext)(line);
    const importPath = context?.closed ? (0, imports_1.decodeImportPath)(context.raw, context.quote) : undefined;
    if (!importPath) {
        return [];
    }
    if (importPath === "std") {
        return completionModel_1.stdRootModules.map((name) => new vscode.CompletionItem(name, vscode.CompletionItemKind.Module));
    }
    const uri = (0, imports_1.resolveImportUri)(document, importPath);
    if (!uri || !fs.existsSync(uri.fsPath)) {
        return [];
    }
    try {
        return exportedNames(fs.readFileSync(uri.fsPath, "utf8")).map((name) => new vscode.CompletionItem(name, vscode.CompletionItemKind.Reference));
    }
    catch {
        return [];
    }
}
function exportedNames(source) {
    const names = new Set();
    for (const match of source.matchAll(/^\s*(?:(?:async\s+)?fn|struct|enum)\s+([A-Z][A-Za-z0-9_]*)/gm)) {
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
function dottedHoverKey(document, range) {
    const line = document.lineAt(range.start.line).text;
    const word = document.getText(range);
    const before = line.slice(0, range.start.character);
    const after = line.slice(range.end.character);
    const receiver = /([A-Za-z_][A-Za-z0-9_]*)\.$/.exec(before)?.[1];
    if (receiver) {
        return `${receiver}.${word}`;
    }
    const member = /^\s*\.([A-Za-z_][A-Za-z0-9_]*)/.exec(after)?.[1];
    if (member) {
        return `${word}.${member}`;
    }
    return undefined;
}
class KuHoverProvider {
    provideHover(document, position) {
        const range = document.getWordRangeAtPosition(position);
        if (!range) {
            return undefined;
        }
        const word = document.getText(range);
        const dotted = dottedHoverKey(document, range);
        const docs = {
            "async": "`async fn` 调用会立即启动一次性 task 句柄，并且必须显式返回 `T!`。",
            "await": "`await task?` 等价于 `(await task)?`；await 会消费 task，普通 task 只能 await 一次。",
            "catch": "`catch (err)` 中 `err` 是结构化 Error 对象：`err.domain`、`err.code`、`err.message`。",
            "err": "`err(message)` 返回 `Unknown!`，失败 payload 是 `{ domain, code, message }`。",
            "fail": "`fail` 主动返回可恢复错误；字符串会包装为 `{ domain: \"ku\", code: \"fail\", message }`。",
            "http": "`import \"std.http\"` 后使用。`http.get/post/request` 返回 `{ status, headers, body }!`；普通 route 不读请求写 `fn()`，读请求写 `fn(req)`，并返回 `http.text/html/json/empty/redirect(...)`。",
            "service": "`http.service()` 返回带默认资源限制的 HTTP service 配置对象；普通 handler 接受 `fn()` 或 `fn(req)`，不接受 `fn(req, res)`。",
            "status": "`http.status` 是 HTTP 协议状态码常量对象，例如 `http.status.ok`、`http.status.created`、`http.status.notFound`。",
            "statusText": "`http.statusText(code)` 把 HTTP 状态码转成标准原因短语，例如 404 -> \"Not Found\"。",
            "server": "`http.server()` 返回带默认 timeout/body/header/concurrency 限制的 server 配置对象。",
            "fs": "`import \"std.fs\"` 后使用。支持 `fs.read/write/try_read/try_write/exists/read_dir`。",
            "fs.exists": "`fs.exists(path)` 返回路径是否存在。",
            "fs.read_dir": "`fs.read_dir(path)?` 返回排序后的目录条目路径数组。",
            "bytes": "`import bytes from \"std.bytes\"` 后使用。`bytes.from_str(text)?` 和 `bytes.from_array(values)?` 创建拥有所有权的二进制数据；对象提供 len/get/to_str/clone。当前通过 native C 路径执行。",
            "net": "`import net from \"std.net\"` 后使用。`net.client(config)?` 创建 TCP 客户端；对象提供 read/write/close，配置可启用 TLS。当前通过 native C 路径执行。",
            "lexer": "`lexer.scan(text)` 返回词法单元字符串数组。",
            "parser": "`parser.parse(textOrTokens)` 对源码返回解析调试文本；token 字符串数组只返回数量摘要。",
            "config": "`import \"std.config\"` 后使用。支持 `config.env/env_file/yaml`。",
            "task": "`task` 不是关键字。业务 task 来自 async fn 调用并只能 await；`std.task` 只提供 stats/stress 诊断压测函数。",
            "object": "`object.get_or(obj, key, default)` 和 `obj.get_or(key, default)` 是显式宽松读取；普通对象缺键仍报错。",
            "object.get_or": "`object.get_or(obj, key, default)` 缺键返回 default，存在则返回字段值；default 参数会立即求值。",
            "get_or": "`obj.get_or(key, default)` 缺键返回 default；default 参数会立即求值，不是惰性计算。",
            "time": "`time.now()` 返回 Unix 毫秒整数；`time.instant()` 返回 Time 对象。测量时长可使用进程内单调时钟 `time.steady_millis()`。",
            "time.now": "`time.now()` 返回当前 Unix 毫秒整数，只接受零个参数。",
            "time.instant": "`time.instant()` 返回 `{ kind: \"time.time\", millis }` Time 对象，供日期与格式化 API 使用。",
            "time.elapsed": "`time.elapsed(previous)` 返回 Time 到当前墙上时间的毫秒差；测量耗时优先使用 `time.steady_millis()` 的差值。",
            "time.steady_millis": "`time.steady_millis()` 返回进程内单调时钟的毫秒整数，不能作为 Unix 时间戳使用。",
            "time.millis": "`time.millis()` 返回当前 Unix 毫秒；`time.millis(timeOrDuration)` 读取 Time/Duration 的毫秒值。",
            "time.unix": "`time.unix()` 返回当前 Unix 秒；`time.unix(time)` 读取 Time 的 Unix 秒。",
            "time.date": "`time.date()` 直接返回今天日期；`time.date(time)?` 按本地时区提取日期，带参数的重载均返回 Result：`time.date(time[, zone])?`、`time.date(year, month, day)?`。",
            "time.datetime": "`time.datetime(year, month, day, hour, minute, second[, zone])?` 构造 Time。",
            "time.duration": "`time.duration(ms)?` 或 `time.duration(value, unit)?` 构造 Duration；unit 支持 ms/s/m/h/d。",
            "time.format": "`time.format(time, layout[, zone])?` 使用 yyyy/MM/dd/HH/mm/ss/SSS token 格式化。",
            "time.parse": "`time.parse(text[, layout[, zone]])?` 解析 Time。",
            "time.add": "`time.add(time, duration)` 返回加上 Duration 后的 Time。",
            "time.sub": "`time.sub(time, duration)` 返回减去 Duration 后的 Time。",
            "time.diff": "`time.diff(later, earlier)` 返回 Duration。",
            "time.compare": "`time.compare(a, b)` 返回 -1、0 或 1。",
            "time.parts": "`time.parts(time[, zone])?` 返回 year/month/day/hour/minute/second/millis 等字段。",
            "time.weekday": "`time.weekday(date)` 返回 1..7，1 表示周一。",
            "time.is_leap": "`time.is_leap(year)` 判断闰年。",
            "time.days_in_month": "`time.days_in_month(year, month)?` 返回月份天数。",
            "time.sleep": "`time.sleep(msOrDuration)?` 阻塞当前任务；async 中会进入 blocking pool。",
            "match": "Ku 0.0.15 保留 `match`，不再支持 `switch`。",
            "try_get": "`values.try_get(index)?` 越界时返回结构化 Error。",
            "trim": "`text.trim()` 是 string 实例方法。",
        };
        const text = (dotted && docs[dotted]) || docs[word];
        return text ? new vscode.Hover(new vscode.MarkdownString(text)) : undefined;
    }
}
class KuDefinitionProvider {
    provideDefinition(document, position) {
        const importDef = importDefinition(document, position);
        if (importDef) {
            return importDef;
        }
        const range = document.getWordRangeAtPosition(position);
        if (!range) {
            return undefined;
        }
        const word = document.getText(range);
        const sameFile = findDefinitionInDocument(document, word);
        if (sameFile) {
            return sameFile;
        }
        for (const imp of (0, imports_1.parseImports)(document)) {
            const uri = (0, imports_1.resolveImportUri)(document, imp.path);
            if (!uri || !fs.existsSync(uri.fsPath)) {
                continue;
            }
            const importedName = imp.kind === "named" ? imp.names.find((name) => name.local === word)?.source : word;
            if (!importedName)
                continue;
            if (imp.module === word)
                return new vscode.Location(uri, new vscode.Position(0, 0));
            try {
                const source = fs.readFileSync(uri.fsPath, "utf8");
                const target = findDefinitionInText(uri, source, importedName);
                if (target)
                    return target;
            }
            catch {
                // The imported file may be removed or become unreadable after resolution.
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
        new RegExp(`^\\s*(?:async\\s+)?fn\\s+${escaped}\\b`, "m"),
        new RegExp(`^\\s*struct\\s+${escaped}\\b`, "m"),
        new RegExp(`^\\s*enum\\s+${escaped}\\b`, "m"),
        new RegExp(`^\\s*${escaped}\\s*=`, "m"),
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
            const match = /^\s*(?:async\s+)?(module|fn|struct|enum)\s+([A-Za-z_][A-Za-z0-9_]*)/.exec(text);
            if (!match) {
                continue;
            }
            const kind = match[1] === "fn" ? vscode.SymbolKind.Function : match[1] === "struct" ? vscode.SymbolKind.Struct : match[1] === "enum" ? vscode.SymbolKind.Enum : vscode.SymbolKind.Module;
            const range = new vscode.Range(line, 0, line, text.length);
            const symbol = new vscode.DocumentSymbol(match[2], match[1], kind, range, range);
            if (/^\s+(?:async\s+)?fn\b/.test(text) && stack.length > 0) {
                stack[stack.length - 1].children.push(symbol);
            }
            else {
                symbols.push(symbol);
            }
            if (match[1] !== "fn" || !/^\s+(?:async\s+)?fn\b/.test(text)) {
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
            if (diagnostic.message.includes("std module 'config' must be imported")) {
                actions.push(insertImportAction(document, "std.config"));
            }
            if (diagnostic.message.includes("std module 'task' must be imported")) {
                actions.push(insertImportAction(document, "std.task"));
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
    provideDocumentFormattingEdits(document, options) {
        const text = document.getText();
        const formatted = (0, formatter_1.formatKu)(text, options);
        if (formatted === text) {
            return [];
        }
        const full = new vscode.Range(document.positionAt(0), document.positionAt(text.length));
        return [vscode.TextEdit.replace(full, formatted)];
    }
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
