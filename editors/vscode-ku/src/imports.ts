import * as path from "path";
import * as fs from "fs";
import * as vscode from "vscode";
import { DependencyStatus, PackageIssue, MAX_INVENTORY_ENTRIES, discoverProject, listProjectDependencies, readPackageText, resolvePackageImport, safePackagePath } from "./packageModel";

export type ImportKind = "namespace" | "named" | "glob";

export interface ImportName {
  source: string;
  local: string;
}

export interface KuImport {
  kind: ImportKind;
  module?: string;
  names: ImportName[];
  path: string;
  range: vscode.Range;
  pathRange: vscode.Range;
}

const IMPORT_PREFIX = /^\s*import\s+(?:(\{[^}]*\})\s+from\s+|([A-Za-z_][A-Za-z0-9_]*)\s+from\s+)?(["'])/;

// Keep raw offsets for VS Code ranges, while resolving the same escape sequences
// as Ku's quoted strings. An apostrophe inside a double-quoted path is ordinary text.
export function importPathContext(line: string) {
  const match = IMPORT_PREFIX.exec(line);
  if (!match) return undefined;
  const quote = match[3];
  const start = match[0].length;
  let end = start;
  while (end < line.length && line[end] !== quote) {
    if (line[end] === "\\") end++;
    end++;
  }
  const raw = line.slice(start, Math.min(end, line.length));
  return { match, quote, start, end: Math.min(end, line.length), closed: end < line.length, raw };
}

export function decodeImportPath(raw: string, quote: string): string | undefined {
  let value = "";
  for (let i = 0; i < raw.length; i++) {
    if (raw[i] !== "\\") {
      value += raw[i];
      continue;
    }
    const escaped = raw[++i];
    if (escaped === "n") value += "\n";
    else if (escaped === "r") value += "\r";
    else if (escaped === "t") value += "\t";
    else if (escaped === "\\" || escaped === quote) value += escaped;
    else return undefined;
  }
  return value;
}

export function parseImports(document: vscode.TextDocument): KuImport[] {
  const imports: KuImport[] = [];
  const maxLines = document.lineCount;
  for (let line = 0; line < maxLines; line++) {
    const text = document.lineAt(line).text;
    const context = importPathContext(text);
    if (!context?.closed) {
      continue;
    }
    const { match } = context;
    const rawNamed = match[1];
    const namespace = match[2];
    const importPath = decodeImportPath(context.raw, context.quote);
    if (!importPath) continue;
    const range = new vscode.Range(line, 0, line, text.length);
    const pathRange = new vscode.Range(line, context.start, line, context.end);

    if (rawNamed) {
      imports.push({
        kind: "named",
        names: parseImportNames(rawNamed),
        path: importPath,
        range,
        pathRange,
      });
    } else if (namespace) {
      imports.push({
        kind: "namespace",
        module: namespace,
        names: [],
        path: importPath,
        range,
        pathRange,
      });
    } else {
      imports.push({
        kind: "glob",
        module: defaultModuleName(importPath),
        names: [],
        path: importPath,
        range,
        pathRange,
      });
    }
  }
  return imports;
}

export function resolveImportUri(
  from: vscode.TextDocument | vscode.Uri,
  importPath: string,
): vscode.Uri | undefined {
  if (isStdImport(importPath)) {
    return undefined;
  }

  const fromPath = "uri" in from ? from.uri.fsPath : from.fsPath;
  if (importPath.startsWith("@")) {
    const resolved = resolvePackageImport(fromPath, importPath);
    return resolved.filePath ? vscode.Uri.file(resolved.filePath) : undefined;
  }
  const project = discoverProject(fromPath).project;
  const baseDir = project && !importPath.startsWith("./") && !importPath.startsWith("../") ? project.importRoot : path.dirname(fromPath);
  let candidate: string;

  if (path.isAbsolute(importPath)) {
    candidate = importPath;
  } else {
    candidate = path.resolve(baseDir, importPath);
  }

  if (!path.extname(candidate)) {
    candidate += ".ku";
  }
  try {
    if (path.extname(candidate).toLowerCase() !== ".ku" || (project && path.isAbsolute(importPath))) return undefined;
    if (project) safePackagePath(project.importRoot, candidate);
    return fs.statSync(candidate).isFile() ? vscode.Uri.file(fs.realpathSync(candidate)) : undefined;
  } catch { return undefined; }
}

export interface ImportedExport {
  name: string;
  kind: "function" | "struct" | "enum" | "value";
  /** UTF-16 offsets of the declaration name in source. */
  start: number;
  end: number;
  signature: string;
}
export interface ImportedModule extends PackageIssue {
  uri?: vscode.Uri;
  source?: string;
  exports: ImportedExport[];
  dependency?: DependencyStatus;
}

/** Collect top-level uppercase declarations, with trivia excluded from syntax. */
export function moduleExports(source: string): ImportedExport[] {
  const tokens: Array<{ text: string; start: number; end: number; depth: number }> = [];
  let depth = 0;
  for (let i = 0; i < source.length;) {
    const start = i;
    const ch = source[i];
    if (/\s/.test(ch)) { i++; continue; }
    if (source.startsWith("//", i)) { const end = source.indexOf("\n", i + 2); i = end < 0 ? source.length : end + 1; continue; }
    if (source.startsWith("/*", i)) { const end = source.indexOf("*/", i + 2); i = end < 0 ? source.length : end + 2; continue; }
    if (ch === '"' || ch === "'" || ch === "`") {
      const quote = source[i++];
      while (i < source.length) { if (source[i] === "\\") i += 2; else if (source[i++] === quote) break; }
      continue;
    }
    if (ch === "}") depth = Math.max(0, depth - 1);
    const word = /^[A-Za-z_][A-Za-z0-9_]*/.exec(source.slice(i));
    i += word ? word[0].length : 1;
    tokens.push({ text: source.slice(start, i), start, end: i, depth });
    if (ch === "{") depth++;
  }
  const result: ImportedExport[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.depth !== 0 || !/^[A-Z][A-Za-z0-9_]*$/.test(token.text)) continue;
    const previous = tokens[i - 1];
    const kind = previous && ["fn", "struct", "enum"].includes(previous.text) ? previous.text as "fn" | "struct" | "enum" : undefined;
    // Ku exports uppercase fn/struct/enum declarations, never module labels or
    // arbitrary assignment expressions (src/cli.rs::item_export_name).
    if (!kind) continue;
    let end = source.indexOf("\n", token.end);
    if (end < 0) end = source.length;
    const declarationStart = kind === "fn" && tokens[i - 2]?.text === "async" ? tokens[i - 2].start : previous.start;
    let signature = source.slice(declarationStart, end).trim();
    if (kind === "fn") {
      let parens = 0;
      for (let j = i + 1; j < tokens.length; j++) {
        if (tokens[j].text === "(") parens++;
        else if (tokens[j].text === ")") parens--;
        else if (tokens[j].text === "{" && parens === 0) { signature = source.slice(declarationStart, tokens[j].start).trim(); break; }
      }
    }
    result.push({ name: token.text, kind: kind === "fn" ? "function" : kind ?? "value", start: token.start, end: token.end, signature });
  }
  return result;
}

/** Read the exact installed Ku module (including generated interop declarations). */
export function readImportModule(from: vscode.TextDocument | vscode.Uri, importPath: string): ImportedModule {
  const fromPath = "uri" in from ? from.uri.fsPath : from.fsPath;
  const resolved = importPath.startsWith("@") ? resolvePackageImport(fromPath, importPath) : undefined;
  if (resolved && !resolved.filePath) return { ...resolved, exports: [] };
  const uri = resolved?.filePath ? vscode.Uri.file(resolved.filePath) : resolveImportUri(from, importPath);
  if (!uri) return { exports: [], reason: isStdImport(importPath) ? "标准库使用内置 API 元数据。" : `找不到导入文件：${importPath}` };
  try {
    const source = readPackageText(uri.fsPath);
    return { uri, source, exports: moduleExports(source), dependency: resolved?.dependency };
  } catch (error) { return { exports: [], dependency: resolved?.dependency, reason: error instanceof Error ? error.message : String(error), nextStep: resolved?.dependency ? "在当前项目手动运行 ku i，恢复锁定依赖。" : undefined }; }
}

export interface DependencyImportItem {
  path: string;
  filePath?: string;
  exports: ImportedExport[];
  dependency: DependencyStatus;
}

/** Only walk the selected dependency's installed src/, with a fixed entry budget. */
export function listDependencyImports(from: vscode.TextDocument | vscode.Uri, prefix = "@"): { items: DependencyImportItem[] } & PackageIssue {
  const fromPath = "uri" in from ? from.uri.fsPath : from.fsPath;
  const requested = /^@([a-z][a-z0-9_-]*)\//.exec(prefix)?.[1];
  const selected = listProjectDependencies(fromPath, requested);
  const items: DependencyImportItem[] = [];
  let entries = 0;
  let bytes = 0;
  if (requested && selected.project && !selected.dependencies.some((dep) => dep.name === requested)) {
    return { items, reason: `当前项目没有声明依赖 ${requested}。`, nextStep: `先在当前项目手动运行 ku i ${requested}。` };
  }
  try {
    for (const dependency of selected.dependencies) {
      const base = `@${dependency.name}/`;
      if ((!requested && base.startsWith(prefix)) || prefix === "@") items.push({ path: base, exports: [], dependency });
      if (!dependency.importRoot || requested !== dependency.name) continue;
      const pending = [{ directory: dependency.importRoot, depth: 0 }];
      while (pending.length) {
        const current = pending.pop()!;
        if (current.depth > 32) throw new Error("依赖模块目录深度超过 32。");
        for (const entry of fs.readdirSync(current.directory, { withFileTypes: true })) {
          if (++entries > MAX_INVENTORY_ENTRIES) throw new Error(`依赖模块目录超过 ${MAX_INVENTORY_ENTRIES} 项。`);
          const target = path.join(current.directory, entry.name);
          safePackagePath(dependency.importRoot, target);
          if (entry.isDirectory()) { pending.push({ directory: target, depth: current.depth + 1 }); continue; }
          if (!entry.isFile() || !entry.name.endsWith(".ku")) continue;
          const modulePath = base + path.relative(dependency.importRoot, target).replace(/\\/g, "/").slice(0, -3);
          if (!modulePath.startsWith(prefix)) continue;
          const source = readPackageText(target);
          bytes += Buffer.byteLength(source, "utf8");
          if (bytes > 10_000_000) throw new Error("依赖模块源码超过 10 MB 读取预算。");
          const exports = moduleExports(source);
          if (exports.length) items.push({ path: modulePath, filePath: target, exports, dependency });
        }
      }
    }
    const missing = requested ? selected.dependencies.find((dep) => dep.name === requested) : undefined;
    const noMatch = requested && !items.length && missing?.importRoot;
    return { items, reason: selected.reason ?? missing?.reason ?? (noMatch ? `锁定依赖 ${requested} 中没有匹配 ${prefix} 的公开 Ku 模块。` : undefined),
      nextStep: selected.nextStep ?? missing?.nextStep ?? (noMatch ? "核对依赖安装回执中的 import 路径；需要恢复缓存时在当前项目手动运行 ku i。" : undefined) };
  } catch (error) { return { items, reason: error instanceof Error ? error.message : String(error), nextStep: "在当前项目手动运行 ku i，并检查依赖的 src/。" }; }
}

export function isStdImport(importPath: string): boolean {
  return importPath === "std" || importPath.startsWith("std.");
}

export function defaultModuleName(importPath: string): string | undefined {
  if (importPath.startsWith("std.")) {
    return importPath.slice("std.".length);
  }

  const parsed = path.parse(importPath.replace(/\\/g, "/"));
  return parsed.name || undefined;
}

function parseImportNames(rawNamed: string): ImportName[] {
  const inner = rawNamed.slice(1, -1);
  return inner
    .split(",")
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => {
      const alias = /^([A-Za-z_][A-Za-z0-9_]*)\s+as\s+([A-Za-z_][A-Za-z0-9_]*)$/.exec(part);
      if (alias) {
        return { source: alias[1], local: alias[2] };
      }
      return { source: part, local: part };
    });
}
