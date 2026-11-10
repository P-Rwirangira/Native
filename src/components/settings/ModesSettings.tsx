import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { Check, FileText, Loader2, Plus, RefreshCw, Save, Trash2, Upload, X } from 'lucide-react';

type Mode = {
  id: string; name: string; templateType: string; customContext: string;
  isActive: boolean; isBuiltin?: boolean; createdAt: string; referenceFileCount: number;
};
type RefFile = { id: string; modeId: string; fileName: string; content: string; createdAt: string };
type Props = {
  onClose: () => void;
  onOpenNativelyAPI?: () => void;
  isPremium?: boolean;
  isLoaded?: boolean;
  isTrialActive?: boolean;
};

const META: Record<string, { label: string; description: string }> = {
  general: { label: 'General', description: 'Adaptive help for any conversation.' },
  sales: { label: 'Sales', description: 'Discovery, objections, and next steps.' },
  recruiting: { label: 'Recruiting', description: 'Structured candidate evaluation.' },
  'team-meet': { label: 'Team Meet', description: 'Decisions, owners, and blockers.' },
  'looking-for-work': { label: 'Looking for work', description: 'Job-search and interview support.' },
  'technical-interview': { label: 'Technical Interview', description: 'Coding and system-design support.' },
  lecture: { label: 'Lecture', description: 'Concepts, definitions, and study notes.' },
  seminar: { label: 'Seminar', description: 'Reference-file-grounded questions and answers.' },
  'call-center': { label: 'Call Center', description: 'Issues, troubleshooting, and escalation.' },
};
const errorMessage = (error?: string) => error === 'pro_required' ? 'This action needs an active Max plan.' : (error || 'Something went wrong.');

/** Core Modes Manager. The private premium submodule is unavailable in this checkout,
 * but the mode engine and all of its IPC operations are core functionality. */
