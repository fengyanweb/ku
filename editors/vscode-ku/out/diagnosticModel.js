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
exports.MAX_DIAGNOSTIC_SOURCE_FILES = exports.MAX_DIAGNOSTIC_SOURCE_BYTES = void 0;
exports.scalarColumnToUtf16 = scalarColumnToUtf16;
exports.sourceLine = sourceLine;
exports.readDiagnosticSource = readDiagnosticSource;
const fs = __importStar(require("fs"));
const util_1 = require("util");
exports.MAX_DIAGNOSTIC_SOURCE_BYTES = 1024 * 1024;
exports.MAX_DIAGNOSTIC_SOURCE_FILES = 8;
const SOURCE_READ_TIMEOUT_MS = 500;
const MAX_ACTIVE_SOURCE_READS = 4;
let activeSourceReads = 0;
// Ku columns count Unicode scalars; VS Code columns count UTF-16 code units.
// Preserve BOM and clamp coordinates to the actual target line.
function scalarColumnToUtf16(line, column) {
    const wanted = Math.max(0, column - 1);
    let scalar = 0;
    let offset = 0;
    while (offset < line.length && scalar < wanted) {
        const code = line.codePointAt(offset);
        offset += code > 0xffff ? 2 : 1;
        scalar++;
    }
    return offset;
}
function sourceLine(source, line) {
    let start = 0;
    for (let current = 1; current < line; current++) {
        const newline = source.indexOf("\n", start);
        if (newline < 0)
            return undefined;
        start = newline + 1;
    }
    const end = source.indexOf("\n", start);
    const text = source.slice(start, end < 0 ? source.length : end);
    return text.endsWith("\r") ? text.slice(0, -1) : text;
}
// Timeout stops waiting, not an already-entered filesystem syscall. Late
// completion still closes its handle, and a process-wide cap prevents repeated
// checks from accumulating arbitrarily many stuck filesystem operations.
async function readDiagnosticSource(file) {
    if (activeSourceReads >= MAX_ACTIVE_SOURCE_READS)
        return undefined;
    activeSourceReads++;
    let expired = false;
    let timer;
    const reading = (async () => {
        let handle;
        try {
            handle = await fs.promises.open(file, fs.constants.O_RDONLY | (fs.constants.O_NONBLOCK || 0));
            if (expired)
                return undefined;
            const before = await handle.stat();
            if (expired || !before.isFile() || before.size > exports.MAX_DIAGNOSTIC_SOURCE_BYTES)
                return undefined;
            const bytes = Buffer.alloc(before.size + 1);
            let length = 0;
            while (!expired && length < bytes.length) {
                const read = await handle.read(bytes, length, Math.min(65536, bytes.length - length), length);
                if (read.bytesRead === 0)
                    break;
                length += read.bytesRead;
            }
            if (expired || length !== before.size)
                return undefined;
            const after = await handle.stat();
            if (expired || after.size !== before.size || after.mtimeMs !== before.mtimeMs)
                return undefined;
            return new util_1.TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes.subarray(0, length));
        }
        catch {
            return undefined;
        }
        finally {
            try {
                await handle?.close();
            }
            catch { /* The diagnostic remains available without source. */ }
            activeSourceReads--;
        }
    })();
    try {
        return await Promise.race([
            reading,
            new Promise((resolve) => {
                timer = setTimeout(() => { expired = true; resolve(undefined); }, SOURCE_READ_TIMEOUT_MS);
            }),
        ]);
    }
    finally {
        if (timer)
            clearTimeout(timer);
    }
}
