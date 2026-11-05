import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it, vi } from 'vitest';
import {
  Button,
  CodeEditor,
  JSONDiffEditor,
  TextInput,
  ToggleCollection,
  Fence,
} from './ui';
import { rosePineDawnTheme } from './rosePineDawnTheme';

vi.mock('@monaco-editor/react', () => ({
  Editor: (props: { theme: string }) =>
    createElement('div', { 'data-editor-theme': props.theme }),
  DiffEditor: (props: { theme: string }) =>
    createElement('div', { 'data-editor-theme': props.theme }),
}));

describe('original Instant controls with BackendRelay branding', () => {
  it('uses upstream primary, hover, disabled and CTA colors instead of the console palette', () => {
    const primary = renderToStaticMarkup(
      createElement(Button, { size: 'mini' }, 'Create'),
    );
    expect(primary).toContain('bg-[#606AF4]');
    expect(primary).toContain('hover:enabled:bg-[#4543e9]');
    expect(primary).toContain('disabled:bg-[#9197f3]');
    expect(primary).toContain('text-sm');
    expect(primary).not.toContain('bg-ui-accent');
    expect(
      renderToStaticMarkup(createElement(Button, { variant: 'cta' }, 'Start')),
    ).toContain('bg-orange-600');
  });

  it('keeps original inputs and accessible field/error associations', () => {
    const html = renderToStaticMarkup(
      createElement(TextInput, {
        id: 'email',
        'aria-label': 'Email address',
        autoComplete: 'email',
        value: '',
        onChange: () => {},
        error: 'Please enter your email',
      }),
    );
    expect(html).toContain('bg-white');
    expect(html).toContain('dark:bg-neutral-800');
    expect(html).toContain('aria-label="Email address"');
    expect(html).toContain('autoComplete="email"');
    expect(html).toContain('aria-describedby="email-error"');
    expect(html).toContain('id="email-error"');
    expect(html).toContain('role="alert"');
  });

  it('preserves accessible names on the original button', () => {
    expect(
      renderToStaticMarkup(
        createElement(Button, {
          'aria-label': 'Switch to light mode',
          size: 'nano',
          variant: 'subtle',
        }),
      ),
    ).toContain('aria-label="Switch to light mode"');
  });

  it('uses the original namespace selection and dark-mode surfaces', () => {
    const html = renderToStaticMarkup(
      createElement(ToggleCollection, {
        items: [{ id: 'todos', label: 'todos' }],
        selectedId: 'todos',
        onChange: () => {},
      }),
    );
    expect(html).toContain('bg-gray-200');
    expect(html).toContain('dark:bg-neutral-600/50');
    expect(html).not.toContain('bg-ui-subtle');
  });

  it('restores default Monaco themes for both code and diff editors', () => {
    for (const darkMode of [false, true]) {
      const theme = darkMode ? 'vs-dark' : 'vs-light';
      expect(
        renderToStaticMarkup(
          createElement(CodeEditor, {
            value: '{}',
            language: 'json',
            darkMode,
            onChange: () => {},
          }),
        ),
      ).toContain(`data-editor-theme="${theme}"`);
      expect(
        renderToStaticMarkup(
          createElement(JSONDiffEditor, {
            original: '{}',
            modified: '{}',
            label: 'Changes',
            darkMode,
          }),
        ),
      ).toContain(`data-editor-theme="${theme}"`);
    }
  });

  it('restores the upstream code-fence backgrounds and light syntax color', () => {
    expect(rosePineDawnTheme.plain).toEqual({
      backgroundColor: '#fff',
      color: '#575279',
    });
    expect(
      renderToStaticMarkup(
        createElement(Fence, {
          code: '{}',
          language: 'json',
          darkMode: true,
        }),
      ),
    ).toContain('background-color:#262626;color:white');
  });
});
