import { basicSetup } from 'codemirror';
import { EditorState, Prec } from '@codemirror/state';
import { EditorView, keymap } from '@codemirror/view';
import {
  StreamLanguage,
  syntaxHighlighting,
  HighlightStyle,
} from '@codemirror/language';
import { autocompletion } from '@codemirror/autocomplete';
import {
  setDiagnostics,
  type Diagnostic as EditorDiagnostic,
} from '@codemirror/lint';
import { tags } from '@lezer/highlight';
import {
  keywords,
  types,
  builtins,
  stdFunctions,
} from '../../../editors/vscode-ku/src/completionModel';
import {
  byteLength,
  LIMITS,
  diagnosticPosition,
  type Diagnostic,
} from './protocol';

const keywordSet = new Set(keywords);
const typeSet = new Set(types);
const builtinSet = new Set(builtins);
const portableFunctions = stdFunctions.filter(
  (name) =>
    /^(json|string|array|object|lexer|parser)\./.test(name) ||
    ['time.now', 'time.millis', 'time.unix', 'time.steady_millis'].includes(
      name,
    ),
);
const language = StreamLanguage.define<{ comment: boolean; depth: number }>({
  startState: () => ({ comment: false, depth: 0 }),
  token(stream, state) {
    if (state.comment) {
      if (stream.skipTo('*/')) {
        stream.match('*/');
        state.comment = false;
      } else stream.skipToEnd();
      return 'comment';
    }
    if (stream.eatSpace()) return null;
    if (stream.match('//')) {
      stream.skipToEnd();
      return 'comment';
    }
    if (stream.match('/*')) {
      state.comment = true;
      return 'comment';
    }
    if (stream.match(/"(?:[^"\\]|\\.)*"?/)) return 'string';
    if (stream.match(/(?:\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/)) return 'number';
    if (stream.match(/[\p{L}_][\p{L}\p{N}_]*/u)) {
      const word = stream.current();
      return keywordSet.has(word)
        ? 'keyword'
        : typeSet.has(word)
          ? 'typeName'
          : builtinSet.has(word)
            ? 'builtin'
            : 'variableName';
    }
    const char = stream.next();
    if (char === '{') state.depth++;
    if (char === '}') state.depth = Math.max(0, state.depth - 1);
    return null;
  },
  indent: (state, textAfter, context) =>
    Math.max(0, state.depth - (/^\s*\}/.test(textAfter) ? 1 : 0)) *
    context.unit,
  languageData: {
    commentTokens: { line: '//', block: { open: '/*', close: '*/' } },
    closeBrackets: { brackets: ['(', '[', '{', '"'] },
  },
});
const theme = EditorView.theme({
  '&': {
    color: 'var(--ku-ink)',
    backgroundColor: 'var(--ku-paper)',
    height: '100%',
    fontSize: '14px',
  },
  '.cm-content': {
    fontFamily: 'var(--ku-mono)',
    minHeight: '380px',
    padding: '16px 0',
  },
  '.cm-scroller': { fontFamily: 'var(--ku-mono)', overflow: 'auto' },
  '.cm-gutters': {
    backgroundColor: 'var(--ku-paper)',
    color: 'var(--ku-muted)',
    borderRight: '1px solid var(--ku-line)',
  },
  '.cm-activeLine, .cm-activeLineGutter': {
    backgroundColor: 'color-mix(in srgb, var(--ku-accent) 12%, transparent)',
  },
  '&.cm-focused': {
    outline: '2px solid var(--ku-accent)',
    outlineOffset: '-2px',
  },
  '.cm-cursor': { borderLeftColor: 'var(--ku-ink)' },
  '.cm-selectionBackground, &.cm-focused .cm-selectionBackground': {
    backgroundColor: 'color-mix(in srgb, var(--ku-accent) 35%, transparent)',
  },
  '.cm-tooltip': {
    backgroundColor: 'var(--ku-paper)',
    border: '1px solid var(--ku-line)',
    color: 'var(--ku-ink)',
  },
  '.cm-tooltip-autocomplete > ul > li[aria-selected]': {
    backgroundColor: 'var(--ku-accent)',
    color: '#242720',
  },
});
const highlights = HighlightStyle.define([
  { tag: tags.keyword, color: 'var(--ku-accent-ink)', fontWeight: '600' },
  { tag: tags.string, color: 'var(--ku-accent-ink)' },
  { tag: tags.number, color: 'var(--ku-muted)' },
  { tag: tags.comment, color: 'var(--ku-muted)', fontStyle: 'italic' },
  {
    tag: tags.typeName,
    textDecoration: 'underline',
    textDecorationColor: 'var(--ku-line)',
  },
]);

export function createEditor(
  parent: HTMLElement,
  source: string,
  changed: (source: string) => void,
  action: (action: 'run' | 'check') => void,
  label: string,
  limit: () => void,
) {
  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc: source,
      extensions: [
        basicSetup,
        language,
        theme,
        syntaxHighlighting(highlights),
        EditorState.changeFilter.of((transaction) => {
          if (
            !transaction.docChanged ||
            byteLength(transaction.newDoc.toString()) <= LIMITS.sourceBytes
          )
            return true;
          queueMicrotask(limit);
          return false;
        }),
        EditorView.contentAttributes.of({
          'aria-label': label,
          spellcheck: 'false',
          autocapitalize: 'off',
        }),
        Prec.highest(
          keymap.of([
            {
              key: 'Mod-Shift-Enter',
              run: () => {
                action('check');
                return true;
              },
            },
            {
              key: 'Mod-Enter',
              run: () => {
                action('run');
                return true;
              },
            },
          ]),
        ),
        autocompletion({
          override: [
            (context) => {
              const word = context.matchBefore(/[\w.]+/);
              if (!word && !context.explicit) return null;
              return {
                from: word?.from ?? context.pos,
                options: [
                  ...keywords.map((label) => ({ label, type: 'keyword' })),
                  ...types.map((label) => ({ label, type: 'type' })),
                  ...builtins.map((label) => ({ label, type: 'function' })),
                  ...portableFunctions.map((label) => ({
                    label,
                    type: 'function',
                    detail: 'Ku standard library',
                  })),
                ],
              };
            },
          ],
        }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) changed(update.state.doc.toString());
        }),
      ],
    }),
  });
  return {
    get source() {
      return view.state.doc.toString();
    },
    replace(source: string) {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: source },
      });
    },
    diagnostics(source: string, diagnostics: Diagnostic[]) {
      const items: EditorDiagnostic[] = diagnostics
        .filter((d) => !d.file || d.file === 'main.ku')
        .map((diagnostic) => {
          const from = diagnosticPosition(source, diagnostic);
          const to =
            diagnostic.endOffset === undefined
              ? Math.min(
                  source.length,
                  from + (source.codePointAt(from)! > 0xffff ? 2 : 1),
                )
              : Math.max(
                  from,
                  diagnosticPosition(source, {
                    offset: diagnostic.endOffset,
                    message: '',
                  }),
                );
          return {
            from,
            to,
            severity: diagnostic.severity ?? 'error',
            message: diagnostic.message,
          };
        });
      view.dispatch(setDiagnostics(view.state, items));
    },
    jump(source: string, diagnostic: Diagnostic) {
      const anchor = diagnosticPosition(source, diagnostic);
      view.dispatch({
        selection: { anchor },
        effects: EditorView.scrollIntoView(anchor, { y: 'center' }),
      });
      view.focus();
    },
    destroy() {
      view.destroy();
    },
  };
}
