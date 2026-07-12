import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { ask } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import {
  ChevronDown,
  ChevronUp,
  Loader2,
  Pencil,
  Plus,
  Search,
  Trash2,
  X,
} from "lucide-react";
import {
  commands,
  DictationCommand,
  DictationCommandList,
  DictationPhrase,
} from "@/bindings";

const OPERATION_TYPES = [
  "insert_text",
  "opening_mark",
  "closing_mark",
  "sentence_terminator",
  "toggle_mark",
  "newline",
  "paragraph",
] as const;

const NO_REPLACEMENT_TYPES = new Set(["newline", "paragraph"]);
const BASE_LANGUAGES = ["de", "en", "it", "fr", "ru"];

const Spinner: React.FC<{ size?: number }> = ({ size = 12 }) => (
  <span className="inline-flex animate-spin" style={{ lineHeight: 0 }}>
    <Loader2 size={size} />
  </span>
);

/** Server errors arrive as "{status}:{detail}". */
const parseApiError = (raw: string): { status: number; detail: string } => {
  const idx = raw.indexOf(":");
  if (idx > 0) {
    const status = Number(raw.slice(0, idx));
    if (Number.isFinite(status)) {
      return { status, detail: raw.slice(idx + 1) };
    }
  }
  return { status: 0, detail: raw };
};

const replacementPreview = (command: DictationCommand): string => {
  if (command.operation_type === "newline") return "↵";
  if (command.operation_type === "paragraph") return "¶";
  return command.replacement_value ?? "";
};

interface PhraseDraft {
  phrase: string;
  language: string;
}

