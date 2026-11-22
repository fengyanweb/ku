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
const semanticModel_1 = require("./semanticModel");
const metadataModel_1 = require("./metadataModel");
const packageModel_1 = require("./packageModel");
const formatter_1 = require("./formatter");
const toolTerminal_1 = require("./toolTerminal");
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
    context.subscriptions.push(vscode.commands.registerCommand("ku.runCurrentFile", (uri) => runKuCommand("run", uri)), vscode.commands.registerCommand("ku.checkCurrentFile", (uri) => checkActiveFile(true, uri)), vscode.commands.registerCommand("ku.showIr", (uri) => runKuCommand("ir", uri)), vscode.commands.registerCommand("ku.buildCurrentFile", (uri, outputUri) => runKuCommand("build", uri, outputUri)), vscode.commands.registerCommand("ku.buildNativeC", (uri) => buildNativeC(uri)), vscode.commands.registerCommand("ku.packageGc", (uri) => runKuCommand("package gc", uri)), vscode.commands.registerCommand("ku.showVersion", () => showVersion()), vscode.commands.registerCommand("ku.selectExecutable", (uri) => selectKuExecutable(uri)), vscode.commands.registerCommand("ku.installDependencies", (uri, source) => installDependencies(uri, source)), vscode.commands.registerCommand("ku.buildWithOptions", (uri) => buildWithOptions(uri)), vscode.workspace.onDidOpenTextDocument((doc) => {
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
    }), vscode.workspace.onDidGrantWorkspaceTrust(() => {
        for (const doc of vscode.workspace.textDocuments) {
            if (isKu(doc) && config(doc.uri).get("checkOnOpen", true))
                void scheduleCheck(doc, 0);
        }
        void refreshStatus();
    }), vscode.window.onDidChangeActiveTextEditor(() => {
        void refreshStatus();
        void refreshEditorContext();
    }), vscode.languages.registerCompletionItemProvider(KU_MODE, new KuCompletionProvider(), ".", "\"", "'", "/", "@"), vscode.languages.registerHoverProvider(KU_MODE, new KuHoverProvider()), vscode.languages.registerSignatureHelpProvider(KU_MODE, new KuSignatureProvider(), "(", ","), vscode.languages.registerDefinitionProvider(KU_MODE, new KuDefinitionProvider()), vscode.languages.registerDocumentSymbolProvider(KU_MODE, new KuSymbolProvider()), vscode.languages.registerCodeActionsProvider(KU_MODE, new KuCodeActionProvider(), {
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
    (0, toolTerminal_1.closeKuTools)();
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
    let resource = uri ?? vscode.workspace.workspaceFolders?.[0]?.uri;
    const folder = resource && containingWorkspace(resource);
    if (resource && folder && !vscode.workspace.getWorkspaceFolder(resource)) {
        resource = vscode.Uri.file(path.join(folder.uri.fsPath, path.relative(fileIdentity(folder.uri.fsPath), fileIdentity(resource.fsPath))));
    }
    return vscode.workspace.getConfiguration("ku", resource ?? null);
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
    if (!allowTools(reveal))
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
    let result = await execFile(exe, ["check", "--json", document.uri.fsPath], terminalCwd(document.uri), 15000, signal);
    if (!current())
        return;
    let diagnostics = parseJsonDiagnosticEntries(result.stdout + result.stderr, document);
    let command = `${exe} check --json ${document.uri.fsPath}`;
    if (result.code !== 0 && diagnostics === undefined && /\b(?:unknown|unrecognized|unexpected|unsupported|invalid)\b[^\r\n]*--json/i.test(result.stdout + result.stderr)) {
        result = await execFile(exe, ["check", document.uri.fsPath], terminalCwd(document.uri), 15000, signal);
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
    output.appendLine(`cwd: ${terminalCwd(document.uri)}`);
    output.appendLine(`> ${command}`);
    output.append((0, toolTerminal_1.redactToolOutput)(result.stdout));
    output.append((0, toolTerminal_1.redactToolOutput)(result.stderr));
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
function fileIdentity(file) {
    try {
        return diagnosticSourceKey(fs.realpathSync.native(path.resolve(file)));
    }
    catch {
        return diagnosticSourceKey(file);
    }
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
        : path.resolve(terminalCwd(document.uri), file);
    const key = fileIdentity(absolute);
    if (key === fileIdentity(document.uri.fsPath))
        return document.uri;
    return (vscode.workspace.textDocuments ?? []).find((opened) => fileIdentity(opened.uri.fsPath) === key)?.uri
        ?? vscode.Uri.file(absolute);
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
async function runKuCommand(command, uri, outputUri) {
    if (!allowTools())
        return;
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
    const args = command === "build" ? await buildArguments(document, ["build"], outputUri) : [...command.split(" "), document.uri.fsPath];
    if (args && await saveForCommand(document))
        await executeKuTask(document.uri, `Ku ${command}`, exe, args, cwd);
}
async function buildArguments(document, args, requestedOutput) {
    const project = (0, packageModel_1.discoverProject)(document.uri.fsPath).project;
    const entry = project && path.resolve(project.importRoot, project.fields.get("main") ?? "main.ku");
    const needsOutput = entry && fileIdentity(entry) !== fileIdentity(document.uri.fsPath);
    if (needsOutput || requestedOutput) {
        const selected = requestedOutput ?? await vscode.window.showSaveDialog({
            title: "当前文件不是 ku.mod 的 main，请选择构建输出", saveLabel: "构建到此文件",
            defaultUri: vscode.Uri.file(path.join(project.directory, path.basename(document.uri.fsPath, ".ku") + (process.platform === "win32" ? ".exe" : ""))),
        });
        if (!selected)
            return undefined;
        if (selected.scheme !== "file" || !selected.fsPath) {
            void vscode.window.showErrorMessage("构建输出必须是本地文件路径。");
            return undefined;
        }
        args.push("-o", selected.fsPath);
    }
    args.push(document.uri.fsPath);
    return args;
}
async function buildNativeC(uri) {
    if (!allowTools())
        return;
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
    if (!await saveForCommand(document))
        return;
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
    if (!allowTools())
        return;
    output.appendLine(`\n> ${(0, toolTerminal_1.redactToolOutput)([exe, ...args].map((arg) => JSON.stringify(arg)).join(" "))}\ncwd: ${cwd}`);
    const interactive = args[0] === "run";
    // VS Code's task process launcher needs a workspace. A loose file can still
    // launch the CLI as the terminal executable, with the same argument array.
    if (!vscode.workspace.workspaceFolders?.length) {
        const terminal = interactive
            ? vscode.window.createTerminal({ name, cwd, shellPath: exe, shellArgs: args })
            : vscode.window.createTerminal({ name, pty: new toolTerminal_1.KuToolTerminal(exe, args, cwd, (text) => output.append(text)) });
        terminal.show();
        return;
    }
    const task = new vscode.Task({ type: "ku", command: args[0], args: [...args], file: uri.fsPath }, containingWorkspace(uri) ?? vscode.TaskScope.Workspace, name, "Ku", interactive ? new vscode.ProcessExecution(exe, args, { cwd })
        : new vscode.CustomExecution(async () => new toolTerminal_1.KuToolTerminal(exe, args, cwd, (text) => output.append(text))), []);
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
    output.appendLine(`CLI: ${exe}\n${version}\nExtension: ${KU_VERSION}`);
    const action = await vscode.window.showInformationMessage(`${version} · 插件 ${KU_VERSION}\n${exe}`, "选择可执行文件", "查看输出");
    if (action === "选择可执行文件")
        await selectKuExecutable(vscode.window.activeTextEditor?.document.uri);
    if (action === "查看输出")
        output.show(true);
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
        setStatus(vscode.workspace.isTrusted === false ? "Ku: 静态编辑（受限模式）" : "Ku: 选择可执行文件", true);
        status.command = vscode.workspace.isTrusted === false ? "workbench.trust.manage" : "ku.selectExecutable";
        return;
    }
    const result = await execFile(exe, ["version"], workspaceFolder(uri));
    if (generation !== statusGeneration)
        return;
    const actual = /ku\s+([0-9]+\.[0-9]+\.[0-9]+)/.exec(result.stdout + result.stderr)?.[1] ?? "unknown";
    setStatus(`Ku ${actual} · 插件 ${KU_VERSION}`, result.code !== 0 || actual === "unknown");
    status.command = "ku.showVersion";
    status.tooltip = `${exe}\nCLI ${actual} · Extension ${KU_VERSION}`;
}
function setStatus(text, warn) {
    status.text = warn ? `$(warning) ${text}` : `$(check) ${text}`;
    status.tooltip = "Ku interpreter and extension version";
    status.show();
}
function allowTools(notify = true) {
    if (vscode.workspace.isTrusted !== false)
        return true;
    if (notify)
        void vscode.window.showInformationMessage("当前为受限模式，可静态浏览和编辑；信任工作区后才能运行 Ku 工具。");
    return false;
}
function executableOnPath(candidate) {
    if (path.isAbsolute(candidate) || /[/\\]/.test(candidate))
        return path.resolve(candidate);
    const extensions = process.platform === "win32" ? (process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";") : [""];
    for (const directory of (process.env.PATH ?? "").split(path.delimiter).filter(Boolean)) {
        for (const extension of ["", ...extensions]) {
            const file = path.join(directory, candidate + extension);
            try {
                if (!fs.statSync(file).isFile())
                    continue;
                // execFile/ProcessExecution require a native executable on Windows.
                if (process.platform === "win32" && !/\.exe$/i.test(file))
                    continue;
                fs.accessSync(file, process.platform === "win32" ? fs.constants.F_OK : fs.constants.X_OK);
                return file;
            }
            catch { /* Try the next PATH entry without changing the user's PATH. */ }
        }
    }
    return undefined;
}
async function selectKuExecutable(uri) {
    if (!allowTools())
        return undefined;
    const resource = uri ?? vscode.window.activeTextEditor?.document.uri ?? vscode.workspace.workspaceFolders?.[0]?.uri;
    const selection = await vscode.window.showOpenDialog({
        title: "选择 Ku 可执行文件", openLabel: "使用此 Ku CLI", canSelectFiles: true,
        canSelectFolders: false, canSelectMany: false,
        ...(process.platform === "win32" ? { filters: { "Ku executable": ["exe"] } } : {}),
    });
    const file = selection?.[0]?.fsPath;
    if (!file)
        return undefined;
    const result = await execFile(file, ["version"], resource ? terminalCwd(resource) : undefined, 3000);
    if (result.code !== 0 || !/\bku\s+\d+\.\d+\.\d+\b/i.test(result.stdout + result.stderr)) {
        output.appendLine(`CLI: ${file}\n${(0, toolTerminal_1.redactToolOutput)(result.stdout + result.stderr)}`);
        void vscode.window.showErrorMessage("所选文件没有成功返回 Ku 版本，原设置已保留。详情见 Ku 输出。");
        return undefined;
    }
    const folder = resource && containingWorkspace(resource);
    await config(resource).update("executablePath", file, folder ? vscode.ConfigurationTarget.WorkspaceFolder : vscode.ConfigurationTarget.Global);
    output.appendLine(`CLI: ${file}\n${result.stdout.trim()}`);
    void refreshStatus();
    return file;
}
async function installDependencies(uri, providedSource) {
    if (!allowTools())
        return;
    const resource = uri ?? vscode.window.activeTextEditor?.document.uri ?? vscode.workspace.workspaceFolders?.[0]?.uri;
    const cwd = resource && projectDirectory(resource);
    if (!resource || !cwd) {
        void vscode.window.showErrorMessage("当前文件不属于 Ku 项目。请先在项目目录执行 ku init，创建 ku.mod。");
        return;
    }
    if (!await saveProjectMetadata(cwd))
        return;
    const exe = await findKuExecutable(resource);
    if (!exe)
        return;
    const help = await execFile(exe, ["help", "i"], cwd, 3000);
    if (help.code !== 0 || !/\bku (?:i|package install)\b/.test(help.stdout)) {
        output.appendLine(`CLI: ${exe}\n${(0, toolTerminal_1.redactToolOutput)(help.stdout + help.stderr)}`);
        void vscode.window.showErrorMessage(`此 CLI 不支持 ku i：${exe}。请选择支持 package install 的 Ku，再显式安装依赖。`);
        return;
    }
    let source = providedSource;
    if (source === undefined) {
        const choice = await vscode.window.showQuickPick(["恢复清单和锁定依赖", "输入包名、Git 或 PyPI 来源", "选择本地目录或文件"], { title: "Ku：安装依赖" });
        if (!choice)
            return;
        if (choice === "输入包名、Git 或 PyPI 来源") {
            source = await vscode.window.showInputBox({ title: "依赖来源", prompt: "例如 ./vendor/library、HTTPS Git URL、pypi:DISTRIBUTION；按所选 CLI 的 ku help i 规则解析" });
            if (!source?.trim())
                return;
        }
        else if (choice === "选择本地目录或文件") {
            const selection = await vscode.window.showOpenDialog({ title: "选择本地 Ku 包或 Python 文件", canSelectFiles: true, canSelectFolders: true, canSelectMany: false });
            source = selection?.[0]?.fsPath;
            if (!source)
                return;
        }
    }
    if (source?.trim().startsWith("-")) {
        void vscode.window.showErrorMessage("请填写依赖来源，不附带 CLI 选项。");
        return;
    }
    if (await saveProjectMetadata(cwd))
        await executeKuTask(resource, "Ku Install Dependencies", exe, ["i", ...(source?.trim() ? [source.trim()] : [])], cwd);
}
async function saveProjectMetadata(cwd) {
    for (const document of vscode.workspace.textDocuments) {
        if (document.uri.scheme === "file" && ["ku.mod", "ku.lock"].some((name) => fileIdentity(document.uri.fsPath) === fileIdentity(path.join(cwd, name)))) {
            if (!await saveForCommand(document))
                return false;
        }
    }
    return true;
}
async function buildWithOptions(uri) {
    if (!allowTools())
        return;
    const document = await kuDocumentFromCommand(uri);
    if (!document || !await saveForCommand(document))
        return;
    const exe = await findKuExecutable(document.uri);
    if (!exe)
        return;
    const cwd = terminalCwd(document.uri);
    const help = await execFile(exe, ["help", "build"], cwd, 3000);
    if (help.code !== 0) {
        output.append((0, toolTerminal_1.redactToolOutput)(help.stdout + help.stderr));
        output.show(true);
        return;
    }
    const choices = ["CLI 默认构建"];
    if (/--release|--profile/.test(help.stdout))
        choices.push("Release");
    if (/--locked/.test(help.stdout))
        choices.push("锁定依赖（--locked）");
    if (/--backend runner/.test(help.stdout))
        choices.push("Runner（匹配工具链）");
    const choice = await vscode.window.showQuickPick(choices, { title: "Ku：构建选项" });
    if (!choice)
        return;
    const args = ["build"];
    if (choice === "Release")
        args.push("--release");
    if (choice.startsWith("锁定"))
        args.push("--locked");
    if (choice.startsWith("Runner")) {
        const doctor = await execFile(exe, ["doctor", "--json", "--backend", "runner"], cwd);
        output.append((0, toolTerminal_1.redactToolOutput)(doctor.stdout + doctor.stderr));
        if (doctor.code !== 0) {
            output.show(true);
            void vscode.window.showErrorMessage(`Runner 工具链未就绪。CLI：${exe}，请查看 Ku 输出并按该 SDK 的 doctor 提示配置。`);
            return;
        }
        args.push("--backend", "runner");
    }
    const selected = await buildArguments(document, args);
    if (selected && await saveForCommand(document))
        await executeKuTask(document.uri, "Ku Build", exe, selected, cwd);
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
        const resolved = executableOnPath(candidate);
        if (!resolved)
            return { code: 1 };
        const result = await execFile(resolved, ["version"], folder, 3000, signal);
        return { code: result.code === 0 && /\bku\s+\d+\.\d+\.\d+\b/i.test(result.stdout + result.stderr) ? 0 : 1 };
    });
    if (signal?.aborted)
        return undefined;
    if (found) {
        return executableOnPath(found);
    }
    if (notify) {
        const message = configured
            ? `ku.executablePath 无法执行 Ku version：${configured}。请填写可执行文件路径，不附带命令参数或 shell 引号。`
            : "找不到 Ku CLI。请选择已安装的 Ku 可执行文件；仍可继续静态编辑。";
        output.appendLine(message);
        if (await vscode.window.showErrorMessage(message, "选择可执行文件") === "选择可执行文件")
            return selectKuExecutable(uri);
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
    const folder = uri ? containingWorkspace(uri) : vscode.workspace.workspaceFolders?.[0];
    return folder?.uri.fsPath;
}
function containingWorkspace(uri) {
    const direct = vscode.workspace.getWorkspaceFolder(uri);
    if (direct)
        return direct;
    const target = fileIdentity(uri.fsPath);
    return [...(vscode.workspace.workspaceFolders ?? [])].sort((a, b) => b.uri.fsPath.length - a.uri.fsPath.length)
        .find((folder) => {
        const relative = path.relative(fileIdentity(folder.uri.fsPath), target);
        return !relative || (relative !== ".." && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
    });
}
function terminalCwd(uri) {
    return projectDirectory(uri) ?? workspaceFolder(uri) ?? path.dirname(uri.fsPath);
}
function projectDirectory(uri) {
    let directory = path.dirname(uri.fsPath);
    try {
        if (fs.statSync(uri.fsPath).isDirectory())
            directory = uri.fsPath;
    }
    catch { /* A new file may not exist yet. */ }
    for (let depth = 0; depth < 64; depth++) {
        if (fs.existsSync(path.join(directory, "ku.mod")))
            return directory;
        const parent = path.dirname(directory);
        if (parent === directory)
            return undefined;
        directory = parent;
    }
    return undefined;
}
function execFile(file, args, cwd, timeoutMs = 15000, signal) {
    if (!allowTools(false))
        return Promise.resolve({ code: 1, stdout: "", stderr: "Workspace trust is required to run Ku tools.\n" });
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
const documentModels = new WeakMap();
const importedModels = new Map();
const symbolOrigins = new WeakMap();
const resolvingModules = new Set();
function sourceModel(source, uri) {
    const lines = source.split(/\r\n|\n|\r/);
    const starts = [0, ...[...source.matchAll(/\r\n|\n|\r/g)].map((match) => match.index + match[0].length)];
    const imports = (0, imports_1.parseImports)({ lineCount: lines.length, lineAt: (line) => ({ text: lines[line] }) })
        .filter((entry) => codePrefix(source.slice(0, starts[entry.pathRange.start.line] + entry.pathRange.start.character - 1)));
    return (0, semanticModel_1.analyzeSource)(source, { resolveExternal: (qualifiedName) => {
            const imp = [...imports].sort((a, b) => b.path.length - a.path.length)
                .find((entry) => qualifiedName === entry.path || qualifiedName.startsWith(`${entry.path}.`));
            if (!imp || imp.path === "std" || imp.path.startsWith("std."))
                return undefined;
            const imported = (0, imports_1.readImportModule)(uri, imp.path);
            if (!imported.uri || imported.source === undefined)
                return undefined;
            const key = imported.uri.toString();
            if (resolvingModules.has(key))
                return undefined;
            resolvingModules.add(key);
            try {
                let cached = importedModels.get(key);
                if (!cached || cached.source !== imported.source) {
                    cached = { source: imported.source, model: sourceModel(imported.source, imported.uri) };
                    importedModels.set(key, cached);
                    if (importedModels.size > 32)
                        importedModels.delete(importedModels.keys().next().value);
                }
                const exported = (name) => imported.exports.some((entry) => entry.name === name)
                    ? cached.model.resolve(name, imported.source.length) : undefined;
                const name = qualifiedName.slice(imp.path.length).replace(/^\./, "");
                const type = name ? exported(name)?.type : { kind: "object", members: new Map(imported.exports.flatMap((entry) => {
                        const symbol = exported(entry.name);
                        return symbol ? [[entry.name, symbol]] : [];
                    })) };
                if (type)
                    markOrigin(type, imported.uri, imported.source, new Set(), 0);
                return type;
            }
            finally {
                resolvingModules.delete(key);
            }
        } });
}
function markOrigin(type, uri, source, seen, depth) {
    if (depth > 12 || seen.has(type))
        return;
    seen.add(type);
    for (const member of type.members?.values() ?? []) {
        if (member.start >= 0) {
            if (!symbolOrigins.has(member))
                symbolOrigins.set(member, { uri, source });
            markOrigin(member.type, uri, source, seen, depth + 1);
        }
    }
    for (const nested of [type.returnType, type.valueType, type.elementType])
        if (nested)
            markOrigin(nested, uri, source, seen, depth + 1);
}
function semanticModel(document) {
    let cached = documentModels.get(document);
    if (!cached || cached.version !== document.version) {
        cached = { version: document.version, model: sourceModel(document.getText(), document.uri) };
        documentModels.set(document, cached);
    }
    return cached.model;
}
function describeType(type, depth = 0) {
    if (depth > 4)
        return type.name ?? "unknown";
    if (type.kind === "function")
        return `fn(${(type.parameters ?? []).map((p) => `${p.name}${p.optional ? "?" : ""}: ${describeType(p.type, depth + 1)}`).join(", ")}): ${describeType(type.returnType ?? { kind: "unknown" }, depth + 1)}`;
    if (type.kind === "result")
        return `${describeType(type.valueType ?? { kind: "unknown" }, depth + 1)}!`;
    if (type.kind === "task")
        return `Task<${describeType(type.valueType ?? { kind: "unknown" }, depth + 1)}>`;
    if (type.kind === "array")
        return `[${describeType(type.elementType ?? { kind: "unknown" }, depth + 1)}]`;
    const name = type.name ?? (type.members ? `{ ${[...type.members.keys()].slice(0, 8).join(", ")} }` : "unknown");
    return type.nullable && !name.includes("null") ? `${name} | null` : name;
}
function semanticCompletion(symbol, range) {
    const kind = symbol.type.kind === "function" ? vscode.CompletionItemKind.Function
        : symbol.kind === "module" ? vscode.CompletionItemKind.Module : symbol.kind === "field" ? vscode.CompletionItemKind.Field
            : symbol.kind === "struct" ? vscode.CompletionItemKind.Struct : vscode.CompletionItemKind.Variable;
    const item = new vscode.CompletionItem(symbol.name, kind);
    item.range = range;
    item.detail = `${symbol.name}: ${describeType(symbol.type)}`;
    if (["service", "server"].includes(symbol.name) && symbol.type.kind === "function")
        item.insertText = new vscode.SnippetString(`${symbol.name}($0)`);
    return item;
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
            return semanticModel(document).memberSymbols(member.receiver, document.offsetAt(position))
                .filter((symbol) => symbol.name.startsWith(member.prefix)).map((symbol) => semanticCompletion(symbol, member.range));
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
        items.push(...semanticModel(document).visibleSymbols(document.offsetAt(position)).map((symbol) => semanticCompletion(symbol)));
        return items;
    }
}
function memberAccessContext(linePrefix, position) {
    const match = /(?:\?\.|\.)([\p{L}_][\p{L}\p{N}_]*)?$/u.exec(linePrefix);
    if (!match) {
        return undefined;
    }
    const receiver = (0, semanticModel_1.expressionBefore)(linePrefix, match.index)?.text ?? "";
    const prefix = match[1] ?? "";
    const start = position.character - prefix.length;
    return {
        receiver,
        prefix,
        range: new vscode.Range(position.line, start, position.line, position.character),
    };
}
function completionIsCode(document, position) {
    return codePrefix(document.getText(new vscode.Range(new vscode.Position(0, 0), position)));
}
function codePrefix(source) {
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
function importPathItem(label, range, kind) {
    const item = new vscode.CompletionItem(label, kind);
    item.insertText = label;
    item.range = range;
    return item;
}
function dependencyCompletions(document, current, range) {
    try {
        const resolved = (0, imports_1.listDependencyImports)(document, current);
        return resolved.items.map((dep) => {
            const item = importPathItem(dep.path, range, vscode.CompletionItemKind.Module);
            item.detail = dep.dependency.reason ?? `${dep.dependency.name}@${dep.dependency.version ?? "unresolved"} · 当前项目 ku.lock`;
            item.documentation = [dep.exports.map((entry) => entry.signature).join("\n"), dep.dependency.reason, dep.dependency.nextStep].filter(Boolean).join("\n");
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
    const imported = (0, imports_1.readImportModule)(document, importPath);
    return imported.exports.map((entry) => {
        const item = new vscode.CompletionItem(entry.name, entry.kind === "function" ? vscode.CompletionItemKind.Function : vscode.CompletionItemKind.Struct);
        item.detail = entry.signature;
        item.documentation = imported.dependency ? `${imported.dependency.name}@${imported.dependency.version} · ${imported.uri?.fsPath}` : imported.uri?.fsPath;
        return item;
    });
}
function expressionAtWord(document, range) {
    return (0, semanticModel_1.expressionBefore)(document.lineAt(range.start.line).text, range.end.character)?.text ?? document.getText(range);
}
class KuHoverProvider {
    provideHover(document, position) {
        for (const imp of (0, imports_1.parseImports)(document)) {
            if (!imp.pathRange.contains(position) || imp.path.startsWith("std")
                || !completionIsCode(document, new vscode.Position(imp.pathRange.start.line, imp.pathRange.start.character - 1)))
                continue;
            const imported = (0, imports_1.readImportModule)(document, imp.path);
            const text = imported.uri ? [imported.dependency ? `${imported.dependency.name}@${imported.dependency.version}` : imp.path,
                imported.uri.fsPath, ...imported.exports.slice(0, 20).map((entry) => entry.signature)].join("\n\n")
                : [imported.reason, imported.nextStep].filter(Boolean).join("\n\n");
            return new vscode.Hover(new vscode.MarkdownString(text));
        }
        const range = document.getWordRangeAtPosition(position);
        if (!range || !completionIsCode(document, range.end))
            return undefined;
        const word = document.getText(range);
        const expression = expressionAtWord(document, range);
        const type = semanticModel(document).resolveExpression(expression, document.offsetAt(position));
        if (type.kind !== "unknown") {
            const key = type.qualifiedName ?? type.name?.split("(")[0] ?? expression;
            const registered = type.kind === "function" ? (0, metadataModel_1.stdSignatures)(key) : [];
            const signature = registered.length ? registered.map((entry) => entry.label).join("\n") : `${expression}: ${describeType(type)}`;
            const markdown = new vscode.MarkdownString();
            markdown.appendCodeblock(signature, "ku");
            if (registered[0])
                markdown.appendMarkdown(`\n${registered[0].documentation}\n\n实现：${registered[0].source}`);
            return new vscode.Hover(markdown, range);
        }
        const docs = {
            async: "async fn 调用启动一次性 Task。await 消费该句柄；Result 的 ? 需要 Result 返回类型或 try。",
            await: "await task? 等价于 (await task)?；普通 Task 不是可重复等待或可取消的生命周期句柄。",
            fail: "fail 返回可恢复错误；用 Result 返回类型或 try/catch 处理。",
            match: "match 分支匹配；插件不会将 switch 当作 Ku 语法。",
        };
        return docs[word] ? new vscode.Hover(new vscode.MarkdownString(docs[word])) : undefined;
    }
}
function symbolLocation(document, symbol) {
    if (symbol.start < 0)
        return undefined;
    const origin = symbolOrigins.get(symbol);
    if (!origin)
        return new vscode.Location(document.uri, document.positionAt(symbol.start));
    return sourceLocation(origin.uri, origin.source, symbol.start);
}
function sourceLocation(uri, source, offset) {
    const lines = source.slice(0, offset).split(/\r\n|\n|\r/);
    return new vscode.Location(uri, new vscode.Position(lines.length - 1, lines[lines.length - 1].length));
}
class KuDefinitionProvider {
    provideDefinition(document, position) {
        const imports = (0, imports_1.parseImports)(document).filter((entry) => completionIsCode(document, new vscode.Position(entry.pathRange.start.line, entry.pathRange.start.character - 1)));
        for (const imp of imports) {
            if (!imp.pathRange.contains(position))
                continue;
            if (!completionIsCode(document, new vscode.Position(imp.pathRange.start.line, imp.pathRange.start.character - 1)))
                continue;
            const imported = (0, imports_1.readImportModule)(document, imp.path);
            return imported.uri ? new vscode.Location(imported.uri, new vscode.Position(0, 0)) : undefined;
        }
        const range = document.getWordRangeAtPosition(position);
        if (!range || !completionIsCode(document, range.end))
            return undefined;
        const word = document.getText(range), model = semanticModel(document), offset = document.offsetAt(position);
        const expression = expressionAtWord(document, range);
        const dot = expression.lastIndexOf(".");
        if (dot >= 0) {
            const receiver = expression.slice(0, dot).replace(/\?$/, "");
            const field = model.memberSymbols(receiver, offset).find((entry) => entry.name === word);
            if (field)
                return symbolLocation(document, field);
            return undefined;
        }
        const local = model.resolve(word, offset);
        if (local && local.kind !== "module")
            return symbolLocation(document, local);
        for (const imp of imports) {
            const name = imp.kind === "named" ? imp.names.find((entry) => entry.local === word)?.source
                : imp.module === word ? undefined : imp.kind === "glob" ? word : undefined;
            if (!name && imp.module !== word)
                continue;
            const imported = (0, imports_1.readImportModule)(document, imp.path);
            if (!imported.uri || imported.source === undefined)
                continue;
            if (imp.module === word)
                return new vscode.Location(imported.uri, new vscode.Position(0, 0));
            const declaration = imported.exports.find((entry) => entry.name === name);
            if (declaration)
                return sourceLocation(imported.uri, imported.source, declaration.start);
        }
        return undefined;
    }
}
class KuSignatureProvider {
    provideSignatureHelp(document, position) {
        const source = document.getText(new vscode.Range(new vscode.Position(0, 0), position));
        const stack = [];
        let state = "code";
        for (let i = 0; i < source.length; i++) {
            const ch = source[i], next = source[i + 1];
            if (state === "line") {
                if (ch === "\n")
                    state = "code";
                continue;
            }
            if (state === "block") {
                if (ch === "*" && next === "/") {
                    state = "code";
                    i++;
                }
                continue;
            }
            if (state !== "code") {
                if (ch === "\\")
                    i++;
                else if (ch === state)
                    state = "code";
                continue;
            }
            if (ch === "/" && next === "/") {
                state = "line";
                i++;
                continue;
            }
            if (ch === "/" && next === "*") {
                state = "block";
                i++;
                continue;
            }
            if (ch === '"' || ch === "'" || ch === "\x60") {
                state = ch;
                continue;
            }
            if ("([{".includes(ch))
                stack.push({ char: ch, index: i, commas: 0 });
            else if (")]}".includes(ch))
                stack.pop();
            else if (ch === "," && stack.length)
                stack[stack.length - 1].commas++;
        }
        const frame = [...stack].reverse().find((entry) => entry.char === "(");
        if (!frame)
            return undefined;
        const callee = (0, semanticModel_1.expressionBefore)(source, frame.index)?.text;
        if (!callee)
            return undefined;
        const type = semanticModel(document).resolveExpression(callee, document.offsetAt(position));
        if (type.kind !== "function")
            return undefined;
        const registered = (0, metadataModel_1.stdSignatures)(type.qualifiedName ?? type.name?.split("(")[0] ?? callee);
        const help = new vscode.SignatureHelp();
        help.signatures = registered.length ? registered.map((entry) => {
            const signature = new vscode.SignatureInformation(entry.label, entry.documentation);
            signature.parameters = entry.parameters.map((parameter) => new vscode.ParameterInformation(`${parameter.name}${parameter.optional ? "?" : ""}: ${parameter.type}`));
            return signature;
        }) : [new vscode.SignatureInformation(`${callee}: ${describeType(type)}`)];
        if (!registered.length)
            help.signatures[0].parameters = (type.parameters ?? [])
                .map((parameter) => new vscode.ParameterInformation(`${parameter.name}: ${describeType(parameter.type)}`));
        help.activeParameter = frame.commas;
        const count = source.slice(frame.index + 1).trim() ? frame.commas + 1 : 0;
        help.activeSignature = registered.findIndex((entry) => count <= entry.parameters.length && count >= entry.parameters.filter((p) => !p.optional).length);
        if (help.activeSignature < 0)
            help.activeSignature = 0;
        return help;
    }
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
