import * as cp from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { builtins, keywords, memberCompletionLabels, stdFunctions, stdImportPathLabels, stdModules, stdRootModules, types } from "./completionModel";
import { defaultModuleName, parseImports, resolveImportUri } from "./imports";

const KU_VERSION = "0.0.16";
const KU_MODE: vscode.DocumentSelector = [{ language: "ku", scheme: "file" }];
const diagnosticCollection = vscode.languages.createDiagnosticCollection("ku");
const output = vscode.window.createOutputChannel("Ku");
let status: vscode.StatusBarItem;
const checkTimers = new Map<string, NodeJS.Timeout>();
const checkGenerations = new Map<string, number>();
const diagnosticUrisByRoot = new Map<string, vscode.Uri[]>();

export function activate(context: vscode.ExtensionContext) {
  context.subscriptions.push(diagnosticCollection, output);

  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 90);
  status.command = "ku.showVersion";
  context.subscriptions.push(status);

  context.subscriptions.push(
    vscode.commands.registerCommand("ku.runCurrentFile", (uri?: vscode.Uri) => runKuCommand("run", uri)),
    vscode.commands.registerCommand("ku.checkCurrentFile", () => checkActiveFile(true)),
    vscode.commands.registerCommand("ku.showIr", () => runKuCommand("ir")),
    vscode.commands.registerCommand("ku.buildCurrentFile", () => runKuCommand("build")),
    vscode.commands.registerCommand("ku.buildNativeC", () => buildNativeC()),
    vscode.commands.registerCommand("ku.packageGc", () => runKuCommand("package gc")),
    vscode.commands.registerCommand("ku.showVersion", () => showVersion()),
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (isKu(doc) && config().get("checkOnOpen", true)) {
        void scheduleCheck(doc, 0);
      }
      void refreshEditorContext(doc);
      if (doc.fileName.endsWith("ku.lock")) {
        void vscode.window.showInformationMessage("ku.lock 是生成文件，通常不建议手动编辑。");
      }
    }),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (isKu(doc) && config().get("checkOnSave", true)) {
        void scheduleCheck(doc, 0);
      }
      void refreshEditorContext(doc);
    }),
    vscode.workspace.onDidChangeTextDocument((event) => {
      if (isKu(event.document) && config().get("checkOnChange", false)) {
        void scheduleCheck(event.document, 500);
      }
      void refreshEditorContext(event.document);
    }),
    vscode.window.onDidChangeActiveTextEditor(() => {
      void refreshStatus();
      void refreshEditorContext();
    }),
    vscode.languages.registerCompletionItemProvider(KU_MODE, new KuCompletionProvider(), ".", "\"", "'", "/", "@"),
    vscode.languages.registerHoverProvider(KU_MODE, new KuHoverProvider()),
    vscode.languages.registerDefinitionProvider(KU_MODE, new KuDefinitionProvider()),
    vscode.languages.registerDocumentSymbolProvider(KU_MODE, new KuSymbolProvider()),
    vscode.languages.registerCodeActionsProvider(KU_MODE, new KuCodeActionProvider(), {
      providedCodeActionKinds: [vscode.CodeActionKind.QuickFix],
    }),
    vscode.languages.registerDocumentFormattingEditProvider(KU_MODE, new KuFormatter()),
  );

  for (const doc of vscode.workspace.textDocuments) {
    if (isKu(doc) && config().get("checkOnOpen", true)) {
      void scheduleCheck(doc, 0);
    }
  }
  void refreshStatus();
  void refreshEditorContext();
}

export function deactivate() {
  diagnosticCollection.clear();
}

function config() {
  return vscode.workspace.getConfiguration("ku");
}

function isKu(document: vscode.TextDocument): boolean {
  return document.languageId === "ku" && document.uri.scheme === "file";
}

async function refreshEditorContext(document = vscode.window.activeTextEditor?.document) {
  const hasMain = !!document && isKu(document) && documentHasMain(document);
  await vscode.commands.executeCommand("setContext", "ku.hasMain", hasMain);
}

