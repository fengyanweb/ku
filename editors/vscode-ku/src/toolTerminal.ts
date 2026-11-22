import * as cp from "child_process";
import * as vscode from "vscode";

const activeTools = new Set<KuToolTerminal>();

export function redactToolOutput(text: string): string {
  return text.replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/@]+@/gi, "$1<redacted>@")
    .replace(/\b(password|passwd|token|secret)\s*=\s*(?:"[^"]*"|'[^']*'|[^\s;]+)/gi, "$1=<redacted>");
}

export function closeKuTools() {
  for (const tool of [...activeTools]) tool.close();
}

// Build/install tools tee their complete output into Ku Output. Run uses VS
// Code's native process terminal, preserving a real terminal and interactive stdin.
export class KuToolTerminal implements vscode.Pseudoterminal {
  private readonly write = new vscode.EventEmitter<string>();
  private readonly exit = new vscode.EventEmitter<number>();
  readonly onDidWrite = this.write.event;
  readonly onDidClose = this.exit.event;
  private child?: cp.ChildProcess;
  private closed = false;
  private readonly buffers = { stdout: { text: "", discard: false }, stderr: { text: "", discard: false } };

  constructor(private readonly executable: string, private readonly args: string[],
    private readonly cwd: string, private readonly log: (text: string) => void) {}

  open() {
    if (this.closed) return;
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
      this.child.stdout?.on("data", (data: string) => this.append(data, "stdout"));
      this.child.stderr?.on("data", (data: string) => this.append(data, "stderr"));
      this.child.stdin?.on("error", () => { /* The tool can stop reading before its terminal closes. */ });
      this.child.once("error", (error) => this.append(`${error.message}\n`, "stderr"));
      this.child.once("close", (code) => {
        if (this.closed) return;
        this.flush();
        this.append(`\n[Ku tool exited ${code ?? 1}]\n`, "stdout");
        this.closed = true;
        activeTools.delete(this);
        this.exit.fire(code ?? 1);
      });
    } catch (error) {
      this.append(`${String(error)}\n`, "stderr");
      this.closed = true;
      activeTools.delete(this);
      this.exit.fire(1);
    }
  }

  handleInput(data: string) {
    if (data === "\x03") { this.close(); return; }
    const input = this.child?.stdin;
    if (!this.closed && input?.writable && !input.destroyed) input.write(data.replace(/\r/g, "\n"), () => {});
  }

  close() {
    if (this.closed) return;
    this.flush();
    this.closed = true;
    activeTools.delete(this);
    const pid = this.child?.pid;
    if (pid && this.child?.exitCode === null) {
      if (process.platform === "win32") {
        // This is the PID owned by this terminal, never a name-based/global kill.
        const killer = cp.spawn("taskkill.exe", ["/pid", String(pid), "/t", "/f"], { windowsHide: true, stdio: "ignore" });
        killer.on("error", () => this.child?.kill());
        killer.on("close", (code) => { if (code !== 0) this.child?.kill(); });
      } else {
        try { process.kill(-pid, "SIGKILL"); } catch { this.child?.kill("SIGKILL"); }
      }
    }
    this.exit.fire(130);
  }

  private append(text: string, stream: "stdout" | "stderr") {
    if (this.closed) return;
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
        } else buffer.text += part;
      }
      if (newline >= 0) {
        if (!buffer.discard) this.log(redactToolOutput(buffer.text));
        buffer.text = "";
        buffer.discard = false;
      }
      offset = end;
    }
  }

  private flush() {
    for (const buffer of Object.values(this.buffers)) {
      if (buffer.text && !buffer.discard) this.log(redactToolOutput(buffer.text));
      buffer.text = "";
    }
  }
}