export const DictationSettings: React.FC = () => {
  const { t } = useTranslation();
  const [data, setData] = useState<DictationCommandList | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [isAuthenticated, setIsAuthenticated] = useState<boolean | null>(null);

  const [search, setSearch] = useState("");
  const [sourceFilter, setSourceFilter] = useState("");
  const [typeFilter, setTypeFilter] = useState("");
  const [languageFilter, setLanguageFilter] = useState("");

  const [expandedId, setExpandedId] = useState<number | null>(null);
  const [busyIds, setBusyIds] = useState<Set<string>>(new Set());
  const [showCreate, setShowCreate] = useState(false);
  const [editingId, setEditingId] = useState<number | null>(null);

  const requestSeq = useRef(0);

  const refreshAuth = useCallback(async () => {
    try {
      const result = await commands.livesttAuthStatus();
      if (result.status === "ok") {
        setIsAuthenticated(result.data.is_authenticated);
      }
    } catch (error) {
      console.error("Failed to read auth status:", error);
    }
  }, []);

  const buildQuery = useCallback(
    (cursor: string | null) => ({
      search: search.trim() ? search.trim() : null,
      language: languageFilter || null,
      source: sourceFilter || null,
      enabled: null,
      operation_type: typeFilter || null,
      cursor,
      limit: 200,
    }),
    [search, sourceFilter, typeFilter, languageFilter],
  );

  const load = useCallback(async () => {
    const seq = ++requestSeq.current;
    setLoading(true);
    setLoadError(null);

    const result = await commands.dictationListCommands(buildQuery(null));
    if (seq !== requestSeq.current) return;

    setLoading(false);
    if (result.status === "ok") {
      setData(result.data);
    } else {
      setLoadError(parseApiError(result.error).detail);
    }
  }, [buildQuery]);

  const loadMore = async () => {
    if (!data?.next_cursor || loadingMore) return;
    setLoadingMore(true);
    const result = await commands.dictationListCommands(
      buildQuery(data.next_cursor),
    );
    setLoadingMore(false);
    if (result.status === "ok") {
      setData({
        ...result.data,
        items: [...data.items, ...result.data.items],
      });
    }
  };

  useEffect(() => {
    refreshAuth();
    const unlistenAuth = listen("livestt://auth-changed", () => refreshAuth());
    return () => {
      unlistenAuth.then((fn) => fn());
    };
  }, [refreshAuth]);

  useEffect(() => {
    const timer = setTimeout(load, search ? 350 : 0);
    return () => clearTimeout(timer);
  }, [load, search]);

  const withBusy = async (key: string, action: () => Promise<void>) => {
    setBusyIds((prev) => new Set(prev).add(key));
    try {
      await action();
    } finally {
      setBusyIds((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  };

  const showApiError = (raw: string) => {
    const { status, detail } = parseApiError(raw);
    if (status === 409) {
      toast.error(t("settings.dictation.errors.duplicate"));
    } else if (detail) {
      toast.error(detail);
    } else {
      toast.error(t("settings.dictation.errors.generic"));
    }
  };

  const patchCommandInState = (updated: DictationCommand) => {
    setData((prev) =>
      prev
        ? {
            ...prev,
            items: prev.items.map((c) => (c.id === updated.id ? updated : c)),
          }
        : prev,
    );
  };

  const toggleCommand = (command: DictationCommand) =>
    withBusy(`cmd-${command.id}`, async () => {
      const disable = command.enabled;
      const result = await commands.dictationSetDefaultCommandDisabled(
        command.id,
        disable,
      );
      if (result.status !== "ok") {
        showApiError(result.error);
        return;
      }
      patchCommandInState({
        ...command,
        enabled: !disable,
        disabled_by_doctor: disable,
      });
    });

  const togglePhrase = (command: DictationCommand, phrase: DictationPhrase) =>
    withBusy(`phr-${phrase.id}`, async () => {
      const disable = phrase.enabled;
      const result = await commands.dictationSetDefaultPhraseDisabled(
        phrase.id,
        disable,
      );
      if (result.status !== "ok") {
        showApiError(result.error);
        return;
      }
      patchCommandInState({
        ...command,
        enabled_phrase_count: command.enabled_phrase_count + (disable ? -1 : 1),
        phrases: command.phrases.map((p) =>
          p.id === phrase.id
            ? { ...p, enabled: !disable, disabled_by_doctor: disable }
            : p,
        ),
      });
    });

  const deletePhrase = (command: DictationCommand, phrase: DictationPhrase) =>
    withBusy(`phr-${phrase.id}`, async () => {
      const result = await commands.dictationDeletePhrase(phrase.id);
      if (result.status !== "ok") {
        showApiError(result.error);
        return;
      }
      patchCommandInState({
        ...command,
        phrase_count: command.phrase_count - 1,
        enabled_phrase_count:
          command.enabled_phrase_count - (phrase.enabled ? 1 : 0),
        phrases: command.phrases.filter((p) => p.id !== phrase.id),
      });
    });

  const addPhrase = async (
    command: DictationCommand,
    phrase: string,
    language: string,
  ): Promise<boolean> => {
    const result = await commands.dictationAddPhrase(command.id, {
      phrase: phrase.trim(),
      language: language || null,
    });
    if (result.status !== "ok") {
      showApiError(result.error);
      return false;
    }
    patchCommandInState({
      ...command,
      phrase_count: command.phrase_count + 1,
      enabled_phrase_count: command.enabled_phrase_count + 1,
      phrases: [...command.phrases, result.data],
    });
    return true;
  };

  const deleteCommand = async (command: DictationCommand) => {
    const confirmed = await ask(
      t("settings.dictation.deleteCommandConfirm", { name: command.name }),
      { title: t("settings.dictation.delete"), kind: "warning" },
    );
    if (!confirmed) return;

    await withBusy(`cmd-${command.id}`, async () => {
      const result = await commands.dictationDeleteCommand(command.id);
      if (result.status !== "ok") {
        showApiError(result.error);
        return;
      }
      toast.success(t("settings.dictation.deleted"));
      setData((prev) =>
        prev
          ? {
              ...prev,
              total: prev.total - 1,
              items: prev.items.filter((c) => c.id !== command.id),
            }
          : prev,
      );
    });
  };

  const languageOptions = useMemo(() => {
    const fromFacets = Object.keys(data?.facets.languages ?? {});
    return Array.from(new Set([...BASE_LANGUAGES, ...fromFacets])).sort();
  }, [data]);

  const items = data?.items ?? [];

  return (
    <div className="mx-auto w-full max-w-3xl space-y-4">
      <div className="flex items-center justify-between px-4">
        <h2 className="text-xs font-medium tracking-wide text-mid-gray uppercase">
          {t("settings.dictation.title")}
        </h2>
        <button
          type="button"
          className="flex items-center gap-1.5 rounded-md bg-background-ui px-3 py-1.5 text-xs font-medium text-white hover:opacity-90"
          onClick={() => setShowCreate(true)}
        >
          <Plus size={14} />
          {t("settings.dictation.addCommand")}
        </button>
      </div>

      {isAuthenticated === false && (
        <div className="rounded-lg border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-800">
          {t("settings.dictation.authRequired")}
        </div>
      )}

      <div className="space-y-2">
        <div className="relative">
          <Search
            size={15}
            className="pointer-events-none absolute start-3 top-1/2 -translate-y-1/2 text-mid-gray"
          />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder={t("settings.dictation.searchPlaceholder")}
            className="w-full rounded-lg border border-mid-gray/20 bg-background py-2 ps-9 pe-3 text-sm text-text placeholder:text-mid-gray focus:border-mid-gray/40 focus:outline-none"
          />
        </div>

        <div className="flex flex-wrap gap-2">
          <select
            value={sourceFilter}
            onChange={(e) => setSourceFilter(e.target.value)}
            className="rounded-lg border border-mid-gray/20 bg-background px-2 py-1.5 text-xs text-text focus:outline-none"
          >
            <option value="">
              {t("settings.dictation.filters.allSources")}
            </option>
            <option value="global">
              {t("settings.dictation.badge.global")}
            </option>
            <option value="user">{t("settings.dictation.badge.user")}</option>
          </select>
          <select
            value={typeFilter}
            onChange={(e) => setTypeFilter(e.target.value)}
            className="rounded-lg border border-mid-gray/20 bg-background px-2 py-1.5 text-xs text-text focus:outline-none"
          >
            <option value="">{t("settings.dictation.filters.allTypes")}</option>
            {OPERATION_TYPES.map((op) => (
              <option key={op} value={op}>
                {t(`settings.dictation.opType.${op}`)}
              </option>
            ))}
          </select>
          <select
            value={languageFilter}
            onChange={(e) => setLanguageFilter(e.target.value)}
            className="rounded-lg border border-mid-gray/20 bg-background px-2 py-1.5 text-xs text-text focus:outline-none"
          >
            <option value="">
              {t("settings.dictation.filters.allLanguages")}
            </option>
            {languageOptions.map((lang) => (
              <option key={lang} value={lang}>
                {lang.toUpperCase()}
              </option>
            ))}
          </select>
          {data && (
            <span className="ms-auto self-center text-xs text-text/50">
              {t("settings.dictation.total", { count: data.total })}
            </span>
          )}
        </div>
      </div>

      <div className="overflow-visible rounded-lg border border-mid-gray/20 bg-background">
        {loading ? (
          <div className="flex items-center justify-center gap-2 px-4 py-8 text-sm text-text/60">
            <Spinner size={16} />
            {t("settings.dictation.loading")}
          </div>
        ) : loadError ? (
          <div className="px-4 py-6 text-center text-sm text-rose-600">
            {loadError}
          </div>
        ) : items.length === 0 ? (
          <div className="px-4 py-6 text-center text-sm text-text/60">
            {t("settings.dictation.empty")}
          </div>
        ) : (
          <div className="divide-y divide-mid-gray/20">
            {items.map((command) => (
              <CommandRow
                key={command.id}
                command={command}
                expanded={expandedId === command.id}
                editing={editingId === command.id}
                busy={busyIds.has(`cmd-${command.id}`)}
                busyPhrases={busyIds}
                languageOptions={languageOptions}
                onToggleExpand={() =>
                  setExpandedId(expandedId === command.id ? null : command.id)
                }
                onToggleEnabled={() => toggleCommand(command)}
                onTogglePhrase={(phrase) => togglePhrase(command, phrase)}
                onDeletePhrase={(phrase) => deletePhrase(command, phrase)}
                onAddPhrase={(phrase, language) =>
                  addPhrase(command, phrase, language)
                }
                onDelete={() => deleteCommand(command)}
                onStartEdit={() => setEditingId(command.id)}
                onStopEdit={() => setEditingId(null)}
                onSaved={(updated) => {
                  patchCommandInState(updated);
                  setEditingId(null);
                }}
                onApiError={showApiError}
              />
            ))}
          </div>
        )}
      </div>

      {data?.next_cursor && (
        <button
          type="button"
          className="w-full rounded-lg border border-mid-gray/20 bg-background py-2 text-sm text-text/70 hover:bg-slate-50"
          onClick={loadMore}
          disabled={loadingMore}
        >
          {loadingMore
            ? t("settings.dictation.loading")
            : t("settings.dictation.loadMore")}
        </button>
      )}

      {showCreate && (
        <CreateCommandModal
          languageOptions={languageOptions}
          onClose={() => setShowCreate(false)}
          onCreated={(created) => {
            setShowCreate(false);
            toast.success(t("settings.dictation.created"));
            setData((prev) =>
              prev
                ? {
                    ...prev,
                    total: prev.total + 1,
                    items: [created, ...prev.items],
                  }
                : prev,
            );
            setExpandedId(created.id);
          }}
          onApiError={showApiError}
        />
      )}
    </div>
  );
};

interface CommandRowProps {
  command: DictationCommand;
  expanded: boolean;
  editing: boolean;
  busy: boolean;
  busyPhrases: Set<string>;
  languageOptions: string[];
  onToggleExpand: () => void;
  onToggleEnabled: () => void;
  onTogglePhrase: (phrase: DictationPhrase) => void;
  onDeletePhrase: (phrase: DictationPhrase) => void;
  onAddPhrase: (phrase: string, language: string) => Promise<boolean>;
  onDelete: () => void;
  onStartEdit: () => void;
  onStopEdit: () => void;
  onSaved: (updated: DictationCommand) => void;
  onApiError: (raw: string) => void;
}

const CommandRow: React.FC<CommandRowProps> = ({
  command,
  expanded,
  editing,
  busy,
  busyPhrases,
  languageOptions,
  onToggleExpand,
  onToggleEnabled,
  onTogglePhrase,
  onDeletePhrase,
  onAddPhrase,
  onDelete,
  onStartEdit,
  onStopEdit,
  onSaved,
  onApiError,
}) => {
  const { t } = useTranslation();
  const [newPhrase, setNewPhrase] = useState("");
  const [newPhraseLang, setNewPhraseLang] = useState("");
  const [addingPhrase, setAddingPhrase] = useState(false);

  const preview = replacementPreview(command);
  const isUser = command.source === "user";

  const submitPhrase = async () => {
    if (!newPhrase.trim() || addingPhrase) return;
    setAddingPhrase(true);
    const ok = await onAddPhrase(newPhrase, newPhraseLang);
    setAddingPhrase(false);
    if (ok) setNewPhrase("");
  };

  return (
    <div className={`px-4 py-3 ${command.enabled ? "" : "opacity-60"}`}>
      <div className="flex items-center gap-3">
        <button
          type="button"
          className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 text-left"
          onClick={onToggleExpand}
        >
          <span className="truncate text-sm font-medium text-text">
            {command.name}
          </span>
          {preview && (
            <code className="shrink-0 rounded bg-slate-100 px-1.5 py-0.5 text-xs text-text/80">
              {preview.length > 24 ? `${preview.slice(0, 24)}…` : preview}
            </code>
          )}
          <span className="shrink-0 rounded-full bg-slate-100 px-2 py-0.5 text-[11px] text-slate-600">
            {t(`settings.dictation.opType.${command.operation_type}`)}
          </span>
          {isUser && (
            <span className="shrink-0 rounded-full bg-sky-100 px-2 py-0.5 text-[11px] text-sky-700">
              {t("settings.dictation.badge.user")}
            </span>
          )}
        </button>

        <span className="shrink-0 text-xs text-text/50">
          {t("settings.dictation.phraseCount", {
            enabled: command.enabled_phrase_count,
            total: command.phrase_count,
          })}
        </span>

        {command.capabilities.can_disable && (
          <label
            className={`relative inline-flex shrink-0 items-center ${busy ? "cursor-wait" : "cursor-pointer"}`}
            title={
              command.enabled
                ? t("settings.dictation.disableCommand")
                : t("settings.dictation.enableCommand")
            }
          >
            <input
              type="checkbox"
              className="peer sr-only"
              checked={command.enabled}
              disabled={busy}
              onChange={onToggleEnabled}
            />
            <div className="peer h-5 w-9 rounded-full bg-mid-gray/20 peer-checked:bg-background-ui after:absolute after:top-[2px] after:start-[2px] after:h-4 after:w-4 after:rounded-full after:bg-white after:transition-all peer-checked:after:translate-x-full rtl:peer-checked:after:-translate-x-full" />
          </label>
        )}

        {command.capabilities.can_delete && (
          <button
            type="button"
            className="shrink-0 rounded-md p-1.5 text-rose-500 hover:bg-rose-50"
            title={t("settings.dictation.delete")}
            onClick={onDelete}
            disabled={busy}
          >
            <Trash2 size={15} />
          </button>
        )}

        <button
          type="button"
          className="shrink-0 rounded-md p-1.5 text-mid-gray hover:bg-slate-100 hover:text-text"
          onClick={onToggleExpand}
        >
          {expanded ? <ChevronUp size={16} /> : <ChevronDown size={16} />}
        </button>
      </div>

      {expanded && (
        <div className="mt-3 space-y-3 rounded-lg bg-slate-50 p-3">
          {editing ? (
            <EditCommandForm
              command={command}
              onCancel={onStopEdit}
              onSaved={onSaved}
              onApiError={onApiError}
            />
          ) : (
            command.capabilities.can_edit && (
              <button
                type="button"
                className="flex items-center gap-1.5 rounded-md border border-mid-gray/30 px-2.5 py-1 text-xs font-medium text-text/70 hover:bg-white"
                onClick={onStartEdit}
              >
                <Pencil size={13} />
                {t("settings.dictation.edit")}
              </button>
            )
          )}

          <div className="flex flex-wrap gap-1.5">
            {command.phrases.map((phrase) => {
              const phraseBusy = busyPhrases.has(`phr-${phrase.id}`);
              return (
                <span
                  key={phrase.id}
                  className={`inline-flex items-center gap-1 rounded-full border px-2.5 py-1 text-xs ${
                    phrase.enabled
                      ? "border-mid-gray/30 bg-white text-text"
                      : "border-mid-gray/20 bg-slate-100 text-text/40 line-through"
                  }`}
                >
                  {phrase.phrase}
                  {phrase.language && (
                    <span className="text-[10px] text-text/40 uppercase no-underline">
                      {phrase.language}
                    </span>
                  )}
                  {phrase.source === "user" && (
                    <span className="h-1.5 w-1.5 rounded-full bg-sky-400" />
                  )}
                  {phrase.capabilities.can_disable && (
                    <button
                      type="button"
                      className="ms-0.5 text-mid-gray hover:text-text"
                      title={
                        phrase.enabled
                          ? t("settings.dictation.disablePhrase")
                          : t("settings.dictation.enablePhrase")
                      }
                      disabled={phraseBusy}
                      onClick={() => onTogglePhrase(phrase)}
                    >
                      {phraseBusy ? (
                        <Spinner size={12} />
                      ) : phrase.enabled ? (
                        <X size={12} />
                      ) : (
                        <Plus size={12} />
                      )}
                    </button>
                  )}
                  {phrase.capabilities.can_delete && (
                    <button
                      type="button"
                      className="ms-0.5 text-rose-400 hover:text-rose-600"
                      title={t("settings.dictation.deletePhrase")}
                      disabled={phraseBusy}
                      onClick={() => onDeletePhrase(phrase)}
                    >
                      {phraseBusy ? (
                        <Spinner size={12} />
                      ) : (
                        <Trash2 size={12} />
                      )}
                    </button>
                  )}
                </span>
              );
            })}
          </div>

          <div className="flex items-center gap-2">
            <input
              type="text"
              value={newPhrase}
              onChange={(e) => setNewPhrase(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && submitPhrase()}
              placeholder={t("settings.dictation.phrasePlaceholder")}
              maxLength={200}
              className="min-w-0 flex-1 rounded-md border border-mid-gray/30 bg-white px-2.5 py-1.5 text-xs text-text placeholder:text-mid-gray focus:outline-none"
            />
            <select
              value={newPhraseLang}
              onChange={(e) => setNewPhraseLang(e.target.value)}
              className="rounded-md border border-mid-gray/30 bg-white px-2 py-1.5 text-xs text-text focus:outline-none"
            >
              <option value="">—</option>
              {languageOptions.map((lang) => (
                <option key={lang} value={lang}>
                  {lang.toUpperCase()}
                </option>
              ))}
            </select>
            <button
              type="button"
              className="flex items-center gap-1 rounded-md border border-mid-gray/30 px-2.5 py-1.5 text-xs font-medium text-text/70 hover:bg-white disabled:opacity-50"
              disabled={!newPhrase.trim() || addingPhrase}
              onClick={submitPhrase}
            >
              {addingPhrase ? <Spinner size={13} /> : <Plus size={13} />}
              {t("settings.dictation.addPhrase")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
};

interface EditCommandFormProps {
  command: DictationCommand;
  onCancel: () => void;
  onSaved: (updated: DictationCommand) => void;
  onApiError: (raw: string) => void;
}

const EditCommandForm: React.FC<EditCommandFormProps> = ({
  command,
  onCancel,
  onSaved,
  onApiError,
}) => {
  const { t } = useTranslation();
  const [name, setName] = useState(command.name);
  const [operationType, setOperationType] = useState(command.operation_type);
  const [replacement, setReplacement] = useState(
    command.replacement_value ?? "",
  );
  const [saving, setSaving] = useState(false);

  const needsReplacement = !NO_REPLACEMENT_TYPES.has(operationType);

  const save = async () => {
    if (saving) return;
    setSaving(true);
    const result = await commands.dictationUpdateCommand(command.id, {
      name: name.trim() || null,
      operation_type:
        operationType !== command.operation_type ? operationType : null,
      replacement_value: needsReplacement ? replacement : null,
    });
    setSaving(false);
    if (result.status !== "ok") {
      onApiError(result.error);
      return;
    }
    onSaved(result.data);
  };

  return (
    <div className="space-y-2 rounded-md border border-mid-gray/20 bg-white p-3">
      <input
        type="text"
        value={name}
        onChange={(e) => setName(e.target.value)}
        maxLength={120}
        placeholder={t("settings.dictation.form.name")}
        className="w-full rounded-md border border-mid-gray/30 px-2.5 py-1.5 text-sm text-text focus:outline-none"
      />
      <div className="flex gap-2">
        <select
          value={operationType}
          onChange={(e) => setOperationType(e.target.value)}
          className="rounded-md border border-mid-gray/30 px-2 py-1.5 text-xs text-text focus:outline-none"
        >
          {OPERATION_TYPES.map((op) => (
            <option key={op} value={op}>
              {t(`settings.dictation.opType.${op}`)}
            </option>
          ))}
        </select>
        {needsReplacement && (
          <input
            type="text"
            value={replacement}
            onChange={(e) => setReplacement(e.target.value)}
            maxLength={2000}
            placeholder={t("settings.dictation.form.replacement")}
            className="min-w-0 flex-1 rounded-md border border-mid-gray/30 px-2.5 py-1.5 text-xs text-text focus:outline-none"
          />
        )}
      </div>
      <div className="flex gap-2">
        <button
          type="button"
          className="rounded-md bg-background-ui px-3 py-1.5 text-xs font-medium text-white hover:opacity-90 disabled:opacity-50"
          disabled={
            saving || !name.trim() || (needsReplacement && !replacement)
          }
          onClick={save}
        >
          {saving
            ? t("settings.dictation.saving")
            : t("settings.dictation.save")}
        </button>
        <button
          type="button"
          className="rounded-md border border-mid-gray/30 px-3 py-1.5 text-xs font-medium text-text/70 hover:bg-slate-50"
          onClick={onCancel}
        >
          {t("settings.dictation.cancel")}
        </button>
      </div>
    </div>
  );
};

interface CreateCommandModalProps {
  languageOptions: string[];
  onClose: () => void;
  onCreated: (created: DictationCommand) => void;
  onApiError: (raw: string) => void;
}

const CreateCommandModal: React.FC<CreateCommandModalProps> = ({
  languageOptions,
  onClose,
  onCreated,
  onApiError,
}) => {
  const { t } = useTranslation();
  const [name, setName] = useState("");
  const [operationType, setOperationType] = useState("insert_text");
  const [replacement, setReplacement] = useState("");
  const [phrases, setPhrases] = useState<PhraseDraft[]>([
    { phrase: "", language: "" },
  ]);
  const [saving, setSaving] = useState(false);

  const needsReplacement = !NO_REPLACEMENT_TYPES.has(operationType);
  const validPhrases = phrases.filter((p) => p.phrase.trim());
  const canSubmit =
    name.trim().length > 0 &&
    validPhrases.length > 0 &&
    (!needsReplacement || replacement.length > 0);

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [onClose]);

  const submit = async () => {
    if (!canSubmit || saving) return;
    setSaving(true);
    const result = await commands.dictationCreateCommand({
      name: name.trim(),
      operation_type: operationType,
      replacement_value: needsReplacement ? replacement : null,
      phrases: validPhrases.map((p) => ({
        phrase: p.phrase.trim(),
        language: p.language || null,
      })),
    });
    setSaving(false);
    if (result.status !== "ok") {
      onApiError(result.error);
      return;
    }
    onCreated(result.data);
  };

  const updateDraft = (index: number, patch: Partial<PhraseDraft>) => {
    setPhrases((prev) =>
      prev.map((p, i) => (i === index ? { ...p, ...patch } : p)),
    );
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-slate-900/40 p-4"
      onClick={onClose}
    >
      <div
        className="w-full max-w-lg space-y-3 rounded-xl border border-mid-gray/20 bg-background p-5 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-semibold text-text">
            {t("settings.dictation.createTitle")}
          </h3>
          <button
            type="button"
            className="rounded-md p-1.5 text-mid-gray hover:bg-slate-100 hover:text-text"
            onClick={onClose}
          >
            <X size={16} />
          </button>
        </div>

        <input
          type="text"
          value={name}
          onChange={(e) => setName(e.target.value)}
          maxLength={120}
          placeholder={t("settings.dictation.form.name")}
          className="w-full rounded-md border border-mid-gray/30 bg-white px-3 py-2 text-sm text-text placeholder:text-mid-gray focus:outline-none"
        />

        <div className="flex gap-2">
          <select
            value={operationType}
            onChange={(e) => setOperationType(e.target.value)}
            className="rounded-md border border-mid-gray/30 bg-white px-2 py-2 text-xs text-text focus:outline-none"
          >
            {OPERATION_TYPES.map((op) => (
              <option key={op} value={op}>
                {t(`settings.dictation.opType.${op}`)}
              </option>
            ))}
          </select>
          {needsReplacement && (
            <input
              type="text"
              value={replacement}
              onChange={(e) => setReplacement(e.target.value)}
              maxLength={2000}
              placeholder={t("settings.dictation.form.replacement")}
              className="min-w-0 flex-1 rounded-md border border-mid-gray/30 bg-white px-3 py-2 text-xs text-text placeholder:text-mid-gray focus:outline-none"
            />
          )}
        </div>
        <p className="text-xs text-text/50">
          {t(`settings.dictation.opTypeHint.${operationType}`)}
        </p>

        <div className="space-y-2">
          <p className="text-xs font-medium text-text/70">
            {t("settings.dictation.form.phrases")}
          </p>
          {phrases.map((draft, index) => (
            <div key={index} className="flex items-center gap-2">
              <input
                type="text"
                value={draft.phrase}
                onChange={(e) => updateDraft(index, { phrase: e.target.value })}
                maxLength={200}
                placeholder={t("settings.dictation.phrasePlaceholder")}
                className="min-w-0 flex-1 rounded-md border border-mid-gray/30 bg-white px-2.5 py-1.5 text-xs text-text placeholder:text-mid-gray focus:outline-none"
              />
              <select
                value={draft.language}
                onChange={(e) =>
                  updateDraft(index, { language: e.target.value })
                }
                className="rounded-md border border-mid-gray/30 bg-white px-2 py-1.5 text-xs text-text focus:outline-none"
              >
                <option value="">—</option>
                {languageOptions.map((lang) => (
                  <option key={lang} value={lang}>
                    {lang.toUpperCase()}
                  </option>
                ))}
              </select>
              {phrases.length > 1 && (
                <button
                  type="button"
                  className="rounded-md p-1.5 text-mid-gray hover:bg-slate-100 hover:text-text"
                  onClick={() =>
                    setPhrases((prev) => prev.filter((_, i) => i !== index))
                  }
                >
                  <X size={14} />
                </button>
              )}
            </div>
          ))}
          <button
            type="button"
            className="flex items-center gap-1 text-xs font-medium text-text/60 hover:text-text"
            onClick={() =>
              setPhrases((prev) => [...prev, { phrase: "", language: "" }])
            }
          >
            <Plus size={13} />
            {t("settings.dictation.addPhraseRow")}
          </button>
        </div>

        <div className="flex justify-end gap-2 pt-1">
          <button
            type="button"
            className="rounded-md border border-mid-gray/30 px-3 py-1.5 text-xs font-medium text-text/70 hover:bg-slate-50"
            onClick={onClose}
          >
            {t("settings.dictation.cancel")}
          </button>
          <button
            type="button"
            className="rounded-md bg-background-ui px-3 py-1.5 text-xs font-medium text-white hover:opacity-90 disabled:opacity-50"
            disabled={!canSubmit || saving}
            onClick={submit}
          >
            {saving
              ? t("settings.dictation.saving")
              : t("settings.dictation.create")}
          </button>
        </div>
      </div>
    </div>
  );
};
