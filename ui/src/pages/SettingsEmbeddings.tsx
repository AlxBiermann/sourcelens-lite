import { useEffect, useState, type CSSProperties, type FormEvent } from 'react';
import { api, errorMessage, type EmbeddingSettings, type EmbeddingSettingsInput, type EmbeddingTestResult } from '../api';
import { Choice, Field } from '../components/Field';
import { Loading, Notice, PageHeader, SettingsTabs } from '../components/Shell';
import { useToast } from '../components/Toast';
import { useApi } from '../hooks/useApi';
import { useUser } from '../hooks/useSession';
import { count, plural } from '../format';

type Form = {
  enabled: boolean;
  baseUrl: string;
  model: string;
  requestDimensions: string;
  queryPrefix: string;
  documentPrefix: string;
  batchSize: string;
  apiKey: string;
  clearApiKey: boolean;
};

function toForm(s: EmbeddingSettings): Form {
  return {
    enabled: s.enabled,
    baseUrl: s.baseUrl,
    model: s.model,
    requestDimensions: s.requestDimensions === null ? '' : String(s.requestDimensions),
    queryPrefix: s.queryPrefix,
    documentPrefix: s.documentPrefix,
    batchSize: String(s.batchSize || 32),
    apiKey: '',
    clearApiKey: false,
  };
}

function toInput(form: Form): EmbeddingSettingsInput {
  const dimensions = Number.parseInt(form.requestDimensions, 10);
  const batch = Number.parseInt(form.batchSize, 10);
  return {
    enabled: form.enabled,
    baseUrl: form.baseUrl.trim(),
    model: form.model.trim(),
    requestDimensions: Number.isFinite(dimensions) && dimensions > 0 ? dimensions : null,
    queryPrefix: form.queryPrefix,
    documentPrefix: form.documentPrefix,
    batchSize: Number.isFinite(batch) ? Math.min(256, Math.max(1, batch)) : 32,
    apiKey: form.apiKey ? form.apiKey : undefined,
    clearApiKey: form.clearApiKey || undefined,
  };
}

