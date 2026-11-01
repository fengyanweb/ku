"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.formatKu = formatKu;
// Protect complete lexer tokens before doing any whitespace edits. In particular,
// newlines inside a string are part of its value, not lines to reindent.
function protectTrivia(source) {
    let prefix = "__KU_PROTECTED_";
    while (source.includes(prefix))
        prefix += "_";
    const spans = new Map();
    let masked = "";
    let codeStart = 0;
    for (let i = 0; i < source.length;) {
        const quote = source[i];
        const lineComment = source.startsWith("//", i);
        const blockComment = source.startsWith("/*", i);
        if (quote !== '"' && quote !== "'" && quote !== "`" && !lineComment && !blockComment) {
            i++;
            continue;
        }
        const start = i;
        if (lineComment) {
            while (i < source.length && source[i] !== "\r" && source[i] !== "\n")
                i++;
        }
        else if (blockComment) {
            const close = source.indexOf("*/", i + 2);
            if (close < 0)
                return undefined;
            i = close + 2;
        }
        else {
            i++;
            let closed = false;
            while (i < source.length) {
                if (source[i] === "\\") {
                    i += 2;
                }
                else if (source[i++] === quote) {
                    closed = true;
                    break;
                }
            }
            if (!closed)
                return undefined;
        }
        const token = `${prefix}${spans.size}__`;
        spans.set(token, { text: source.slice(start, i), blockComment });
        masked += source.slice(codeStart, start) + token;
        codeStart = i;
    }
    masked += source.slice(codeStart);
    return { masked, spans, prefix };
}
function restoreTrivia(masked, spans, prefix) {
    return masked.replace(new RegExp(`${prefix}\\d+__`, "g"), (token, offset) => {
        const span = spans.get(token);
        if (!span.blockComment || !/[\r\n]/.test(span.text))
            return span.text;
        const lineStart = masked.lastIndexOf("\n", offset - 1) + 1;
        const followingNewline = masked.indexOf("\n", offset + token.length);
        const lineEnd = followingNewline < 0 ? masked.length : followingNewline;
        const before = masked.slice(lineStart, offset);
        const after = masked.slice(offset + token.length, lineEnd);
        if (!/^[\t ]*$/.test(before) || !/^[\t \r]*$/.test(after))
            return span.text;
        // Standalone block comments follow the surrounding indentation. Remove only
        // their common margin, retaining relative spacing and all comment text.
        const lines = span.text.split(/(\r\n|\n|\r)/);
        const contentLines = lines.filter((_, index) => index > 0 && index % 2 === 0);
        let margin = contentLines.find((line) => line.trim())?.match(/^[\t ]*/)?.[0] ?? "";
        for (const line of contentLines) {
            if (!line.trim())
                continue;
            while (!line.startsWith(margin))
                margin = margin.slice(0, -1);
        }
        return lines.map((line, index) => index > 0 && index % 2 === 0
            ? before + (line.startsWith(margin) ? line.slice(margin.length) : line)
            : line).join("");
    });
}
function formatKu(source, options = {}) {
    const protectedSource = protectTrivia(source);
    // Leave unfinished strings/comments untouched while the user is typing.
    if (!protectedSource)
        return source;
    const eol = source.match(/\r\n|\n|\r/)?.[0] ?? "\n";
    const requestedSize = options.tabSize ?? 4;
    const tabSize = Number.isFinite(requestedSize) && requestedSize > 0 ? Math.max(1, Math.floor(requestedSize)) : 4;
    const indentation = options.insertSpaces === false ? "\t" : " ".repeat(tabSize);
    let indent = 0;
    const out = [];
    let blankRun = 0;
    for (const raw of protectedSource.masked.split(/\r\n|\n|\r/)) {
        const trimmed = raw.trim();
        if (!trimmed) {
            if (++blankRun === 1)
                out.push("");
            continue;
        }
        blankRun = 0;
        const leadingCloses = /^(?:}\s*)+/.exec(trimmed)?.[0].match(/}/g)?.length ?? 0;
        const lineIndent = Math.max(0, indent - leadingCloses);
        const formatted = formatCodeLine(trimmed);
        if (/^(else|catch|finally)\b/.test(formatted) && out.length > 0 && /}\s*$/.test(out[out.length - 1])) {
            out[out.length - 1] += ` ${formatted}`;
        }
        else {
            out.push(`${indentation.repeat(lineIndent)}${formatted}`);
        }
        const opens = (trimmed.match(/{/g) ?? []).length;
        const closes = (trimmed.match(/}/g) ?? []).length;
        indent = Math.max(0, indent + opens - closes);
    }
    while (out[out.length - 1] === "")
        out.pop();
    return restoreTrivia(`${out.join(eol)}${eol}`, protectedSource.spans, protectedSource.prefix);
}
function formatCodeLine(code) {
    let out = "";
    const operators = ["++", "--", "+=", "-=", "*=", "/=", "%=", "==", "!=", "<=", ">=", "&&", "||", "=>", "=", "+", "-", "*", "/", "%", "<", ">"];
    for (let i = 0; i < code.length; i++) {
        const ch = code[i];
        const op = operators.find((candidate) => code.startsWith(candidate, i));
        if (op) {
            if ((op === "<" || op === ">") && isGenericDeclarationAngle(code, i, op)) {
                out += ch;
                continue;
            }
            out = out.replace(/\s+$/, "");
            // Keep a binary + / - separate from a following prefix ++ / --. Without
            // this separator `a + ++b` becomes `a +++b`, changing the lexer's tokens.
            if ((op === "++" || op === "--") && out.endsWith(op[0]))
                out += " ";
            out += op === "++" || op === "--" ? op : ` ${op} `;
            i += op.length - 1;
            while (op !== "++" && op !== "--" && i + 1 < code.length && /\s/.test(code[i + 1]))
                i++;
        }
        else if (ch === "," || ch === ":") {
            out = out.replace(/\s+$/, "") + `${ch} `;
            while (i + 1 < code.length && /\s/.test(code[i + 1]))
                i++;
        }
        else if (ch === "{") {
            out = out.replace(/\s+$/, "") + " {";
        }
        else {
            out += ch;
        }
    }
    return out.replace(/^}\s*(else|catch|finally)\b/, "} $1").replace(/^\s*import\s+/, "import ").trim();
}
function isGenericDeclarationAngle(code, index, op) {
    if (op === "<") {
        return /\b(?:async\s+)?(fn|struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*$/.test(code.slice(0, index).trimEnd());
    }
    let depth = 0;
    for (let i = index; i >= 0; i--) {
        if (code[i] === ">")
            depth++;
        else if (code[i] === "<" && --depth === 0)
            return isGenericDeclarationAngle(code, i, "<");
    }
    return false;
}