function documentHasMain(document: vscode.TextDocument): boolean {
  return /^\s*(?:async\s+)?fn\s+main\s*\(/m.test(document.getText());
}

async function scheduleCheck(document: vscode.TextDocument, delayMs: number) {
  const key = document.uri.toString();
  const existing = checkTimers.get(key);
  if (existing) {
    clearTimeout(existing);
  }
  checkTimers.set(
    key,
    setTimeout(() => {
      checkTimers.delete(key);
      void runCheck(document, false);
    }, delayMs),
  );
}

async function checkActiveFile(reveal: boolean) {
  const editor = vscode.window.activeTextEditor;
  if (!editor || !isKu(editor.document)) {
    void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
    return;
  }
  await runCheck(editor.document, reveal);
}

async function runCheck(document: vscode.TextDocument, reveal: boolean) {
  const rootKey = document.uri.toString();
  const generation = (checkGenerations.get(rootKey) ?? 0) + 1;
  checkGenerations.set(rootKey, generation);
  const exe = await findKuExecutable(document.uri);
  if (!exe) {
    setStatus("Ku: missing", true);
    return;
  }
  let result = await execFile(exe, ["check", "--json", document.uri.fsPath], workspaceFolder(document.uri));
  let diagnostics = parseJsonDiagnosticEntries(result.stdout + result.stderr, document);
  let command = `${exe} check --json ${document.uri.fsPath}`;
  if (result.code !== 0 && diagnostics === undefined) {
    result = await execFile(exe, ["check", document.uri.fsPath], workspaceFolder(document.uri));
    diagnostics = parseTextDiagnosticEntries(result.stdout + result.stderr, document);
    command = `${exe} check ${document.uri.fsPath}`;
  }
  if (checkGenerations.get(rootKey) !== generation) {
    return;
  }
  output.clear();
  output.appendLine(`> ${command}`);
  output.append(result.stdout);
  output.append(result.stderr);
  replaceDiagnostics(rootKey, diagnostics ?? []);
  if (reveal) {
    output.show(true);
  }
  setStatus(result.code === 0 ? `Ku ${KU_VERSION}: check ok` : `Ku ${KU_VERSION}: check failed`, result.code !== 0);
}

interface JsonDiagnostic {
  level: string;
  code: string;
  message: string;
  file: string;
  line: number;
  column: number;
  endLine: number;
  endColumn: number;
  notes: string[];
  helps: string[];
}

export function parseDiagnostics(text: string, document: vscode.TextDocument): vscode.Diagnostic[] {
  return (parseJsonDiagnosticEntries(text, document) ?? parseTextDiagnosticEntries(text, document))
    .map((entry) => entry.diagnostic);
}

interface DiagnosticEntry {
  uri: vscode.Uri;
  diagnostic: vscode.Diagnostic;
}

function parseJsonDiagnosticEntries(text: string, document: vscode.TextDocument): DiagnosticEntry[] | undefined {
  const records: JsonDiagnostic[] = [];
  for (const line of text.split(/\r?\n/)) {
    if (!line.trim()) {
      continue;
    }
    try {
      const value = JSON.parse(line) as Partial<JsonDiagnostic>;
      if (
        typeof value.level !== "string" ||
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
        !value.helps.every((help) => typeof help === "string")
      ) {
        continue;
      }
      records.push(value as JsonDiagnostic);
    } catch {
      // Older Ku executables emit human-readable diagnostics.
    }
  }
  if (records.length === 0) {
    return undefined;
  }
  return records.map((record) => {
    const startLine = Math.max(0, record.line - 1);
    const startColumn = Math.max(0, record.column - 1);
    const endLine = Math.max(startLine, record.endLine - 1);
    const endColumn = endLine === startLine
      ? Math.max(startColumn + 1, record.endColumn - 1)
      : Math.max(0, record.endColumn - 1);
    const details = [
      ...record.notes.map((note) => `note: ${note}`),
      ...record.helps.map((help) => `help: ${help}`),
    ];
    const message = details.length > 0 ? `${record.message}\n${details.join("\n")}` : record.message;
    const diagnostic = new vscode.Diagnostic(
      new vscode.Range(startLine, startColumn, endLine, endColumn),
      message,
      diagnosticSeverity(record.level),
    );
    diagnostic.source = "ku check";
    diagnostic.code = record.code;
    return {
      uri: diagnosticUri(record.file, document),
      diagnostic,
    };
  });
}

function isPositiveInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 1;
}

