"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.SemanticModel = void 0;
exports.expressionBefore = expressionBefore;
exports.analyzeSource = analyzeSource;
const metadataModel_1 = require("./metadataModel");
const UNKNOWN = { kind: "unknown" };
const unknown = () => ({ tag: "unknown" });
const primitive = (name) => ({ kind: "primitive", name });
// Comments are discarded; entire strings remain opaque, including multiline
// strings and templates. Their contents can never introduce declarations.
function tokenize(source) {
    const tokens = [];
    let line = 0;
    for (let i = 0; i < source.length;) {
        if (/\s/.test(source[i])) {
            if (source[i++] === "\n")
                line++;
            continue;
        }
        if (source.startsWith("//", i)) {
            while (i < source.length && source[i] !== "\n")
                i++;
            continue;
        }
        if (source.startsWith("/*", i)) {
            i += 2;
            while (i < source.length && !source.startsWith("*/", i))
                if (source[i++] === "\n")
                    line++;
            i = Math.min(source.length, i + 2);
            continue;
        }
        const start = i, startLine = line;
        const quote = source[i];
        let kind = "punct";
        if (quote === '"' || quote === "'" || quote === "`") {
            kind = "string";
            i++;
            while (i < source.length) {
                const ch = source[i++];
                if (ch === "\n")
                    line++;
                if (ch === "\\") {
                    if (source[i] === "\n")
                        line++;
                    i = Math.min(source.length, i + 1);
                }
                else if (ch === quote)
                    break;
            }
        }
        else {
            const id = /^[\p{L}_][\p{L}\p{N}_]*/u.exec(source.slice(i));
            const number = /^\d+(?:\.\d+)?/.exec(source.slice(i));
            if (id) {
                i += id[0].length;
                kind = "id";
            }
            else if (number) {
                i += number[0].length;
                kind = "number";
            }
            else
                i += ["...", "?.", "=>", "==", "!=", "<=", ">=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "%="].find((op) => source.startsWith(op, i))?.length ?? 1;
        }
        tokens.push({ text: source.slice(start, i), start, end: i, line: startLine, endLine: line, kind });
    }
    return tokens;
}
function stringValue(token) {
    const text = token.text;
    return text.slice(1, text.endsWith(text[0]) ? -1 : undefined).replace(/\\([nrt\\"'])/g, (_, ch) => ({ n: "\n", r: "\r", t: "\t" }[ch] ?? ch));
}
/** Read the final receiver/callee without interpreting punctuation in trivia. */
function expressionBefore(source, endOffset = source.length) {
    const end = Number.isFinite(endOffset) ? Math.max(0, Math.min(source.length, Math.floor(endOffset))) : source.length;
    const tokens = tokenize(source.slice(0, end));
    if (!tokens.length)
        return undefined;
    const openers = [], pairs = new Map();
    for (let i = 0; i < tokens.length; i++) {
        const text = tokens[i].text;
        if (["(", "[", "{"].includes(text))
            openers.push(i);
        else if ([")", "]", "}"].includes(text)) {
            const opener = openers.pop();
            if (opener !== undefined && "([{".indexOf(tokens[opener].text) === ")]}".indexOf(text))
                pairs.set(i, opener);
        }
    }
    const reserved = new Set(["return", "fail", "panic", "if", "else", "fn", "async", "await", "for", "in", "while", "try", "catch", "finally", "match", "struct", "enum", "import", "from", "module", "print"]);
    const canEnd = (index) => index >= 0 && (tokens[index].kind === "id" && !reserved.has(tokens[index].text)
        || tokens[index].kind === "string" || tokens[index].kind === "number" || [")", "]", "}", "?"].includes(tokens[index].text));
    const consume = (last, depth) => {
        if (last < 0 || depth > 64)
            return undefined;
        let index = last;
        while (tokens[index]?.text === "?")
            index--;
        if (index < 0)
            return undefined;
        const token = tokens[index];
        let start;
        if ([")", "]", "}"].includes(token.text)) {
            const opener = pairs.get(index);
            if (opener === undefined)
                return undefined;
            start = opener;
            if (canEnd(opener - 1)) {
                const prior = consume(opener - 1, depth + 1);
                if (prior !== undefined)
                    start = prior;
            }
        }
        else if (token.kind === "id" || token.kind === "string" || token.kind === "number") {
            if (token.kind === "id" && reserved.has(token.text))
                return undefined;
            start = index;
        }
        else
            return undefined;
        if (tokens[start - 1]?.text === "." || tokens[start - 1]?.text === "?.") {
            const receiver = consume(start - 2, depth + 1);
            if (receiver === undefined)
                return undefined;
            start = receiver;
        }
        while (tokens[start - 1]?.text === "await")
            start--;
        return start;
    };
    const first = consume(tokens.length - 1, 0);
    if (first === undefined)
        return undefined;
    const start = tokens[first].start, finish = tokens[tokens.length - 1].end;
    return { text: source.slice(start, finish), start, end: finish };
}
class SemanticModel {
    constructor(source, options = {}) {
        this.source = source;
        this.options = options;
        this.pairs = new Map();
        this.busy = new Set();
        this.expressionDepth = 0;
        this.structureDepth = 0;
        this.tokens = tokenize(source);
        const stack = [];
        for (let i = 0; i < this.tokens.length; i++) {
            const ch = this.tokens[i].text;
            if (["(", "[", "{"].includes(ch))
                stack.push(i);
            else if ([")", "]", "}"].includes(ch)) {
                const opener = stack[stack.length - 1];
                if (opener !== undefined && "([{".indexOf(this.tokens[opener].text) === ")]}".indexOf(ch)) {
                    stack.pop();
                    this.pairs.set(opener, i);
                }
            }
        }
        this.root = { start: 0, end: source.length, symbols: [], children: [] };
        this.statements(0, this.tokens.length, this.root);
    }
    visibleSymbols(offset) {
        const result = new Map();
        for (let scope = this.scopeAt(offset); scope; scope = scope.parent) {
            for (let i = scope.symbols.length - 1; i >= 0; i--) {
                const binding = scope.symbols[i];
                if (!result.has(binding.name) && (binding.hoisted || binding.token.start <= offset))
                    result.set(binding.name, this.symbol(binding, offset));
            }
        }
        return [...result.values()];
    }
    resolve(name, offset) {
        const binding = this.lookup(name, this.scopeAt(offset), offset);
        return binding && this.symbol(binding, offset);
    }
    resolveExpression(expression, offset) {
        const fragment = new SemanticModel(expression, this.options);
        for (const token of fragment.tokens) {
            token.start = offset;
            token.end = offset;
        }
        const expr = fragment.expression(0, fragment.tokens.length, this.scopeAt(offset), false);
        // References in a query fragment resolve in the original document's scope.
        return this.infer(expr, offset, 0);
    }
    memberSymbols(receiverExpression, offset) {
        return [...(this.resolveExpression(receiverExpression, offset).members?.values() ?? [])];
    }
    /** Metadata adapters can use the same type annotation parser as source code. */
    typeFromAnnotation(annotation, offset = 0) {
        return this.typeTokens(tokenize(annotation), this.scopeAt(offset), offset, 0);
    }
    scopeAt(offset, scope = this.root) {
        for (const child of scope.children)
            if (child.start <= offset && offset < child.end)
                return this.scopeAt(offset, child);
        return scope;
    }
    child(parent, start, end) {
        const scope = { start, end, parent, symbols: [], children: [] };
        parent.children.push(scope);
        return scope;
    }
    lookup(name, scope, offset) {
        for (let current = scope; current; current = current.parent) {
            for (let i = current.symbols.length - 1; i >= 0; i--) {
                const binding = current.symbols[i];
                if (binding.name === name && (binding.hoisted || binding.token.start <= offset))
                    return binding;
            }
        }
        return undefined;
    }
    symbol(binding, offset) {
        return { name: binding.name, start: binding.token.start, end: binding.token.end, kind: binding.kind,
            type: this.bindingType(binding, offset, 0), detail: binding.annotation?.map((token) => token.text).join("") };
    }
    bindingType(binding, offset, depth) {
        if (depth > 48 || this.busy.has(binding))
            return UNKNOWN;
        this.busy.add(binding);
        try {
            if (binding.fixed)
                return binding.fixed;
            if (binding.fn)
                return this.functionType(binding.fn, offset, depth + 1);
            if (binding.annotation?.length)
                return this.typeTokens(binding.annotation, binding.scope, offset, depth + 1);
            if (binding.qualifiedName)
                return this.external(binding.qualifiedName, undefined, true) ?? { kind: "object", qualifiedName: binding.qualifiedName };
            let type = binding.expression ? this.infer(binding.expression, Math.min(offset, binding.token.start), depth + 1) : UNKNOWN;
            for (const revision of binding.revisions ?? []) {
                if (revision.token.start > offset)
                    continue;
                let inFunction = false;
                for (let scope = revision.scope; scope && scope !== binding.scope; scope = scope.parent)
                    inFunction || (inFunction = !!scope.functionScope);
                const active = this.isAncestor(revision.scope, this.scopeAt(offset));
                if (inFunction && !active)
                    continue;
                const expr = revision.expression;
                const cloneSelf = expr.tag === "call" && expr.base.tag === "member" && expr.base.name === "clone" && expr.base.base.tag === "id" && expr.base.base.token.text === binding.name;
                const next = cloneSelf ? type : this.infer(expr, revision.token.start, depth + 1);
                type = active || this.sameType(type, next) ? next : UNKNOWN;
            }
            return type;
        }
        finally {
            this.busy.delete(binding);
        }
    }
    close(index, limit) { return Math.min(this.pairs.get(index) ?? limit, limit); }
    isAncestor(ancestor, scope) {
        for (let current = scope; current; current = current.parent)
            if (current === ancestor)
                return true;
        return false;
    }
    sameType(left, right, depth = 0) {
        if (left === right)
            return true;
        if (depth > 6 || left.kind !== right.kind || left.name !== right.name || left.nullable !== right.nullable)
            return false;
        const keys = [...(left.members?.keys() ?? [])];
        return keys.join("\0") === [...(right.members?.keys() ?? [])].join("\0") && keys.every((key) => this.sameType(left.members.get(key).type, right.members.get(key).type, depth + 1));
    }
    split(start, end, separator = ",") {
        const ranges = [];
        let begin = start;
        for (let i = start; i < end; i++) {
            if (this.tokens[i].text === separator) {
                ranges.push([begin, i]);
                begin = i + 1;
            }
            else if (["(", "[", "{"].includes(this.tokens[i].text))
                i = this.close(i, end);
        }
        if (begin < end)
            ranges.push([begin, end]);
        return ranges;
    }
    statementEnd(start, end) {
        for (let i = start; i < end; i++) {
            const token = this.tokens[i];
            if (token.text === ";" || token.text === "}")
                return i;
            if (i > start && token.line > this.tokens[i - 1].endLine && !/[=,+\-*/%&|.:(]$/.test(this.tokens[i - 1].text) && token.text !== "." && token.text !== "?.")
                return i;
            if (["(", "[", "{"].includes(token.text))
                i = this.close(i, end);
        }
        return end;
    }
    statements(start, end, scope, fn) {
        if (this.structureDepth >= 64)
            return;
        this.structureDepth++;
        try {
            this.parseStatements(start, end, scope, fn);
        }
        finally {
            this.structureDepth--;
        }
    }
    parseStatements(start, end, scope, fn) {
        for (let i = start; i < end;) {
            const token = this.tokens[i];
            if (token.text === ";") {
                i++;
                continue;
            }
            if (token.text === "import") {
                i = this.importStatement(i, end, scope);
                continue;
            }
            if ((token.text === "async" && this.tokens[i + 1]?.text === "fn") || (token.text === "fn" && this.tokens[i + 1]?.kind === "id")) {
                const parsed = this.functionDeclaration(i, end, scope);
                i = Math.max(i + 1, parsed.end);
                continue;
            }
            if ((token.text === "struct" || token.text === "enum") && this.tokens[i + 1]?.kind === "id") {
                i = this.typeDeclaration(i, end, scope);
                continue;
            }
            if (token.text === "return") {
                const stop = this.statementEnd(i + 1, end);
                const expr = this.expression(i + 1, stop, scope);
                fn?.returns.push(expr);
                i = Math.max(i + 1, stop);
                continue;
            }
            if (["if", "else", "while", "for", "try", "catch", "finally", "match"].includes(token.text)) {
                let body = i + 1;
                if (this.tokens[body]?.text === "if") {
                    i = body;
                    continue;
                }
                if (this.tokens[body]?.text === "(")
                    body = this.close(body, end) + 1;
                while (body < end && this.tokens[body].text !== "{" && this.tokens[body].text !== ";") {
                    // A brace on a later unrelated statement is not this condition's body.
                    if (this.tokens[body].line > token.line || ["fail", "return", "panic", "println", "print"].includes(this.tokens[body].text))
                        break;
                    if (this.tokens[body].text === "(")
                        body = this.close(body, end);
                    body++;
                }
                if (body < end && this.tokens[body].text === "{") {
                    const close = this.close(body, end);
                    const child = this.child(scope, this.tokens[body].start, close < end ? this.tokens[close].end : this.source.length + 1);
                    if (token.text === "catch") {
                        const parameter = this.tokens.slice(i + 1, body).find((part) => part.kind === "id");
                        if (parameter)
                            child.symbols.push({ name: parameter.text, token: parameter, kind: "parameter", scope: child, hoisted: true, fixed: this.errorType() });
                    }
                    if (token.text === "for" && this.tokens[i + 1]?.kind === "id") {
                        const parameter = this.tokens[i + 1];
                        child.symbols.push({ name: parameter.text, token: parameter, kind: "variable", scope: child, hoisted: true });
                    }
                    this.statements(body + 1, close, child, fn);
                    i = close + 1;
                    continue;
                }
            }
            const stop = this.statementEnd(i, end);
            let equal = -1;
            for (let at = i; at < stop; at++) {
                if (this.tokens[at].text === "=") {
                    equal = at;
                    break;
                }
                if (["(", "[", "{"].includes(this.tokens[at].text))
                    at = this.close(at, stop);
            }
            if (equal >= 0) {
                const expression = this.expression(equal + 1, stop, scope);
                if (token.kind === "id" && (equal === i + 1 || this.tokens[i + 1]?.text === ":")) {
                    // Reassignment updates an existing name; it never creates a fake
                    // shadow declaration in a nested branch.
                    const existing = this.lookup(token.text, scope, token.start);
                    if (existing)
                        (existing.revisions ?? (existing.revisions = [])).push({ token, scope, expression });
                    else
                        scope.symbols.push({ name: token.text, token, kind: "variable", scope,
                            annotation: this.tokens[i + 1]?.text === ":" ? this.tokens.slice(i + 2, equal) : undefined, expression });
                }
                else if (token.text === "{")
                    this.destructure(i, equal, expression, scope);
            }
            else if (token.kind === "id" && this.tokens[i + 1]?.text === ":") {
                scope.symbols.push({ name: token.text, token, kind: "variable", scope, annotation: this.tokens.slice(i + 2, stop) });
            }
            else
                this.expression(i, stop, scope);
            i = Math.max(i + 1, stop);
        }
    }
    importStatement(start, end, scope) {
        const stop = this.statementEnd(start, end);
        const pathToken = this.tokens.slice(start + 1, stop).find((token) => token.kind === "string");
        if (!pathToken)
            return Math.max(start + 1, stop);
        const modulePath = stringValue(pathToken);
        const first = this.tokens[start + 1];
        if (first?.kind === "id" && first.text !== "from")
            scope.symbols.push({ name: first.text, token: first, kind: "module", scope, hoisted: true, qualifiedName: modulePath });
        else if (first?.text === "{") {
            for (const [a, b] of this.split(start + 2, this.close(start + 1, stop))) {
                const imported = this.tokens[a];
                if (imported?.kind !== "id")
                    continue;
                const local = this.tokens[a + 1]?.text === "as" ? this.tokens[a + 2] : imported;
                if (local && local.start < (this.tokens[b]?.start ?? Infinity))
                    scope.symbols.push({ name: local.text, token: local, kind: "module", scope, hoisted: true, qualifiedName: `${modulePath}.${imported.text}` });
            }
        }
        else {
            const name = modulePath.replace(/\\/g, "/").replace(/\.ku$/, "").split(/[/.]/).filter(Boolean).pop();
            if (name)
                scope.symbols.push({ name, token: pathToken, kind: "module", scope, hoisted: true, qualifiedName: modulePath });
        }
        return Math.max(start + 1, stop);
    }
    typeDeclaration(start, end, scope) {
        const name = this.tokens[start + 1];
        let body = start + 2;
        while (body < end && this.tokens[body].text !== "{")
            body++;
        const close = this.close(body, end);
        const members = new Map();
        const type = { kind: "object", name: name.text, members };
        scope.symbols.push({ name: name.text, token: name, kind: this.tokens[start].text, scope, hoisted: true, fixed: type });
        for (let i = body + 1; i < close;) {
            const field = this.tokens[i];
            let stop = i + 1;
            while (stop < close && this.tokens[stop].text !== "," && this.tokens[stop].line === field.line) {
                if (stop > i + 2 && this.tokens[stop].kind === "id" && this.tokens[stop + 1]?.text === ":")
                    break;
                if (["(", "["].includes(this.tokens[stop].text))
                    stop = this.close(stop, close);
                stop++;
            }
            if (field.kind === "id" && this.tokens[i + 1]?.text === ":") {
                const annotation = this.tokens.slice(i + 2, stop);
                // A getter defers references to structs declared later in the file.
                const model = this;
                members.set(field.text, { name: field.text, start: field.start, end: field.end, kind: "field",
                    get type() { return model.typeTokens(annotation, scope, field.start, 0); } });
            }
            i = Math.max(i + 1, stop + (this.tokens[stop]?.text === "," ? 1 : 0));
        }
        return Math.min(close + 1, end);
    }
    functionDeclaration(start, end, parent, named = true) {
        let i = start;
        const async = this.tokens[i]?.text === "async";
        if (async)
            i++;
        i++; // fn
        const name = named && this.tokens[i]?.kind === "id" ? this.tokens[i++] : undefined;
        while (i < end && this.tokens[i].text !== "(")
            i++;
        const parameterEnd = this.close(i, end);
        let body = parameterEnd + 1;
        const annotationStart = this.tokens[body]?.text === ":" ? ++body : -1;
        while (body < end && this.tokens[body].text !== "{")
            body++;
        const close = this.close(body, end);
        const scope = this.child(parent, this.tokens[i]?.start ?? this.tokens[start].start, close < end ? this.tokens[close].end : this.source.length + 1);
        scope.functionScope = true;
        const fn = { scope, parameters: [], returns: [], async, annotation: annotationStart >= 0 ? this.tokens.slice(annotationStart, body) : undefined };
        if (name)
            parent.symbols.push({ name: name.text, token: name, kind: "function", scope: parent, hoisted: true, fn });
        this.parameters(i + 1, parameterEnd, scope, fn);
        this.statements(body + 1, close, scope, fn);
        return { fn, end: Math.min(close + 1, end) };
    }
    parameters(start, end, scope, fn) {
        for (const [a, b] of this.split(start, end)) {
            const parameterStart = this.tokens[a]?.text === "&" ? a + 1 : a;
            const token = this.tokens[parameterStart];
            if (token?.kind !== "id")
                continue;
            const binding = { name: token.text, token, kind: "parameter", scope, hoisted: true,
                annotation: this.tokens[parameterStart + 1]?.text === ":" ? this.tokens.slice(parameterStart + 2, b) : undefined };
            scope.symbols.push(binding);
            fn.parameters.push(binding);
        }
    }
    destructure(start, end, base, scope, keys = []) {
        const close = Math.min(this.close(start, end), end);
        const fields = this.split(start + 1, close);
        const excluded = fields.filter(([a]) => this.tokens[a]?.text !== "...").map(([a]) => this.tokens[a]?.kind === "string" ? stringValue(this.tokens[a]) : this.tokens[a]?.text);
        for (const [a, b] of fields) {
            let at = a;
            const rest = this.tokens[at]?.text === "...";
            if (rest)
                at++;
            const key = this.tokens[at];
            if (!key)
                continue;
            const keyName = key.kind === "string" ? stringValue(key) : key.text;
            let local = at;
            if (this.tokens[local + 1]?.text === ":")
                local += 2;
            if (this.tokens[local]?.text === "{") {
                this.destructure(local, b, base, scope, [...keys, keyName]);
                continue;
            }
            const token = this.tokens[local];
            if (!token || token.kind !== "id" || token.text === "_")
                continue;
            const fallback = this.tokens[local + 1]?.text === "=" ? this.expression(local + 2, b, scope) : undefined;
            if (!this.lookup(token.text, scope, token.start))
                scope.symbols.push({ name: token.text, token, kind: "variable", scope,
                    expression: { tag: "project", base, keys: rest ? keys : [...keys, keyName], excluded: rest ? excluded : undefined, fallback } });
        }
    }
    expression(start, end, scope, createScopes = true) {
        if (this.expressionDepth >= 64)
            return unknown();
        this.expressionDepth++;
        try {
            return this.parseExpression(start, end, scope, createScopes);
        }
        finally {
            this.expressionDepth--;
        }
    }
    parseExpression(start, end, scope, createScopes) {
        if (start >= end || !this.tokens[start])
            return unknown();
        const tokens = this.tokens;
        const initialClose = tokens[start].text === "(" ? this.close(start, end) : -1;
        let initialArrow = initialClose + 1;
        if (initialClose >= 0 && tokens[initialArrow]?.text === ":")
            while (initialArrow < end && tokens[initialArrow].text !== "=>")
                initialArrow++;
        const isArrow = initialClose >= 0 && tokens[initialArrow]?.text === "=>";
        // Only top-level operators participate; operators in call arguments,
        // strings, object values, or nested functions cannot change the receiver.
        for (let i = start; !isArrow && i < end; i++) {
            const op = tokens[i].text;
            if (["(", "[", "{"].includes(op))
                i = this.close(i, end);
            else if (["==", "!=", "<", ">", "<=", ">=", "&&", "||", "+", "-", "*", "/", "%"].includes(op) && i > start)
                return { tag: "binary", operator: op, left: this.expression(start, i, scope, createScopes), right: this.expression(i + 1, end, scope, createScopes) };
        }
        const first = tokens[start];
        if (first.text === "await") {
            const unwrap = tokens[end - 1]?.text === "?";
            const awaited = { tag: "await", base: this.expression(start + 1, unwrap ? end - 1 : end, scope, createScopes) };
            return unwrap ? { tag: "unwrap", base: awaited } : awaited;
        }
        if (first.text === "!" || first.text === "-")
            return { tag: "type", type: primitive(first.text === "!" ? "bool" : "int") };
        if (first.text === "fn" || (first.text === "async" && tokens[start + 1]?.text === "fn")) {
            return createScopes ? { tag: "function", fn: this.functionDeclaration(start, end, scope, false).fn } : unknown();
        }
        let at = start, value = unknown();
        if (first.text === "(") {
            const close = this.close(start, end);
            let arrow = close + 1;
            if (tokens[arrow]?.text === ":") {
                while (arrow < end && tokens[arrow].text !== "=>")
                    arrow++;
            }
            if (tokens[arrow]?.text === "=>" && createScopes) {
                const body = arrow + 1, bodyClose = tokens[body]?.text === "{" ? this.close(body, end) : end;
                const child = this.child(scope, first.start, bodyClose < end ? tokens[bodyClose].end : tokens[end - 1].end);
                child.functionScope = true;
                const fn = { scope: child, parameters: [], returns: [], async: false,
                    annotation: tokens[close + 1]?.text === ":" ? tokens.slice(close + 2, arrow) : undefined };
                this.parameters(start + 1, close, child, fn);
                if (tokens[body]?.text === "{")
                    this.statements(body + 1, bodyClose, child, fn);
                else
                    fn.returns.push(this.expression(body, end, child));
                return { tag: "function", fn };
            }
            value = this.expression(start + 1, close, scope, createScopes);
            at = close + 1;
        }
        else if (first.text === "{") {
            const close = this.close(start, end), fields = [], spreads = [];
            for (const [a, b] of this.split(start + 1, close)) {
                const key = tokens[a];
                if (!key)
                    continue;
                if (key.text === "...") {
                    spreads.push(this.expression(a + 1, b, scope, createScopes));
                    continue;
                }
                const fieldToken = key.kind === "string" ? { ...key, text: stringValue(key) } : key;
                fields.push({ token: fieldToken, value: tokens[a + 1]?.text === ":" ? this.expression(a + 2, b, scope, createScopes) : { tag: "id", token: key, scope } });
            }
            value = { tag: "object", fields, spreads, scope };
            at = close + 1;
        }
        else if (first.text === "[") {
            const close = this.close(start, end);
            value = { tag: "array", elements: this.split(start + 1, close).map(([a, b]) => this.expression(a, b, scope, createScopes)) };
            at = close + 1;
        }
        else if (first.kind === "string") {
            value = { tag: "type", type: primitive("str") };
            at++;
        }
        else if (first.kind === "number") {
            value = { tag: "type", type: primitive(first.text.includes(".") ? "float" : "int") };
            at++;
        }
        else if (["true", "false", "null"].includes(first.text)) {
            value = { tag: "type", type: primitive(first.text === "null" ? "null" : "bool") };
            at++;
        }
        else if (first.kind === "id") {
            value = { tag: "id", token: first, scope };
            at++;
            if (tokens[at]?.text === "{") {
                const object = this.expression(at, Math.min(this.close(at, end) + 1, end), scope, createScopes);
                if (object.tag === "object") {
                    object.typeName = first;
                    value = object;
                }
                at = this.close(at, end) + 1;
            }
        }
        while (at < end) {
            const op = tokens[at].text;
            if (op === "." || op === "?.") {
                const name = tokens[at + 1];
                if (!name || name.kind !== "id")
                    break;
                value = { tag: "member", base: value, name: name.text };
                at += 2;
            }
            else if (op === "?") {
                value = { tag: "unwrap", base: value };
                at++;
            }
            else if (op === "(") {
                const close = this.close(at, end);
                value = { tag: "call", base: value, args: this.split(at + 1, close).map(([a, b]) => this.expression(a, b, scope, createScopes)) };
                at = close + 1;
            }
            else if (op === "[") {
                const close = this.close(at, end);
                const key = tokens[at + 1];
                value = key?.kind === "string" ? { tag: "member", base: value, name: stringValue(key) } : { tag: "index", base: value };
                at = close + 1;
            }
            else
                break;
        }
        return value;
    }
    functionType(fn, offset, depth) {
        let returns = fn.annotation?.length ? this.typeTokens(fn.annotation, fn.scope, offset, depth + 1) : fn.returns.length ? this.infer(fn.returns[0], fn.scope.end, depth + 1) : UNKNOWN;
        if (!fn.annotation?.length && fn.returns.some((expression) => !this.sameType(returns, this.infer(expression, fn.scope.end, depth + 1))))
            returns = UNKNOWN;
        if (fn.async)
            returns = { kind: "task", valueType: returns };
        return { kind: "function", name: "fn", returnType: returns, parameters: fn.parameters.map((parameter) => ({ name: parameter.name, type: this.bindingType(parameter, offset, depth + 1) })) };
    }
    typeTokens(tokens, scope, offset, depth) {
        if (!tokens.length || depth > 48)
            return UNKNOWN;
        let text = tokens.map((token) => token.text).join("");
        if (text.startsWith("&"))
            text = text.slice(1);
        if (text.startsWith("fn(") || text.startsWith("asyncfn(")) {
            const async = text.startsWith("async");
            const open = text.indexOf("(");
            let nesting = 0, close = -1;
            for (let i = open; i < text.length; i++) {
                if (text[i] === "(")
                    nesting++;
                else if (text[i] === ")" && --nesting === 0) {
                    close = i;
                    break;
                }
            }
            const parameterText = close >= 0 ? text.slice(open + 1, close) : "";
            const parts = [];
            let begin = 0;
            nesting = 0;
            for (let i = 0; i <= parameterText.length; i++) {
                if ("([{".includes(parameterText[i] ?? "\0"))
                    nesting++;
                else if (")]}".includes(parameterText[i] ?? "\0"))
                    nesting--;
                if ((parameterText[i] === "," && nesting === 0) || i === parameterText.length) {
                    if (i > begin)
                        parts.push(parameterText.slice(begin, i));
                    begin = i + 1;
                }
            }
            let returns = close >= 0 && text[close + 1] === ":" ? this.typeTokens(tokenize(text.slice(close + 2)), scope, offset, depth + 1) : UNKNOWN;
            if (async)
                returns = { kind: "task", valueType: returns };
            return { kind: "function", name: async ? "async fn" : "fn", returnType: returns,
                parameters: parts.map((part, index) => ({ name: `arg${index + 1}`, type: this.typeTokens(tokenize(part), scope, offset, depth + 1) })) };
        }
        if (text.endsWith("!"))
            return { kind: "result", valueType: this.typeTokens(tokenize(text.slice(0, -1)), scope, offset, depth + 1) };
        if (text.endsWith("|null"))
            return { ...this.typeTokens(tokenize(text.slice(0, -5)), scope, offset, depth + 1), nullable: true };
        if (text.startsWith("null|"))
            return { ...this.typeTokens(tokenize(text.slice(5)), scope, offset, depth + 1), nullable: true };
        if (text.startsWith("[") && text.endsWith("]"))
            return { ...this.namedType(text), kind: "array", name: text, elementType: this.typeTokens(tokenize(text.slice(1, -1)), scope, offset, depth + 1) };
        if (["int", "float", "bool", "str", "null"].includes(text))
            return this.namedType(text) ?? primitive(text);
        const binding = this.lookup(text, scope, offset);
        if (binding && (binding.kind === "struct" || binding.kind === "enum"))
            return this.bindingType(binding, offset, depth + 1);
        const [moduleName, ...members] = text.split(".");
        const imported = this.lookup(moduleName, scope, offset);
        if (imported?.kind === "module") {
            let type = this.bindingType(imported, offset, depth + 1);
            for (const member of members) {
                type = type.members?.get(member)?.type
                    ?? (type.qualifiedName ? this.external(`${type.qualifiedName}.${member}`) : undefined)
                    ?? UNKNOWN;
            }
            // Imported namespaces are not types. A public struct/enum resolved from
            // its source has a concrete name and field map, including an empty map.
            if (type.kind === "object" && type.name && type.members && type.name !== type.qualifiedName)
                return type;
        }
        return this.namedType(text) ?? { kind: "unknown", name: text };
    }
    infer(expression, offset, depth) {
        if (depth > 48)
            return UNKNOWN;
        switch (expression.tag) {
            case "unknown": return UNKNOWN;
            case "type": return this.decorate(expression.type);
            case "id": {
                const binding = this.lookup(expression.token.text, expression.scope, Math.min(offset, expression.token.start));
                return binding ? this.bindingType(binding, offset, depth + 1) : this.external(expression.token.text) ?? UNKNOWN;
            }
            case "function": return this.functionType(expression.fn, offset, depth + 1);
            case "array": {
                const elementType = expression.elements[0] ? this.infer(expression.elements[0], offset, depth + 1) : UNKNOWN;
                return { ...this.namedType(`[${elementType.name ?? "unknown"}]`), kind: "array", name: `[${elementType.name ?? "unknown"}]`, elementType };
            }
            case "object": {
                const declared = expression.typeName && this.lookup(expression.typeName.text, expression.scope, offset);
                if (declared?.kind === "struct")
                    return this.bindingType(declared, offset, depth + 1);
                const members = new Map();
                for (const spread of expression.spreads)
                    for (const [name, member] of this.infer(spread, offset, depth + 1).members ?? [])
                        members.set(name, member);
                for (const field of expression.fields)
                    members.set(field.token.text, { name: field.token.text, start: field.token.start, end: field.token.end, kind: "field", type: this.infer(field.value, offset, depth + 1) });
                return { kind: "object", members };
            }
            case "member": {
                const base = this.decorate(this.infer(expression.base, offset, depth + 1));
                const known = base.members?.get(expression.name)?.type;
                if (known)
                    return base.qualifiedName && known.kind === "function" ? { ...known, qualifiedName: `${base.qualifiedName}.${expression.name}` } : known;
                if (base.name === "StringMap")
                    return base.valueType ?? UNKNOWN;
                if (base.qualifiedName)
                    return this.external(`${base.qualifiedName}.${expression.name}`) ?? { kind: "unknown", qualifiedName: `${base.qualifiedName}.${expression.name}` };
                return UNKNOWN;
            }
            case "index": return this.infer(expression.base, offset, depth + 1).elementType ?? UNKNOWN;
            case "call": {
                if (expression.base.tag === "id" && expression.base.token.text === "ok" && !this.lookup("ok", expression.base.scope, offset))
                    return { kind: "result", valueType: expression.args[0] ? this.infer(expression.args[0], offset, depth + 1) : UNKNOWN };
                if (expression.base.tag === "member" && expression.base.name === "clone" && expression.args.length === 0)
                    return this.infer(expression.base.base, offset, depth + 1);
                const base = this.infer(expression.base, offset, depth + 1);
                const external = base.qualifiedName ? this.external(base.qualifiedName, expression.args.length) : undefined;
                return (external ?? base).returnType ?? UNKNOWN;
            }
            case "await": {
                const base = this.infer(expression.base, offset, depth + 1);
                return base.kind === "task" ? base.valueType ?? UNKNOWN : UNKNOWN;
            }
            case "unwrap": {
                const base = this.infer(expression.base, offset, depth + 1);
                return base.kind === "result" ? base.valueType ?? UNKNOWN : base;
            }
            case "project": {
                let type = this.infer(expression.base, offset, depth + 1);
                for (const key of expression.keys)
                    type = type.members?.get(key)?.type ?? UNKNOWN;
                if (expression.excluded)
                    return { kind: "object", members: new Map([...(type.members ?? [])].filter(([name]) => !expression.excluded.includes(name))) };
                if ((type.kind === "unknown" || type.name === "null") && expression.fallback)
                    return this.infer(expression.fallback, offset, depth + 1);
                return type;
            }
            case "binary": {
                if (["==", "!=", "<", ">", "<=", ">=", "&&", "||"].includes(expression.operator))
                    return this.decorate(primitive("bool"));
                const left = this.infer(expression.left, offset, depth + 1), right = this.infer(expression.right, offset, depth + 1);
                return left.kind === "primitive" && left.name === right.name ? left : UNKNOWN;
            }
        }
    }
    decorate(type) {
        return !type.members && type.name ? this.namedType(type.name) ?? type : type;
    }
    external(name, argumentCount, allowModule = false) {
        const override = this.options.resolveExternal?.(name, argumentCount);
        if (override)
            return { ...override, qualifiedName: override.qualifiedName ?? name };
        const signature = (0, metadataModel_1.stdSignature)(name, argumentCount);
        if (signature)
            return { ...this.signatureType(signature), qualifiedName: name };
        const members = allowModule ? (0, metadataModel_1.moduleMembers)(name) : [];
        return members.length ? { kind: "object", name, qualifiedName: name, members: this.metadataMembers(members) } : undefined;
    }
    signatureType(signature) {
        return { kind: "function", name: signature.label, parameters: signature.parameters.map((parameter) => ({
                name: parameter.name, type: this.typeTokens(tokenize(parameter.type), this.root, 0, 0), optional: parameter.optional,
            })), returnType: this.typeTokens(tokenize(signature.returns), this.root, 0, 0) };
    }
    namedType(name) {
        const override = this.options.resolveExternalType?.(name);
        if (override)
            return override;
        // These SDK maps accept arbitrary string keys. Do not invent their names.
        if (name === "StringMap")
            return { kind: "object", name, valueType: this.namedType("str") ?? primitive("str") };
        const fields = (0, metadataModel_1.typeMembers)(name);
        if (!fields.length)
            return undefined;
        return { kind: ["int", "float", "bool", "str", "null"].includes(name) ? "primitive" : name.startsWith("[") ? "array" : "object", name,
            members: this.metadataMembers(fields) };
    }
    metadataMembers(fields) {
        const model = this;
        return new Map(fields.map((field) => [field.name, {
                name: field.name, start: -1, end: -1, kind: field.kind === "method" ? "function" : "field",
                get type() { return field.signature ? model.signatureType(field.signature) : model.typeTokens(tokenize(field.type), model.root, 0, 0); },
            }]));
    }
    errorType() {
        return this.namedType("Error") ?? { kind: "object", name: "Error", members: new Map(["domain", "code", "message"].map((name) => [name, { name, start: -1, end: -1, kind: "field", type: primitive("str") }])) };
    }
}
exports.SemanticModel = SemanticModel;
function analyzeSource(source, options = {}) {
    return new SemanticModel(source, options);
}
