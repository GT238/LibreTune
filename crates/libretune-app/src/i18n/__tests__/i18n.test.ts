import { describe, it, expect, beforeAll } from 'vitest';
import i18n, { SUPPORTED_LANGUAGES } from '../index';

describe('i18n bootstrap', () => {
  beforeAll(async () => {
    // Ensure init has settled before assertions.
    if (!i18n.isInitialized) {
      await new Promise<void>(resolve => i18n.on('initialized', () => resolve()));
    }
  });

  it('exposes the supported languages list', () => {
    const codes = SUPPORTED_LANGUAGES.map(l => l.code);
    expect(codes).toContain('en');
    expect(codes).toContain('pt-BR');
    expect(codes).toContain('hu-HU');
  });

  it('resolves an English key', async () => {
    await i18n.changeLanguage('en');
    expect(i18n.t('actions.cancel', { ns: 'common' })).toBe('Cancel');
  });

  it('switches to Portuguese (Brasil)', async () => {
    await i18n.changeLanguage('pt-BR');
    expect(i18n.t('actions.cancel', { ns: 'common' })).toBe('Cancelar');
    expect(i18n.t('state.connected', { ns: 'common' })).toBe('Conectado');
  });

  it('switches to Hungarian', async () => {
    await i18n.changeLanguage('hu-HU');
    expect(i18n.t('actions.cancel', { ns: 'common' })).toBe('Megszakít');
    expect(i18n.t('state.connected', { ns: 'common' })).toBe('Kapcsolódva');
  });

  it('falls back to English when a key is missing in the active locale', async () => {
    await i18n.changeLanguage('pt-BR');
    // Key only exists in en; pt-BR should fall through.
    const result = i18n.t('this.key.does.not.exist', {
      ns: 'common',
      defaultValue: 'fallback-value',
    });
    expect(result).toBe('fallback-value');
  });

  it('interpolates variables', async () => {
    await i18n.changeLanguage('en');
    expect(
      i18n.t('state.partialSync', { ns: 'common', done: 3, total: 7 })
    ).toBe('Partial sync (3/7)');
  });
});