function diagnosticSeverity(level: string): vscode.DiagnosticSeverity {
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

function parseTextDiagnosticEntries(text: string, document: vscode.TextDocument): DiagnosticEntry[] {
  const diagnostics: DiagnosticEntry[] = [];
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
    diagnostics.push({
      uri: diagnosticUri(location[1], document),
      diagnostic,
    });
  }
  return diagnostics;
}

function diagnosticUri(file: string, document: vscode.TextDocument): vscode.Uri {
  if (!file || file === document.fileName || file === document.uri.fsPath) {
    return document.uri;
  }
  const absolute = path.isAbsolute(file)
    ? file
    : path.resolve(workspaceFolder(document.uri) ?? path.dirname(document.uri.fsPath), file);
  return vscode.Uri.file(absolute);
}

function replaceDiagnostics(rootKey: string, entries: DiagnosticEntry[]) {
  for (const uri of diagnosticUrisByRoot.get(rootKey) ?? []) {
    diagnosticCollection.delete(uri);
  }
  const grouped = new Map<string, { uri: vscode.Uri; diagnostics: vscode.Diagnostic[] }>();
  for (const entry of entries) {
    const key = entry.uri.toString();
    const group = grouped.get(key) ?? { uri: entry.uri, diagnostics: [] };
    group.diagnostics.push(entry.diagnostic);
    grouped.set(key, group);
  }
  const uris: vscode.Uri[] = [];
  for (const group of grouped.values()) {
    diagnosticCollection.set(group.uri, group.diagnostics);
    uris.push(group.uri);
  }
  diagnosticUrisByRoot.set(rootKey, uris);
}

function cleanupMessage(message: string): string {
  return message.replace(/^error(?:\[[A-Z]\d+\])?:\s+/, "").replace(/^error:\s+/, "").trim();
}

