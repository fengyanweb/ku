import * as path from "path";
import * as vscode from "vscode";

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

const IMPORT_LINE =
  /^\s*import\s+(?:(\{[^}]*\})\s+from\s+|([A-Za-z_][A-Za-z0-9_]*)\s+from\s+)?["']([^"']+)["']/;

export function parseImports(document: vscode.TextDocument): KuImport[] {
  const imports: KuImport[] = [];
  const maxLines = document.lineCount;
  for (let line = 0; line < maxLines; line++) {
    const text = document.lineAt(line).text;
    const match = IMPORT_LINE.exec(text);
    if (!match) {
      continue;
    }

    const rawNamed = match[1];
    const namespace = match[2];
    const importPath = match[3];
    const pathStart = text.indexOf(importPath);
    const range = new vscode.Range(line, 0, line, text.length);
    const pathRange = new vscode.Range(line, pathStart, line, pathStart + importPath.length);

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
  const baseDir = path.dirname(fromPath);
  let candidate: string;

  if (path.isAbsolute(importPath)) {
    candidate = importPath;
  } else {
    candidate = path.resolve(baseDir, importPath);
  }

  if (!candidate.toLowerCase().endsWith(".ku")) {
    candidate += ".ku";
  }
  return vscode.Uri.file(candidate);
}

export function isStdImport(importPath: string): boolean {
  return importPath.startsWith("std.");
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
