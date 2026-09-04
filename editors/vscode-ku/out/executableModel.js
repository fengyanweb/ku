"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.execFileExitCode = execFileExitCode;
exports.firstWorkingExecutable = firstWorkingExecutable;
function execFileExitCode(error) {
    if (error === null) {
        return 0;
    }
    return typeof error.code === "number" && error.code !== 0 ? error.code : 1;
}
async function firstWorkingExecutable(candidates, probe) {
    for (const candidate of [...new Set(candidates)]) {
        if ((await probe(candidate)).code === 0) {
            return candidate;
        }
    }
    return undefined;
}