function hintFor(message: string): string {
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

async function kuDocumentFromCommand(uri?: vscode.Uri): Promise<vscode.TextDocument | undefined> {
  if (uri?.scheme === "file" && uri.fsPath.endsWith(".ku")) {
    return await vscode.workspace.openTextDocument(uri);
  }
  const editor = vscode.window.activeTextEditor;
  return editor && isKu(editor.document) ? editor.document : undefined;
}

async function runKuCommand(command: "run" | "ir" | "build" | "package gc", uri?: vscode.Uri) {
  const document = await kuDocumentFromCommand(uri);
  if (!document || !isKu(document)) {
    void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
    return;
  }
  if (command === "run" && !documentHasMain(document)) {
    void vscode.window.showWarningMessage("当前 Ku 文件没有 fn main()，不能运行。");
    return;
  }
  await document.save();
  const exe = await findKuExecutable(document.uri);
  if (!exe) {
    return;
  }
  const cwd = terminalCwd(document.uri);
  const terminal = vscode.window.createTerminal({ name: `Ku ${command}`, cwd });
  terminal.show();
  terminal.sendText(terminalCommand(exe, [...command.split(" "), document.uri.fsPath], cwd));
}

async function buildNativeC() {
  const editor = vscode.window.activeTextEditor;
  if (!editor || !isKu(editor.document)) {
    void vscode.window.showWarningMessage("当前文件不是 Ku 源文件。");
    return;
  }
  const unsupported = detectNativeUnsupported(editor.document.getText());
  if (unsupported.length > 0) {
    const answer = await vscode.window.showWarningMessage(
      `当前 native C prototype 不支持：${unsupported.join(", ")}。是否仍然继续构建？`,
      "继续构建",
      "取消",
    );
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

function detectNativeUnsupported(source: string): string[] {
  const checks: Array<[string, RegExp]> = [
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

function setStatus(text: string, warn: boolean) {
  status.text = warn ? `$(warning) ${text}` : `$(check) ${text}`;
  status.tooltip = "Ku interpreter and extension version";
  status.show();
}

async function findKuExecutable(uri?: vscode.Uri, notify = true): Promise<string | undefined> {
  const configured = config().get<string>("executablePath", "").trim();
  const candidates: string[] = [];
  if (configured) {
    candidates.push(configured);
  }
  candidates.push("ku");
  const folder = workspaceFolder(uri);
  if (folder) {
    candidates.push(
      path.join(folder, "release", exeName()),
      path.join(folder, "target", "release", exeName()),
      path.join(folder, "target", "debug", exeName()),
    );
  }
  for (const candidate of [...new Set(candidates)]) {
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

function exeName(): string {
  return process.platform === "win32" ? "ku.exe" : "ku";
}

function workspaceFolder(uri?: vscode.Uri): string | undefined {
  const folder = uri ? vscode.workspace.getWorkspaceFolder(uri) : vscode.workspace.workspaceFolders?.[0];
  return folder?.uri.fsPath;
}

function terminalCwd(uri: vscode.Uri): string {
  return workspaceFolder(uri) ?? path.dirname(uri.fsPath);
}

function execFile(file: string, args: string[], cwd?: string, timeoutMs = 15000): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    cp.execFile(file, args, { cwd, timeout: timeoutMs, windowsHide: true }, (error, stdout, stderr) => {
      const code = typeof (error as cp.ExecFileException | null)?.code === "number" ? ((error as cp.ExecFileException).code as number) : 0;
      resolve({ code, stdout: stdout.toString(), stderr: stderr.toString() });
    });
  });
}

function terminalCommand(exe: string, args: string[], cwd: string): string {
  const quoted = [shortPath(exe, cwd), ...args.map((arg) => shortPath(arg, cwd))].map(shellQuote).join(" ");
  return process.platform === "win32" ? `& ${quoted}` : quoted;
}

function shellQuote(value: string): string {
  if (/^[A-Za-z0-9_./\\:-]+$/.test(value)) {
    return value;
  }
  if (process.platform === "win32") {
    return `"${value.replace(/"/g, '`"')}"`;
  }
  return `"${value.replace(/(["\\$`])/g, "\\$1")}"`;
}

function shortPath(value: string, cwd: string): string {
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

class KuCompletionProvider implements vscode.CompletionItemProvider {
  async provideCompletionItems(document: vscode.TextDocument, position: vscode.Position) {
    const linePrefix = document.lineAt(position).text.slice(0, position.character);
    const items: vscode.CompletionItem[] = [];

    if (isImportPathContext(linePrefix)) {
      return await importPathCompletions(document, position, linePrefix);
    }
    if (isNamedImportContext(document, position)) {
      return exportNameCompletions(document, position);
    }
    const member = memberAccessContext(linePrefix, position);
    if (member) {
      const labels = memberCompletionLabels(member.receiver, member.prefix);
      if (labels.length > 0) {
        return labels.map((label) => memberCompletionItem(label, member.receiver, member.range));
      }
      return [];
    }
    if (/@dep\/?$/.test(linePrefix)) {
      return dependencyCompletions(document);
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

function memberAccessContext(linePrefix: string, position: vscode.Position) {
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

function isImportPathContext(linePrefix: string): boolean {
  return /^\s*import\b.*["'][^"']*$/.test(linePrefix);
}

async function importPathCompletions(document: vscode.TextDocument, position: vscode.Position, linePrefix: string) {
  const quoteMatch = /["']([^"']*)$/.exec(linePrefix);
  const current = quoteMatch?.[1] ?? "";
  const replaceRange = importPathReplaceRange(linePrefix, position);
  if (current.startsWith("std.")) {
    return stdImportPathLabels(current).map((module) => importPathItem(module, replaceRange, vscode.CompletionItemKind.Module));
  }
  if ("std".startsWith(current)) {
    const item = importPathItem("std", replaceRange, vscode.CompletionItemKind.Module);
    return [item, ...stdImportPathLabels("std.").map((module) => importPathItem(module, replaceRange, vscode.CompletionItemKind.Module))];
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
      item.range = replaceRange;
      if (entry.isDirectory()) {
        item.insertText = `${entry.name}/`;
      }
      return item;
    });
}

function importPathReplaceRange(linePrefix: string, position: vscode.Position): vscode.Range | undefined {
  const quoteIndex = Math.max(linePrefix.lastIndexOf("\""), linePrefix.lastIndexOf("'"));
  if (quoteIndex < 0) {
    return undefined;
  }
  return new vscode.Range(position.line, quoteIndex + 1, position.line, position.character);
}

function memberCompletionItem(label: string, receiver: string, range: vscode.Range) {
  const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Method);
  if (receiver === "http" && (label === "service" || label === "server")) {
    item.insertText = new vscode.SnippetString(`${label}($0)`);
  } else {
    item.insertText = label;
  }
  item.range = range;
  item.detail = `${receiver} member`;
  return item;
}

function importPathItem(label: string, range: vscode.Range | undefined, kind: vscode.CompletionItemKind) {
  const item = new vscode.CompletionItem(label, kind);
  item.insertText = label;
  item.range = range;
  return item;
}

function dependencyCompletions(document: vscode.TextDocument) {
  const deps = readKuModDependencies(document);
  return deps.map((dep) => {
    const item = new vscode.CompletionItem(`@${dep}/`, vscode.CompletionItemKind.Module);
    item.detail = "ku.mod dependency";
    return item;
  });
}

function isNamedImportContext(document: vscode.TextDocument, position: vscode.Position): boolean {
  const before = document.getText(new vscode.Range(new vscode.Position(position.line, 0), position));
  return /^\s*import\s+\{[^}]*$/.test(before);
}

function exportNameCompletions(document: vscode.TextDocument, position: vscode.Position) {
  const line = document.lineAt(position).text;
  const importPath = /from\s+["']([^"']+)["']/.exec(line)?.[1];
  if (!importPath) {
    return [];
  }
  if (importPath === "std") {
    return stdRootModules.map((name) => new vscode.CompletionItem(name, vscode.CompletionItemKind.Module));
  }
  const uri = resolveImportUri(document, importPath);
  if (!uri || !fs.existsSync(uri.fsPath)) {
    return [];
  }
  return exportedNames(fs.readFileSync(uri.fsPath, "utf8")).map((name) => new vscode.CompletionItem(name, vscode.CompletionItemKind.Reference));
}

function exportedNames(source: string): string[] {
  const names = new Set<string>();
  for (const match of source.matchAll(/^\s*(?:fn|struct|enum)\s+([A-Z][A-Za-z0-9_]*)/gm)) {
    names.add(match[1]);
  }
  for (const match of source.matchAll(/^\s*([A-Z][A-Za-z0-9_]*)\s*=/gm)) {
    names.add(match[1]);
  }
  return [...names].sort();
}

function fieldCompletions(fields: string[], detail: string) {
  return fields.map((field) => {
    const item = new vscode.CompletionItem(field, vscode.CompletionItemKind.Field);
    item.detail = detail;
    return item;
  });
}

function methodCompletions(methods: string[]) {
  return methods.map((method) => new vscode.CompletionItem(method, vscode.CompletionItemKind.Method));
}

function dottedHoverKey(document: vscode.TextDocument, range: vscode.Range): string | undefined {
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

class KuHoverProvider implements vscode.HoverProvider {
  provideHover(document: vscode.TextDocument, position: vscode.Position) {
    const range = document.getWordRangeAtPosition(position);
    if (!range) {
      return undefined;
    }
    const word = document.getText(range);
    const dotted = dottedHoverKey(document, range);
    const docs: Record<string, string> = {
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
      "fs": "`import \"std.fs\"` 后使用。支持 `fs.read/write/try_read/try_write`。",
      "config": "`import \"std.config\"` 后使用。支持 `config.env/env_file/yaml`。",
      "task": "`task` 不是关键字。业务 task 来自 async fn 调用并只能 await；`std.task` 只提供 stats/stress 诊断压测函数。",
      "object": "`object.get_or(obj, key, default)` 和 `obj.get_or(key, default)` 是显式宽松读取；普通对象缺键仍报错。",
      "object.get_or": "`object.get_or(obj, key, default)` 缺键返回 default，存在则返回字段值；default 参数会立即求值。",
      "get_or": "`obj.get_or(key, default)` 缺键返回 default；default 参数会立即求值，不是惰性计算。",
      "time": "`time.now()` 返回 Time 对象；`time.millis()` 返回当前毫秒时间戳；支持 date/duration/format/parse/sleep。",
      "time.now": "`time.now()` 返回 `{ kind: \"time.time\", millis }`；`time.now(t)` 返回 t 到当前时间的毫秒差。",
      "time.millis": "`time.millis()` 返回当前 Unix 毫秒；`time.millis(timeOrDuration)` 读取 Time/Duration 的毫秒值。",
      "time.unix": "`time.unix()` 返回当前 Unix 秒；`time.unix(time)` 读取 Time 的 Unix 秒。",
      "time.date": "`time.date()` 返回今天日期；`time.date(time, zone)?` 或 `time.date(year, month, day)?` 返回 Date。",
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

class KuDefinitionProvider implements vscode.DefinitionProvider {
  provideDefinition(document: vscode.TextDocument, position: vscode.Position) {
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
    for (const imp of parseImports(document)) {
      const uri = resolveImportUri(document, imp.path);
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

function importDefinition(document: vscode.TextDocument, position: vscode.Position) {
  for (const imp of parseImports(document)) {
    if (imp.pathRange.contains(position)) {
      const uri = resolveImportUri(document, imp.path);
      return uri && fs.existsSync(uri.fsPath) ? new vscode.Location(uri, new vscode.Position(0, 0)) : undefined;
    }
  }
  return undefined;
}

function findDefinitionInDocument(document: vscode.TextDocument, word: string) {
  return findDefinitionInText(document.uri, document.getText(), word);
}

function findDefinitionInText(uri: vscode.Uri, source: string, word: string) {
  const escaped = word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const patterns = [
    new RegExp(`^\\s*(?:async\\s+)?fn\\s+${escaped}\\b`, "m"),
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

class KuSymbolProvider implements vscode.DocumentSymbolProvider {
  provideDocumentSymbols(document: vscode.TextDocument) {
    const symbols: vscode.DocumentSymbol[] = [];
    const stack: vscode.DocumentSymbol[] = [];
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
      } else {
        symbols.push(symbol);
      }
      if (match[1] !== "fn" || !/^\s+(?:async\s+)?fn\b/.test(text)) {
        stack[0] = symbol;
      }
    }
    return symbols;
  }
}

class KuCodeActionProvider implements vscode.CodeActionProvider {
  provideCodeActions(document: vscode.TextDocument, _range: vscode.Range, context: vscode.CodeActionContext) {
    const actions: vscode.CodeAction[] = [];
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

function insertImportAction(document: vscode.TextDocument, module: string) {
  const action = new vscode.CodeAction(`Ku: add import "${module}"`, vscode.CodeActionKind.QuickFix);
  action.edit = new vscode.WorkspaceEdit();
  action.edit.insert(document.uri, new vscode.Position(0, 0), `import "${module}"\n`);
  return action;
}

class KuFormatter implements vscode.DocumentFormattingEditProvider {
  provideDocumentFormattingEdits(document: vscode.TextDocument) {
    const text = document.getText();
    const formatted = formatKu(text);
    if (formatted === text) {
      return [];
    }
    const full = new vscode.Range(document.positionAt(0), document.positionAt(text.length));
    return [vscode.TextEdit.replace(full, formatted)];
  }
}

export function formatKu(source: string): string {
  const lines = source.replace(/\r\n/g, "\n").split("\n");
  let indent = 0;
  const out: string[] = [];
  let blankRun = 0;
  let inBlockComment = false;
  for (const raw of lines) {
    const trimmed = raw.trimEnd().trimStart();
    if (!trimmed) {
      blankRun++;
      if (blankRun === 1) {
        out.push("");
      }
      continue;
    }
    blankRun = 0;
    if (inBlockComment || /^\/\*/.test(trimmed)) {
      out.push(`${"    ".repeat(indent)}${trimmed}`);
      inBlockComment = !trimmed.includes("*/");
      continue;
    }
    if (/^}/.test(trimmed)) {
      indent = Math.max(0, indent - 1);
    }
    const formatted = formatCodeLine(trimmed);
    if (/^(else|catch|finally)\b/.test(formatted) && out.length > 0 && /\}\s*$/.test(out[out.length - 1])) {
      out[out.length - 1] = `${out[out.length - 1]} ${formatted}`;
    } else {
      out.push(`${"    ".repeat(indent)}${formatted}`);
    }
    const balance = braceBalanceOutsideTrivia(formatted);
    indent = Math.max(0, indent + balance.opens - balance.closes);
  }
  while (out.length > 0 && out[out.length - 1] === "") {
    out.pop();
  }
  return `${out.join("\n")}\n`;
}

function formatCodeLine(line: string): string {
  const { code, comment } = splitCommentOutsideTrivia(line);
  const formatted = formatCodeOutsideStrings(code)
    .replace(/^}\s*(else|catch|finally)\b/, "} $1")
    .replace(/^\s*import\s+/, "import ")
    .trim();
  return comment ? `${formatted} ${comment.trimEnd()}`.trimEnd() : formatted;
}

function splitCommentOutsideTrivia(line: string) {
  let quote: string | undefined;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    const next = line[i + 1];
    if (quote) {
      if (ch === "\\") {
        i++;
      } else if (ch === quote) {
        quote = undefined;
      }
      continue;
    }
    if (ch === "\"" || ch === "'" || ch === "`") {
      quote = ch;
      continue;
    }
    if (ch === "/" && next === "/") {
      return { code: line.slice(0, i), comment: line.slice(i) };
    }
  }
  return { code: line, comment: "" };
}

function formatCodeOutsideStrings(code: string): string {
  let out = "";
  let quote: string | undefined;
  const operators = ["++", "--", "+=", "-=", "*=", "/=", "%=", "==", "!=", "<=", ">=", "&&", "||", "=>", "=", "+", "-", "*", "/", "%", "<", ">"];
  for (let i = 0; i < code.length; i++) {
    const ch = code[i];
    if (quote) {
      out += ch;
      if (ch === "\\") {
        i++;
        if (i < code.length) {
          out += code[i];
        }
      } else if (ch === quote) {
        quote = undefined;
      }
      continue;
    }
    if (ch === "\"" || ch === "'" || ch === "`") {
      quote = ch;
      out += ch;
      continue;
    }
    const op = operators.find((candidate) => code.startsWith(candidate, i));
    if (op) {
      if ((op === "<" || op === ">") && isGenericDeclarationAngle(code, i, op)) {
        out += ch;
        continue;
      }
      out = out.replace(/\s+$/, "");
      if (op === "++" || op === "--") {
        out += op;
      } else {
        out += ` ${op} `;
      }
      i += op.length - 1;
      while (op !== "++" && op !== "--" && i + 1 < code.length && /\s/.test(code[i + 1])) {
        i++;
      }
      continue;
    }
    if (ch === ",") {
      out = out.replace(/\s+$/, "");
      out += ", ";
      while (i + 1 < code.length && /\s/.test(code[i + 1])) {
        i++;
      }
      continue;
    }
    if (ch === ":") {
      out = out.replace(/\s+$/, "");
      out += ": ";
      while (i + 1 < code.length && /\s/.test(code[i + 1])) {
        i++;
      }
      continue;
    }
    if (ch === "{") {
      out = out.replace(/\s+$/, "");
      out += " {";
      continue;
    }
    out += ch;
  }
  return out;
}

function isGenericDeclarationAngle(code: string, index: number, op: string): boolean {
  if (op === "<") {
    const before = code.slice(0, index).trimEnd();
    return /\b(?:async\s+)?(fn|struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*$/.test(before);
  }
  let depth = 0;
  for (let i = index; i >= 0; i--) {
    if (code[i] === ">") {
      depth++;
    } else if (code[i] === "<") {
      depth--;
      if (depth === 0) {
        return isGenericDeclarationAngle(code, i, "<");
      }
    }
  }
  return false;
}

function braceBalanceOutsideTrivia(line: string) {
  let opens = 0;
  let closes = 0;
  let quote: string | undefined;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    const next = line[i + 1];
    if (!quote && ch === "/" && next === "/") {
      break;
    }
    if (quote) {
      if (ch === "\\") {
        i++;
      } else if (ch === quote) {
        quote = undefined;
      }
      continue;
    }
    if (ch === "\"" || ch === "'" || ch === "`") {
      quote = ch;
      continue;
    }
    if (ch === "{") {
      opens++;
    } else if (ch === "}") {
      closes++;
    }
  }
  if (/^}/.test(line)) {
    closes = Math.max(0, closes - 1);
  }
  return { opens, closes };
}

function readKuModDependencies(document: vscode.TextDocument): string[] {
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
