export interface ExecutableProbeResult {
  code: number;
}

export function execFileExitCode(error: { code?: unknown } | null): number {
  if (error === null) {
    return 0;
  }
  return typeof error.code === "number" && error.code !== 0 ? error.code : 1;
}

export async function firstWorkingExecutable(
  candidates: readonly string[],
  probe: (candidate: string) => Promise<ExecutableProbeResult>,
): Promise<string | undefined> {
  for (const candidate of [...new Set(candidates)]) {
    if ((await probe(candidate)).code === 0) {
      return candidate;
    }
  }
  return undefined;
}
