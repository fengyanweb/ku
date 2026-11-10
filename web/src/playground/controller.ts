import { createEditor } from './editor';
import { examples } from './examples';
import type { PlaygroundMessages } from './messages';
import {
  LIMITS,
  PROTOCOL_VERSION,
  byteLength,
  diagnosticPosition,
  validateInput,
  type VirtualFile,
  type EngineInfo,
  type EngineResult,
  type Diagnostic,
  type WorkerResponse,
} from './protocol';

const DRAFT_KEY = 'ku-playground-draft-v1';
// A bfcache page can return after pagehide disposed its editor. Keep the last
// draft without requiring localStorage (which may be blocked or full).
const restoredDrafts = new WeakMap<
  HTMLElement,
  {
    source: string;
    files: VirtualFile[];
    baseline: string;
    baselineFiles: string;
    selected: string;
  }
>();

export function mount(root: HTMLElement, t: PlaygroundMessages['status']) {
  const listeners = new AbortController();
  const find = <T extends HTMLElement>(selector: string) =>
    root.querySelector<T>(selector)!;
  const status = find('[data-status]');
  const engineLabel = find('[data-engine]');
  const engineDetails = find('[data-engine-details]');
  const stale = find('[data-stale]');
  const notices = find('[data-notice]');
  const diagnosticsPanel = find('[data-panel="diagnostics"]');
  const runButton = find<HTMLButtonElement>('[data-action="run"]');
  const checkButton = find<HTMLButtonElement>('[data-action="check"]');
  const stopButton = find<HTMLButtonElement>('[data-action="stop"]');
  const sourcePreview = find<HTMLTextAreaElement>('[data-source-preview]');
  const selector = find<HTMLSelectElement>('[data-examples]');
  const fileInput = find<HTMLInputElement>('[data-file-input]');
  const confirmationDialog = find<HTMLDialogElement>('[data-confirm-dialog]');
  const restored = restoredDrafts.get(root);
  restoredDrafts.delete(root);
  let files: VirtualFile[] = restored?.files ?? [];
  let source = restored?.source ?? sourcePreview.value;
  let baseline = restored?.baseline ?? source;
  let baselineFiles = restored?.baselineFiles ?? '[]';
  let selected = restored?.selected ?? examples[0].id;
  selector.value = selected;
  let resultSource: string | undefined;
  let resultFiles = '';
  let resultDiagnostics: Diagnostic[] = [];
  let highlightedModule:
    { element: HTMLPreElement; source: string } | undefined;
  const clearModuleHighlight = () => {
    if (highlightedModule)
      highlightedModule.element.textContent = highlightedModule.source;
    highlightedModule = undefined;
  };
  let worker: Worker | undefined;
  let generation = 0;
  let jobId = 0;
  let deadline: ReturnType<typeof setTimeout> | undefined;
  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  let disposed = false;
  let active = false;
  let importing = false;
  let fileReadEpoch = 0;
  const downloads = new Map<string, ReturnType<typeof setTimeout>>();
  let action: 'run' | 'check' = 'run';
  let initializationMs = 0;
  let storageReported = false;
  let replacementEpoch = 0;
  let confirmation:
    | {
        resolve: (accepted: boolean) => void;
        previousFocus: HTMLElement | null;
      }
    | undefined;
  const notify = (message: string) => {
    notices.textContent = message;
  };
  const errorMessage = (error: unknown) => {
    const key = error instanceof Error ? error.message : '';
    return key in t ? t[key as keyof typeof t] : t.failure;
  };
  const showError = (error: unknown) => notify(errorMessage(error));
  const isChanged = () =>
    source !== baseline || JSON.stringify(files) !== baselineFiles;
  const finishConfirmation = (accepted: boolean) => {
    const pending = confirmation;
    confirmation = undefined;
    if (confirmationDialog.open)
      confirmationDialog.close(accepted ? 'confirm' : 'cancel');
    if (!pending) return;
    if (!accepted) {
      selector.value = selected;
      clearModuleHighlight();
    }
    pending.resolve(accepted && !disposed);
    if (!disposed && pending.previousFocus?.isConnected)
      pending.previousFocus.focus();
  };
  const askConfirmation = (message: string) => {
    finishConfirmation(false);
    if (disposed) return Promise.resolve(false);
    find('[data-confirm-message]').textContent = message;
    return new Promise<boolean>((resolve) => {
      confirmation = {
        resolve,
        previousFocus:
          document.activeElement instanceof HTMLElement
            ? document.activeElement
            : null,
      };
      try {
        confirmationDialog.returnValue = '';
        confirmationDialog.showModal();
        find<HTMLButtonElement>('[data-confirm-cancel]').focus();
      } catch {
        finishConfirmation(false);
      }
    });
  };
  const allowReplacement = async () =>
    !isChanged() || (await askConfirmation(t.replace));
  find('[data-confirm-cancel]').addEventListener(
    'click',
    () => finishConfirmation(false),
    { signal: listeners.signal },
  );
  find('[data-confirm-accept]').addEventListener(
    'click',
    () => finishConfirmation(true),
    { signal: listeners.signal },
  );
  confirmationDialog.addEventListener(
    'cancel',
    (event) => {
      event.preventDefault();
      finishConfirmation(false);
    },
    { signal: listeners.signal },
  );
  confirmationDialog.addEventListener(
    'close',
    () => {
      if (!confirmationDialog.open) finishConfirmation(false);
    },
    { signal: listeners.signal },
  );
  const persist = () => {
    clearTimeout(saveTimer);
    try {
      validateInput(source, files);
      localStorage.setItem(DRAFT_KEY, JSON.stringify({ v: 1, source, files }));
    } catch (error) {
      if (
        error instanceof Error &&
        ['source_limit', 'file_limit', 'file_path'].includes(error.message)
      )
        return;
      if (!storageReported) {
        notify(t.storage);
        storageReported = true;
      }
    }
  };
  const currentMatchesResult = () =>
    source === resultSource && JSON.stringify(files) === resultFiles;
  const updateStale = () => {
    stale.hidden = resultSource === undefined || currentMatchesResult();
    stale.textContent = t.stale;
  };
  const editor = createEditor(
    find('[data-editor]'),
    source,
    (value) => {
      source = value;
      clearModuleHighlight();
      find('[data-share-container]').hidden = true;
      find('[data-source-size]').textContent =
        `${byteLength(source)} / 65536 B`;
      updateStale();
      queueMicrotask(() => {
        if (!disposed && !currentMatchesResult())
          editor.diagnostics(source, []);
      });
      clearTimeout(saveTimer);
      saveTimer = setTimeout(persist, 300);
    },
    (next) => {
      if (!active) execute(next);
    },
    t.editor,
    () => notify(t.source_limit),
  );
  sourcePreview.hidden = true;
  find('[data-source-size]').textContent = `${byteLength(source)} / 65536 B`;
  root
    .querySelectorAll<HTMLInputElement | HTMLSelectElement>(
      '[data-needs-editor]',
    )
    .forEach((element) => {
      element.disabled = false;
    });
  root.dataset.active = 'true';
  root
    .querySelectorAll<HTMLButtonElement>('button[data-action]')
    .forEach((button) => {
      button.disabled = button.dataset.action === 'stop';
    });
  status.textContent = t.idle;
  const renderFiles = () => {
    clearModuleHighlight();
    const panel = find('[data-files]');
    panel.replaceChildren();
    if (!files.length) {
      panel.textContent = t.modules;
      return;
    }
    for (const file of files) {
      const item = document.createElement('details');
      item.dataset.filePath = file.path;
      const label = document.createElement('summary');
      label.textContent = file.path;
      const code = document.createElement('pre');
      code.textContent = file.source;
      item.append(label, code);
      panel.append(item);
    }
  };
  const load = (next: { source: string; files?: VirtualFile[] }) => {
    validateInput(next.source, next.files ?? []);
    files = next.files?.map((file) => ({ ...file })) ?? [];
    find('[data-share-container]').hidden = true;
    source = next.source;
    editor.replace(source);
    editor.diagnostics(source, []);
    renderFiles();
    updateStale();
    persist();
  };
  try {
    const saved = restored ? null : localStorage.getItem(DRAFT_KEY);
    if (saved && byteLength(saved) <= 2 * 1024 * 1024) {
      const draft = JSON.parse(saved);
      if (
        draft?.v === 1 &&
        typeof draft.source === 'string' &&
        Array.isArray(draft.files)
      )
        load(draft);
    }
  } catch {
    notify(t.storage);
    storageReported = true;
  }
  renderFiles();

  const stop = (message?: string) => {
    clearModuleHighlight();
    clearTimeout(deadline);
    deadline = undefined;
    if (worker) {
      worker.onmessage = null;
      worker.onerror = null;
      worker.onmessageerror = null;
      worker.terminate();
      worker = undefined;
    }
    generation++;
    fileReadEpoch++;
    importing = false;
    fileInput.disabled = false;
    active = false;
    runButton.disabled = false;
    checkButton.disabled = false;
    stopButton.disabled = true;
    if (message) status.textContent = message;
  };
  const showTab = (tab: string) => {
    root.querySelectorAll<HTMLElement>('[data-panel]').forEach((panel) => {
      panel.hidden = panel.dataset.panel !== tab;
    });
    root.querySelectorAll<HTMLButtonElement>('[data-tab]').forEach((button) => {
      button.setAttribute('aria-selected', String(button.dataset.tab === tab));
      button.tabIndex = button.dataset.tab === tab ? 0 : -1;
    });
  };
  const showEngine = (info: EngineInfo) => {
    engineLabel.textContent = `Ku ${info.version} · ${t.preview} · ${info.buildId.slice(0, 12)}`;
    const details = [
      `wasm-interpreter · ${info.releaseTag} · ${info.releaseCommit}`,
      `build ${info.buildId}${info.dirty ? ` · ${t.dirty}` : ''}`,
      `${t.capabilities}: ${info.capabilities.join(', ')}`,
      `${t.budgets}: ${JSON.stringify(info.budgets)}`,
    ];
    engineDetails.textContent = details.join('\n');
  };
  const renderResult = (
    result: EngineResult,
    snapshot: string,
    snapshotFiles: string,
    elapsed: number,
  ) => {
    resultSource = snapshot;
    resultFiles = snapshotFiles;
    resultDiagnostics = result.diagnostics;
    updateStale();
    find('[data-stdout]').textContent = result.stdout || t.noOutput;
    find('[data-stderr]').textContent = result.stderr || t.noOutput;
    diagnosticsPanel.replaceChildren();
    if (!result.diagnostics.length)
      diagnosticsPanel.textContent = t.noDiagnostics;
    for (const [index, diagnostic] of result.diagnostics.entries()) {
      const item = document.createElement('button');
      item.type = 'button';
      item.className = 'pg-diagnostic';
      item.dataset.diagnostic = String(index);
      const where = diagnostic.line
        ? `${diagnostic.file ?? 'main.ku'}:${diagnostic.line}:${diagnostic.column ?? 1} — `
        : '';
      item.textContent = where + diagnostic.message;
      diagnosticsPanel.append(item);
    }
    if (currentMatchesResult())
      editor.diagnostics(snapshot, result.diagnostics);
    const label =
      result.status === 'success' && action === 'check'
        ? t.checked
        : t[result.status];
    status.textContent = `${label} · ${elapsed.toFixed(1)} ms · ${t.init} ${initializationMs.toFixed(0)} ms`;
    if (result.diagnostics.length) showTab('diagnostics');
    else if (result.stderr) showTab('stderr');
    else showTab('stdout');
  };

  function execute(next: 'run' | 'check') {
    if (disposed || active || importing || confirmationDialog.open) return;
    if (document.hidden) {
      status.textContent = t.hidden;
      return;
    }
    try {
      validateInput(source, files);
    } catch (error) {
      status.textContent = errorMessage(error);
      return;
    }
    stop();
    active = true;
    action = next;
    runButton.disabled = true;
    checkButton.disabled = true;
    stopButton.disabled = false;
    status.textContent = t.loading;
    notify('');
    editor.diagnostics(source, []);
    const snapshot = source;
    const snapshotFiles = JSON.stringify(files);
    const thisGeneration = generation;
    const thisJob = ++jobId;
    let engineReady = false;
    let current: Worker;
    try {
      current = new Worker(new URL('./ku.worker.ts', import.meta.url), {
        type: 'module',
        name: 'ku-playground',
      });
    } catch {
      stop(t.initialization_error);
      return;
    }
    worker = current;
    deadline = setTimeout(() => {
      if (worker === current && generation === thisGeneration)
        stop(t.initializationTimeout);
    }, LIMITS.initializationMs);
    current.onerror = (event) => {
      event.preventDefault();
      if (worker === current && generation === thisGeneration)
        stop(engineReady ? t.wasm_trap : t.initialization_error);
    };
    current.onmessageerror = () => {
      if (worker === current && generation === thisGeneration)
        stop(t.protocol_error);
    };
    current.onmessage = (event: MessageEvent<WorkerResponse>) => {
      const message = event.data;
      if (
        disposed ||
        worker !== current ||
        generation !== thisGeneration ||
        message?.protocolVersion !== PROTOCOL_VERSION ||
        message.jobId !== thisJob ||
        message.generation !== thisGeneration
      )
        return;
      if (message.type === 'ready') {
        if (engineReady) {
          stop(t.protocol_error);
          return;
        }
        engineReady = true;
        showEngine(message.info);
        initializationMs = message.initializationMs;
        clearTimeout(deadline);
        status.textContent = next === 'run' ? t.running : t.checking;
        deadline = setTimeout(() => {
          if (worker === current && generation === thisGeneration)
            stop(t.timedOut);
        }, LIMITS.executionMs);
      } else if (message.type === 'result') {
        if (!engineReady) {
          stop(t.protocol_error);
          return;
        }
        stop();
        renderResult(
          message.result,
          snapshot,
          snapshotFiles,
          message.executionMs,
        );
      } else if (message.type === 'failure') stop(t[message.code]);
    };
    current.postMessage({
      protocolVersion: PROTOCOL_VERSION,
      jobId: thisJob,
      generation: thisGeneration,
      action: next,
      source: snapshot,
      files: files.map((file) => ({ ...file })),
    });
  }

  root.addEventListener(
    'click',
    async (event) => {
      const button = (event.target as HTMLElement).closest<HTMLButtonElement>(
        'button[data-action]',
      );
      if (!button || button.disabled) return;
      try {
        switch (button.dataset.action) {
          case 'run':
            execute('run');
            break;
          case 'check':
            execute('check');
            break;
          case 'stop':
            stop(t.stopped);
            break;
          case 'clear':
            clearModuleHighlight();
            find('[data-stdout]').textContent = '';
            find('[data-stderr]').textContent = '';
            diagnosticsPanel.replaceChildren();
            resultDiagnostics = [];
            editor.diagnostics(source, []);
            resultSource = undefined;
            updateStale();
            break;
          case 'reset': {
            const request = ++replacementEpoch;
            if (
              (await allowReplacement()) &&
              !disposed &&
              request === replacementEpoch
            ) {
              stop();
              load(examples.find((example) => example.id === selected)!);
              baseline = source;
              baselineFiles = JSON.stringify(files);
            }
            break;
          }
          case 'copy':
            try {
              await navigator.clipboard.writeText(source);
              if (!disposed) notify(t.copied);
            } catch {
              if (!disposed) notify(t.clipboard);
            }
            break;
          case 'download': {
            const blob = new Blob([source], {
              type: 'text/plain;charset=utf-8',
            });
            const url = URL.createObjectURL(blob);
            const anchor = document.createElement('a');
            anchor.href = url;
            anchor.download = 'main.ku';
            anchor.click();
            const timer = setTimeout(() => {
              URL.revokeObjectURL(url);
              downloads.delete(url);
            }, 1000);
            downloads.set(url, timer);
            break;
          }
          case 'share': {
            validateInput(source, files);
            const data = JSON.stringify({ v: 1, source, files });
            if (byteLength(data) > LIMITS.shareBytes) {
              notify(t.shareLimit);
              break;
            }
            const encoded = btoa(
              Array.from(new TextEncoder().encode(data), (byte) =>
                String.fromCharCode(byte),
              ).join(''),
            )
              .replaceAll('+', '-')
              .replaceAll('/', '_')
              .replace(/=+$/, '');
            const url = new URL(location.href);
            url.hash = `ku1=${encoded}`;
            find('[data-share-container]').hidden = false;
            const shareInput = find<HTMLInputElement>('[data-share-link]');
            shareInput.value = url.href;
            try {
              await navigator.clipboard.writeText(url.href);
              if (!disposed) notify(t.shared);
            } catch {
              if (disposed) return;
              notify(t.shareFallback);
              shareInput.focus();
              shareInput.select();
            }
            break;
          }
        }
      } catch (error) {
        showError(error);
      }
    },
    { signal: listeners.signal },
  );
  root.addEventListener(
    'keydown',
    (event) => {
      if (
        event.defaultPrevented ||
        !(event.metaKey || event.ctrlKey) ||
        event.key !== 'Enter'
      )
        return;
      event.preventDefault();
      execute(event.shiftKey ? 'check' : 'run');
    },
    { signal: listeners.signal },
  );
  diagnosticsPanel.addEventListener(
    'click',
    (event) => {
      const button = (event.target as HTMLElement).closest<HTMLElement>(
        '[data-diagnostic]',
      );
      const diagnostic = button
        ? resultDiagnostics[Number(button.dataset.diagnostic)]
        : undefined;
      if (!diagnostic || resultSource === undefined) return;
      if (!currentMatchesResult()) {
        clearModuleHighlight();
        notify(t.stale);
        return;
      }
      clearModuleHighlight();
      if (!diagnostic.file || diagnostic.file === 'main.ku')
        editor.jump(resultSource, diagnostic);
      else {
        const path = diagnostic.file.replace(/^(?:\.\/)+/, '');
        const file = files.find((entry) => entry.path === path);
        const module = Array.from(
          root.querySelectorAll<HTMLDetailsElement>('[data-file-path]'),
        ).find((item) => item.dataset.filePath === path);
        const code = module?.querySelector('pre');
        if (module && file && code) {
          const container = module.closest<HTMLDetailsElement>('.pg-modules');
          if (container) container.open = true;
          module.open = true;
          const position = diagnosticPosition(file.source, diagnostic);
          const start =
            position === 0
              ? 0
              : file.source.lastIndexOf('\n', position - 1) + 1;
          const newline = file.source.indexOf('\n', position);
          const end = newline < 0 ? file.source.length : newline;
          const mark = document.createElement('mark');
          mark.tabIndex = -1;
          mark.textContent = file.source.slice(start, end);
          mark.title = `${path}:${diagnostic.line ?? 1}`;
          code.replaceChildren(
            document.createTextNode(file.source.slice(0, start)),
            mark,
            document.createTextNode(file.source.slice(end)),
          );
          highlightedModule = { element: code, source: file.source };
          mark.scrollIntoView({ block: 'center' });
          mark.focus({ preventScroll: true });
        }
      }
    },
    { signal: listeners.signal },
  );
  selector.addEventListener(
    'change',
    async () => {
      const request = ++replacementEpoch;
      const example = examples.find((example) => example.id === selector.value);
      if (!example) return;
      const accepted = await allowReplacement();
      if (disposed || request !== replacementEpoch) return;
      if (!accepted) {
        selector.value = selected;
        return;
      }
      stop();
      selected = example.id;
      selector.value = selected;
      load(example);
      baseline = source;
      baselineFiles = JSON.stringify(files);
    },
    { signal: listeners.signal },
  );
  root.querySelectorAll<HTMLButtonElement>('[data-tab]').forEach((button) => {
    button.addEventListener('click', () => showTab(button.dataset.tab!), {
      signal: listeners.signal,
    });
    button.addEventListener(
      'keydown',
      (event) => {
        if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key))
          return;
        const tabs = Array.from(
          root.querySelectorAll<HTMLButtonElement>('[data-tab]'),
        );
        let index = tabs.indexOf(button);
        index =
          event.key === 'Home'
            ? 0
            : event.key === 'End'
              ? tabs.length - 1
              : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) %
                tabs.length;
        event.preventDefault();
        showTab(tabs[index].dataset.tab!);
        tabs[index].focus();
      },
      { signal: listeners.signal },
    );
  });
  fileInput.addEventListener(
    'change',
    async (event) => {
      const input = event.target as HTMLInputElement;
      const picked = Array.from(input.files ?? []);
      input.value = '';
      if (!picked.length) return;
      const request = ++replacementEpoch;
      if (
        !(await askConfirmation(t.replace)) ||
        disposed ||
        request !== replacementEpoch
      )
        return;
      stop();
      const thisRead = fileReadEpoch;
      importing = true;
      runButton.disabled = true;
      checkButton.disabled = true;
      stopButton.disabled = false;
      fileInput.disabled = true;
      status.textContent = t.importing;
      try {
        if (
          picked.length > LIMITS.fileCount ||
          picked.reduce((sum, file) => sum + file.size, 0) > LIMITS.fileBytes
        )
          throw new Error('file_limit');
        const imported = await Promise.all(
          picked.map(async (file) => ({
            path: file.name,
            source: new TextDecoder('utf-8', { fatal: true }).decode(
              await file.arrayBuffer(),
            ),
          })),
        );
        if (disposed || thisRead !== fileReadEpoch) return;
        validateInput(source, imported);
        stop();
        files = imported;
        find('[data-share-container]').hidden = true;
        renderFiles();
        updateStale();
        editor.diagnostics(source, []);
        persist();
        notify(t.imported);
        status.textContent = t.idle;
      } catch (error) {
        if (disposed || thisRead !== fileReadEpoch) return;
        stop(t.idle);
        showError(error);
      }
    },
    { signal: listeners.signal },
  );

  const readShare = async () => {
    const request = ++replacementEpoch;
    const fragment = location.hash;
    finishConfirmation(false);
    if (!location.hash.startsWith('#ku1=')) return;
    try {
      const encoded = location.hash.slice(5);
      if (
        encoded.length > Math.ceil((LIMITS.shareBytes * 4) / 3) + 4 ||
        !/^[A-Za-z0-9_-]+$/.test(encoded)
      )
        throw new Error('invalidShare');
      const bytes = Uint8Array.from(
        atob(encoded.replaceAll('-', '+').replaceAll('_', '/')),
        (char) => char.charCodeAt(0),
      );
      if (bytes.length > LIMITS.shareBytes) throw new Error('invalidShare');
      const payload = JSON.parse(
        new TextDecoder('utf-8', { fatal: true }).decode(bytes),
      );
      if (
        payload?.v !== 1 ||
        typeof payload.source !== 'string' ||
        !Array.isArray(payload.files)
      )
        throw new Error('invalidShare');
      validateInput(payload.source, payload.files);
      const accepted = await askConfirmation(
        isChanged() ? `${t.shareLoad}\n\n${t.replace}` : t.shareLoad,
      );
      if (
        accepted &&
        !disposed &&
        request === replacementEpoch &&
        location.hash === fragment
      ) {
        stop();
        load(payload);
      }
    } catch {
      notify(t.invalidShare);
    }
  };
  window.addEventListener('hashchange', readShare, {
    signal: listeners.signal,
  });
  readShare();
  document.addEventListener(
    'visibilitychange',
    () => {
      if (document.hidden && active) stop(t.hidden);
    },
    { signal: listeners.signal },
  );
  const dispose = () => {
    if (disposed) return;
    persist();
    restoredDrafts.set(root, {
      source,
      files: files.map((file) => ({ ...file })),
      baseline,
      baselineFiles,
      selected,
    });
    disposed = true;
    replacementEpoch++;
    finishConfirmation(false);
    stop();
    clearTimeout(saveTimer);
    listeners.abort();
    editor.destroy();
    for (const [url, timer] of downloads) {
      clearTimeout(timer);
      URL.revokeObjectURL(url);
    }
    downloads.clear();
    sourcePreview.value = source;
    sourcePreview.hidden = false;
    root
      .querySelectorAll<HTMLButtonElement>('button[data-action]')
      .forEach((button) => {
        button.disabled = true;
      });
    root
      .querySelectorAll<HTMLInputElement | HTMLSelectElement>(
        '[data-needs-editor]',
      )
      .forEach((element) => {
        element.disabled = true;
      });
    delete root.dataset.active;
  };
  window.addEventListener('pagehide', dispose, { signal: listeners.signal });
  document.addEventListener('astro:before-swap', dispose, {
    signal: listeners.signal,
  });
  return dispose;
}