const ModesSettings: React.FC<Props> = ({ onClose, onOpenNativelyAPI }) => {
  const [modes, setModes] = useState<Mode[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [refs, setRefs] = useState<RefFile[]>([]);
  const [context, setContext] = useState('');
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState('');
  const [newTemplate, setNewTemplate] = useState('general');
  const selected = useMemo(() => modes.find((m) => m.id === selectedId) ?? null, [modes, selectedId]);

  const load = useCallback(async (preferred?: string | null) => {
    if (!window.electronAPI?.modesGetAll) { setError('Electron modes bridge unavailable. Reload the app.'); setLoading(false); return; }
    try {
      setError(null);
      const [all, active] = await Promise.all([window.electronAPI.modesGetAll(), window.electronAPI.modesGetActive()]);
      const next = all as Mode[];
      setModes(next);
      const wanted = preferred ?? active?.id ?? selectedId;
      setSelectedId(next.some((m) => m.id === wanted) ? wanted! : (next[0]?.id ?? null));
    } catch (e) { setError(e instanceof Error ? e.message : 'Could not load modes.'); }
    finally { setLoading(false); }
  }, [selectedId]);
  useEffect(() => { void load(); }, []);
  useEffect(() => {
    if (!selected) { setContext(''); setRefs([]); return; }
    setContext(selected.customContext || '');
    let alive = true;
    window.electronAPI.modesGetReferenceFiles(selected.id).then((files) => { if (alive) setRefs(files); }).catch(() => {});
    return () => { alive = false; };
  }, [selected?.id]);
  useEffect(() => { const off = window.electronAPI?.onModeChanged?.(() => void load(selectedId)); return () => off?.(); }, [load, selectedId]);

  const activate = async () => {
    if (!selected || selected.isActive) return;
    setBusy('activate'); setError(null);
    try { const r = await window.electronAPI.modesSetActive(selected.id); if (!r.success) { setError(errorMessage(r.error)); if (r.error === 'pro_required') onOpenNativelyAPI?.(); } else await load(selected.id); }
    finally { setBusy(null); }
  };
  const save = async () => {
    if (!selected) return;
    setBusy('save'); setError(null);
    try { const r = await window.electronAPI.modesUpdate(selected.id, { customContext: context }); if (!r.success) setError(errorMessage(r.error)); else await load(selected.id); }
    finally { setBusy(null); }
  };
  const upload = async () => {
    if (!selected) return;
    setBusy('upload'); setError(null);
    try { const r = await window.electronAPI.modesUploadReferenceFile(selected.id); if (!r.success && !r.cancelled) setError(errorMessage(r.error)); else if (!r.cancelled) { setRefs(await window.electronAPI.modesGetReferenceFiles(selected.id)); await load(selected.id); } }
    finally { setBusy(null); }
  };
  const removeRef = async (file: RefFile) => {
    setBusy(`remove:${file.id}`); setError(null);
    try { const r = await window.electronAPI.modesDeleteReferenceFile(file.id); if (!r.success) setError(errorMessage(r.error)); else { setRefs((old) => old.filter((f) => f.id !== file.id)); await load(selectedId); } }
    finally { setBusy(null); }
  };
  const create = async () => {
    if (!newName.trim()) return;
    setBusy('create'); setError(null);
    try { const r = await window.electronAPI.modesCreate({ name: newName.trim(), templateType: newTemplate }); if (!r.success) { setError(errorMessage(r.error)); if (r.error === 'pro_required') onOpenNativelyAPI?.(); } else { setNewName(''); setCreating(false); await load(r.mode?.id); } }
    finally { setBusy(null); }
  };
  const meta = selected ? (META[selected.templateType] ?? META.general) : META.general;

  return <div className="flex h-full min-h-0 overflow-hidden text-text-primary">
    <aside className="flex w-[245px] shrink-0 flex-col border-r border-border-muted bg-bg-primary/50">
      <div className="flex h-16 items-center justify-between border-b border-border-muted px-4"><div><h2 className="text-base font-semibold">Modes</h2><p className="text-xs text-text-tertiary">Choose how Natively helps</p></div><button type="button" onClick={() => void load(selectedId)} className="rounded-lg p-2 text-text-tertiary hover:bg-bg-hover" aria-label="Refresh"><RefreshCw size={15} className={loading ? 'animate-spin' : ''} /></button></div>
      <div className="min-h-0 flex-1 overflow-y-auto p-2">{loading ? <div className="flex h-32 items-center justify-center text-text-tertiary"><Loader2 size={20} className="animate-spin" /></div> : modes.length === 0 ? <p className="m-2 rounded-xl border border-border-muted p-3 text-xs text-text-secondary">No modes returned. Restart Natively to reseed defaults.</p> : modes.map((mode) => <button type="button" key={mode.id} onClick={() => setSelectedId(mode.id)} className={`mb-1 flex w-full items-center gap-3 rounded-xl px-3 py-2.5 text-left ${mode.id === selectedId ? 'bg-bg-hover text-text-primary' : 'text-text-secondary hover:bg-bg-hover/60'}`}><span className="grid h-8 w-8 shrink-0 place-items-center rounded-lg bg-accent/15 text-accent text-xs font-bold">{(META[mode.templateType]?.label ?? 'Mode').slice(0, 1)}</span><span className="min-w-0 flex-1"><span className="block truncate text-sm font-medium">{mode.name}</span><span className="block truncate text-[11px] text-text-tertiary">{META[mode.templateType]?.label ?? mode.templateType}</span></span>{mode.isActive && <span className="h-2 w-2 rounded-full bg-emerald-400" title="Active" />}</button>)}</div>
      <div className="border-t border-border-muted p-3">{creating ? <div className="space-y-2 rounded-xl border border-border-muted bg-bg-secondary p-3"><input autoFocus value={newName} onChange={(e) => setNewName(e.target.value)} onKeyDown={(e) => { if (e.key === 'Enter') void create(); }} placeholder="Mode name" className="w-full rounded-lg border border-border-muted bg-bg-primary px-3 py-2 text-sm outline-none focus:border-accent" /><select value={newTemplate} onChange={(e) => setNewTemplate(e.target.value)} className="w-full rounded-lg border border-border-muted bg-bg-primary px-3 py-2 text-sm">{Object.entries(META).map(([value, m]) => <option key={value} value={value}>{m.label}</option>)}</select><div className="flex gap-2"><button type="button" disabled={!newName.trim() || busy === 'create'} onClick={() => void create()} className="flex-1 rounded-lg bg-accent px-3 py-2 text-xs font-semibold text-white disabled:opacity-50">Create</button><button type="button" onClick={() => setCreating(false)} className="rounded-lg px-3 py-2 text-xs text-text-secondary">Cancel</button></div></div> : <button type="button" onClick={() => setCreating(true)} className="flex w-full items-center justify-center gap-2 rounded-xl border border-border-muted px-3 py-2.5 text-sm text-text-secondary hover:bg-bg-hover"><Plus size={15} /> New mode</button>}</div>
    </aside>
    <main className="flex min-w-0 flex-1 flex-col bg-bg-elevated">
      <header className="flex h-16 shrink-0 items-center justify-between border-b border-border-muted px-5"><div className="min-w-0"><h3 className="truncate text-base font-semibold">{selected?.name ?? 'Modes Manager'}</h3><p className="truncate text-xs text-text-tertiary">{selected ? meta.description : 'Select a mode to configure it.'}</p></div><div className="flex items-center gap-2">{selected && (selected.isActive ? <span className="flex items-center gap-1.5 rounded-full bg-emerald-500/12 px-3 py-1.5 text-xs text-emerald-400"><Check size={13} /> Active</span> : <button type="button" onClick={() => void activate()} disabled={busy === 'activate'} className="rounded-lg bg-accent px-3.5 py-2 text-xs font-semibold text-white disabled:opacity-60">{busy === 'activate' ? 'Activating…' : 'Use this mode'}</button>)}<button type="button" onClick={onClose} className="rounded-lg p-2 text-text-tertiary hover:bg-bg-hover" aria-label="Close"><X size={18} /></button></div></header>
      <div className="min-h-0 flex-1 overflow-y-auto p-5">{error && <div className="mb-4 rounded-xl border border-red-500/25 bg-red-500/10 px-4 py-3 text-sm text-red-300">{error}</div>}{!selected && !loading ? <div className="grid h-full place-items-center text-sm text-text-tertiary">Select a mode from the list.</div> : selected && <div className="mx-auto max-w-2xl space-y-5"><section className="rounded-2xl border border-border-muted bg-bg-primary/35 p-4"><div className="mb-3 flex items-start justify-between gap-4"><div><h4 className="text-sm font-semibold">Instructions</h4><p className="mt-1 text-xs text-text-tertiary">Add goals, terminology, response style, or mode-specific facts.</p></div><button type="button" onClick={() => void save()} disabled={context === (selected.customContext || '') || busy === 'save'} className="flex items-center gap-1.5 rounded-lg border border-border-muted px-3 py-2 text-xs text-text-secondary hover:bg-bg-hover disabled:opacity-40"><Save size={13} /> Save</button></div><textarea value={context} onChange={(e) => setContext(e.target.value)} rows={6} placeholder="Example: Keep answers concise and use examples from our TypeScript stack…" className="w-full resize-y rounded-xl border border-border-muted bg-bg-primary px-3 py-3 text-sm leading-5 outline-none placeholder:text-text-tertiary focus:border-accent" /></section><section className="rounded-2xl border border-border-muted bg-bg-primary/35 p-4"><div className="mb-3 flex items-start justify-between gap-4"><div><h4 className="text-sm font-semibold">Reference files</h4><p className="mt-1 text-xs text-text-tertiary">Ground answers in PDFs, documents, Markdown, or text files.</p></div><button type="button" onClick={() => void upload()} disabled={busy === 'upload'} className="flex items-center gap-1.5 rounded-lg border border-border-muted px-3 py-2 text-xs text-text-secondary hover:bg-bg-hover disabled:opacity-50"><Upload size={13} /> Add file</button></div>{refs.length === 0 ? <div className="rounded-xl border border-dashed border-border-muted px-4 py-6 text-center text-xs text-text-tertiary">No reference files attached to this mode.</div> : <div className="space-y-2">{refs.map((file) => <div key={file.id} className="flex items-center gap-3 rounded-xl border border-border-muted bg-bg-primary px-3 py-2.5"><FileText size={15} className="text-accent" /><span className="min-w-0 flex-1 truncate text-sm text-text-secondary">{file.fileName}</span><button type="button" onClick={() => void removeRef(file)} disabled={busy === `remove:${file.id}`} className="rounded-lg p-1.5 text-text-tertiary hover:text-red-400" aria-label={`Remove ${file.fileName}`}><Trash2 size={14} /></button></div>)}</div>}</section></div>}</div>
    </main>
  </div>;
};

export default ModesSettings;
