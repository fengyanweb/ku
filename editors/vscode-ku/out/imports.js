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
exports.parseImports = parseImports;
exports.resolveImportUri = resolveImportUri;
exports.isStdImport = isStdImport;
exports.defaultModuleName = defaultModuleName;
const path = __importStar(require("path"));
const vscode = __importStar(require("vscode"));
const IMPORT_LINE = /^\s*import\s+(?:(\{[^}]*\})\s+from\s+|([A-Za-z_][A-Za-z0-9_]*)\s+from\s+)?["']([^"']+)["']/;
function parseImports(document) {
    const imports = [];
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
        }
        else if (namespace) {
            imports.push({
                kind: "namespace",
                module: namespace,
                names: [],
                path: importPath,
                range,
                pathRange,
            });
        }
        else {
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
function resolveImportUri(from, importPath) {
    if (isStdImport(importPath)) {
        return undefined;
    }
    const fromPath = "uri" in from ? from.uri.fsPath : from.fsPath;
    const baseDir = path.dirname(fromPath);
    let candidate;
    if (path.isAbsolute(importPath)) {
        candidate = importPath;
    }
    else {
        candidate = path.resolve(baseDir, importPath);
    }
    if (!candidate.toLowerCase().endsWith(".ku")) {
        candidate += ".ku";
    }
    return vscode.Uri.file(candidate);
}
function isStdImport(importPath) {
    return importPath.startsWith("std.");
}
function defaultModuleName(importPath) {
    if (importPath.startsWith("std.")) {
        return importPath.slice("std.".length);
    }
    const parsed = path.parse(importPath.replace(/\\/g, "/"));
    return parsed.name || undefined;
}
function parseImportNames(rawNamed) {
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
