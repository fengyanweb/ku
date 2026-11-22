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
exports.KuToolTerminal = void 0;
exports.redactToolOutput = redactToolOutput;
exports.closeKuTools = closeKuTools;
const cp = __importStar(require("child_process"));
const vscode = __importStar(require("vscode"));
const activeTools = new Set();
function redactToolOutput(text) {
    return text.replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/@]+@/gi, "$1<redacted>@")
        .replace(/\b(password|passwd|token|secret)\s*=\s*(?:"[^"]*"|'[^']*'|[^\s;]+)/gi, "$1=<redacted>");
}
function closeKuTools() {
    for (const tool of [...activeTools])
        tool.close();
}
// Build/install tools tee their complete output into Ku Output. Run uses VS
// Code's native process terminal, preserving a real terminal and interactive stdin.
class KuToolTerminal {
    constructor(executable, args, cwd, log) {
        this.executable = executable;
        this.args = args;
        this.cwd = cwd;
        this.log = log;
        this.write = new vscode.EventEmitter();
        this.exit = new vscode.EventEmitter();
        this.onDidWrite = this.write.event;
        this.onDidClose = this.exit.event;
        this.closed = false;
        this.buffers = { stdout: { text: "", discard: false }, stderr: { text: "", discard: false } };
    }
    open() {
        if (this.closed)
            return;
        if (vscode.workspace.isTrusted === false) {
            this.append("Workspace trust is required to run Ku tools.\n", "stderr");
            this.closed = true;
            this.exit.fire(1);
            return;
        }
        activeTools.add(this);
        try {
            this.child = cp.spawn(this.executable, this.args, {
                cwd: this.cwd, shell: false, windowsHide: true, detached: process.platform !== "win32",
            });
            this.child.stdout?.setEncoding("utf8");
            this.child.stderr?.setEncoding("utf8");
            this.child.stdout?.on("data", (data) => this.append(data, "stdout"));
            this.child.stderr?.on("data", (data) => this.append(data, "stderr"));
            this.child.stdin?.on("error", () => { });
            this.child.once("error", (error) => this.append(`${error.message}\n`, "stderr"));
            this.child.once("close", (code) => {
                if (this.closed)
                    return;
                this.flush();
                this.append(`\n[Ku tool exited ${code ?? 1}]\n`, "stdout");
                this.closed = true;
                activeTools.delete(this);
                this.exit.fire(code ?? 1);
            });
        }
        catch (error) {
            this.append(`${String(error)}\n`, "stderr");
            this.closed = true;
            activeTools.delete(this);
            this.exit.fire(1);
        }
    }
    handleInput(data) {
        if (data === "\x03") {
            this.close();
            return;
        }
        const input = this.child?.stdin;
        if (!this.closed && input?.writable && !input.destroyed)
            input.write(data.replace(/\r/g, "\n"), () => { });
    }
    close() {
        if (this.closed)
            return;
        this.flush();
        this.closed = true;
        activeTools.delete(this);
        const pid = this.child?.pid;
        if (pid && this.child?.exitCode === null) {
            if (process.platform === "win32") {
                // This is the PID owned by this terminal, never a name-based/global kill.
                const killer = cp.spawn("taskkill.exe", ["/pid", String(pid), "/t", "/f"], { windowsHide: true, stdio: "ignore" });
                killer.on("error", () => this.child?.kill());
                killer.on("close", (code) => { if (code !== 0)
                    this.child?.kill(); });
            }
            else {
                try {
                    process.kill(-pid, "SIGKILL");
                }
                catch {
                    this.child?.kill("SIGKILL");
                }
            }
        }
        this.exit.fire(130);
    }
    append(text, stream) {
        if (this.closed)
            return;
        this.write.fire(text.replace(/\r?\n/g, "\r\n"));
        const buffer = this.buffers[stream];
        let offset = 0;
        while (offset < text.length) {
            const newline = text.indexOf("\n", offset);
            const end = newline < 0 ? text.length : newline + 1;
            if (!buffer.discard) {
                const part = text.slice(offset, end);
                if (buffer.text.length + part.length > 65536) {
                    this.log("[Long tool output line redacted]\n");
                    buffer.text = "";
                    buffer.discard = true;
                }
                else
                    buffer.text += part;
            }
            if (newline >= 0) {
                if (!buffer.discard)
                    this.log(redactToolOutput(buffer.text));
                buffer.text = "";
                buffer.discard = false;
            }
            offset = end;
        }
    }
    flush() {
        for (const buffer of Object.values(this.buffers)) {
            if (buffer.text && !buffer.discard)
                this.log(redactToolOutput(buffer.text));
            buffer.text = "";
        }
    }
}
exports.KuToolTerminal = KuToolTerminal;