export function SettingsEmbeddings() {
  const user = useUser();
  const owner = user.role === 'owner';
  const settings = useApi<EmbeddingSettings>('/api/settings/embeddings', { pollMs: 10000 });
  const notify = useToast();
  const [form, setForm] = useState<Form | null>(null);
  const [busy, setBusy] = useState<'test' | 'save' | null>(null);
  const [test, setTest] = useState<EmbeddingTestResult | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    if (settings.data && form === null) setForm(toForm(settings.data));
  }, [settings.data, form]);

  function update<K extends keyof Form>(key: K, value: Form[K]) {
    setForm((current) => (current ? { ...current, [key]: value } : current));
    setTest(null);
  }

  async function runTest() {
    if (!form) return;
    setBusy('test');
    setTest(null);
    try {
      setTest(await api.post<EmbeddingTestResult>('/api/settings/embeddings/test', toInput(form)));
    } catch (e) {
      setTest({ ok: false, error: errorMessage(e) });
    } finally {
      setBusy(null);
    }
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    if (!form) return;
    setBusy('save');
    setSaveError(null);
    try {
      const saved = await api.put<EmbeddingSettings & { reembedRepositories: number }>('/api/settings/embeddings', toInput(form));
      setForm(toForm(saved));
      notify(saved.reembedRepositories > 0 ? `Saved. Re-embedding ${plural(saved.reembedRepositories, 'repository', 'repositories')}.` : 'Saved.');
      await settings.reload();
    } catch (e) {
      setSaveError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  }

  const data = settings.data;
  const coverage = data && data.totalChunks > 0 ? (100 * data.embeddedChunks) / data.totalChunks : 0;

  return (
    <div className="sl-page">
      <PageHeader title="Settings" />
      <SettingsTabs owner={owner} />
      <p className="sl-prose sl-muted">
        Embeddings add search by meaning next to keyword and identifier search. SourceLens calls any server that speaks the OpenAI embeddings API, such as OpenAI, Ollama or LM Studio.
      </p>
      {!owner ? <Notice>Only the owner can change embedding settings.</Notice> : null}
      {settings.error && !data ? <Notice tone="error">{settings.error.message}</Notice> : null}
      {!form ? <Loading /> : null}

      {data ? (
        <section className="sl-meter" aria-label="Embedding coverage">
          <div className="sl-meter-track" role="progressbar" aria-label="Embedded chunks" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(coverage)}>
            <div className="sl-meter-fill" style={{ '--p': `${coverage}%` } as CSSProperties} />
          </div>
          <p className="sl-meter-text">
            {count(data.embeddedChunks)} of {count(data.totalChunks)} chunks embedded
            {data.failedChunks > 0 ? <span className="sl-text-red">{`, ${count(data.failedChunks)} rejected by the provider`}</span> : null}
          </p>
        </section>
      ) : null}

      {form && data ? (
        <form className="sl-form sl-form-narrow" onSubmit={(e) => void save(e)}>
          <fieldset className="sl-fieldset" disabled={!owner}>
            <legend className="sl-visually-hidden">Embedding settings</legend>
            <label className="sl-switch">
              <input type="checkbox" role="switch" className="sl-switch-input" checked={form.enabled} onChange={(e) => update('enabled', e.target.checked)} />
              <span className="sl-switch-label">Use embeddings</span>
            </label>
            <Field label="Base URL" hint="For example https://api.openai.com/v1 or http://localhost:11434/v1.">
              {(props) => <input {...props} className="sl-input sl-input-mono" type="url" value={form.baseUrl} onChange={(e) => update('baseUrl', e.target.value)} autoComplete="off" spellCheck={false} />}
            </Field>
            <Field label="API key" hint={data.apiKeySet ? 'Leave empty to keep the saved key.' : 'Leave empty for servers without authentication.'}>
              {(props) => (
                <input {...props} className="sl-input sl-input-mono" type="password" value={form.apiKey} placeholder={data.apiKeySet ? 'Saved' : undefined} onChange={(e) => update('apiKey', e.target.value)} autoComplete="new-password" disabled={form.clearApiKey} />
              )}
            </Field>
            {data.apiKeySet ? <Choice type="checkbox" checked={form.clearApiKey} onChange={(checked) => update('clearApiKey', checked)} label="Remove saved key" /> : null}
            <Field label="Model">
              {(props) => <input {...props} className="sl-input sl-input-mono" value={form.model} onChange={(e) => update('model', e.target.value)} autoComplete="off" spellCheck={false} />}
            </Field>
            <Field label="Dimensions" hint={data.dimensions ? `Leave empty to use the model's default. Detected: ${data.dimensions}.` : "Leave empty to use the model's default"}>
              {(props) => <input {...props} className="sl-input sl-input-short" type="number" min={1} inputMode="numeric" value={form.requestDimensions} onChange={(e) => update('requestDimensions', e.target.value)} />}
            </Field>
            <Field label="Query prefix" hint="Put in front of every search query, for example search_query: ">
              {(props) => <input {...props} className="sl-input sl-input-mono" value={form.queryPrefix} onChange={(e) => update('queryPrefix', e.target.value)} autoComplete="off" spellCheck={false} />}
            </Field>
            <Field label="Document prefix" hint="Put in front of every code chunk, for example search_document: ">
              {(props) => <input {...props} className="sl-input sl-input-mono" value={form.documentPrefix} onChange={(e) => update('documentPrefix', e.target.value)} autoComplete="off" spellCheck={false} />}
            </Field>
            <Field label="Batch size" hint="Chunks per request, 1 to 256.">
              {(props) => <input {...props} className="sl-input sl-input-short" type="number" min={1} max={256} inputMode="numeric" value={form.batchSize} onChange={(e) => update('batchSize', e.target.value)} />}
            </Field>
          </fieldset>

          {test ? (
            <p className={test.ok ? 'sl-result-ok' : 'sl-field-error'} role="status">
              {test.ok ? `Connected. ${test.dimensions} dimensions, ${test.latencyMs} ms.` : test.error}
            </p>
          ) : null}
          {saveError ? (
            <p className="sl-field-error" role="alert">
              {saveError}
            </p>
          ) : null}

          {owner ? (
            <div className="sl-actions">
              <button type="submit" className="sl-button sl-button-primary" disabled={busy !== null}>
                Save embedding settings
              </button>
              <button type="button" className="sl-button sl-button-secondary" disabled={busy !== null || !form.baseUrl.trim() || !form.model.trim()} onClick={() => void runTest()}>
                Test connection
              </button>
            </div>
          ) : null}
        </form>
      ) : null}
    </div>
  );
}
