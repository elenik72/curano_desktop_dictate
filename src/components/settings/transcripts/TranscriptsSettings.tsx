import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import { convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { readFile } from "@tauri-apps/plugin-fs";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import {
  ChevronDown,
  ChevronUp,
  CloudUpload,
  Copy,
  FileAudio,
  Loader2,
  Maximize2,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react";
import { commands, events, UploadEntry, UploadStatus } from "@/bindings";
import { AudioPlayer } from "@/components/ui/AudioPlayer";
import { useOsType } from "@/hooks/useOsType";

const AUDIO_EXTENSIONS = ["mp3", "m4a", "wav", "mp4"];

const MAX_FILES_PER_DROP = 10;

const Spinner: React.FC<{ size?: number }> = ({ size = 12 }) => (
  <span className="inline-flex animate-spin" style={{ lineHeight: 0 }}>
    <Loader2 size={size} />
  </span>
);

const isSupportedAudioPath = (path: string) => {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return AUDIO_EXTENSIONS.includes(ext);
};

const formatBytes = (bytes: number) => {
  if (bytes >= 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }
  if (bytes >= 1024) {
    return `${Math.round(bytes / 1024)} KB`;
  }
  return `${bytes} B`;
};

export const TranscriptsSettings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const osType = useOsType();
  const [entries, setEntries] = useState<UploadEntry[]>([]);
  const [progressById, setProgressById] = useState<Record<string, number>>({});
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [fullscreenId, setFullscreenId] = useState<string | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const [isAuthenticated, setIsAuthenticated] = useState<boolean | null>(null);
  const statusesRef = useRef<Record<string, UploadStatus>>({});

  const trackStatuses = useCallback(
    (list: UploadEntry[], notify: boolean) => {
      const previous = statusesRef.current;
      const next: Record<string, UploadStatus> = {};

      for (const entry of list) {
        next[entry.id] = entry.status;
        if (!notify) continue;

        const before = previous[entry.id];
        if (!before || before === entry.status) continue;

        if (entry.status === "completed") {
          toast.success(
            t("settings.transcripts.readyToast", { name: entry.file_name }),
          );
        } else if (entry.status === "failed" && before === "processing") {
          toast.error(
            t("settings.transcripts.failedToast", { name: entry.file_name }),
          );
        }
      }

      statusesRef.current = next;
    },
    [t],
  );

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

  const addFiles = useCallback(
    async (paths: string[]) => {
      const supported = paths.filter(isSupportedAudioPath);

      if (supported.length === 0) {
        toast.error(t("settings.transcripts.errors.unsupported"));
        return;
      }
      if (paths.length > supported.length) {
        toast.warning(t("settings.transcripts.errors.someSkipped"));
      }
      if (supported.length > MAX_FILES_PER_DROP) {
        toast.error(t("settings.transcripts.errors.tooMany"));
        return;
      }

      const result = await commands.uploadsAddFiles(supported);
      if (result.status !== "ok") {
        const key = `settings.transcripts.errors.${result.error}`;
        toast.error(
          i18n.exists(key) ? t(key) : t("settings.transcripts.errors.generic"),
        );
      }
    },
    [i18n, t],
  );

  useEffect(() => {
    let cancelled = false;

    commands
      .uploadsList()
      .then((list) => {
        if (cancelled) return;
        trackStatuses(list, false);
        setEntries(list);
      })
      .catch((error) => console.error("Failed to load uploads:", error));

    refreshAuth();

    const unlistenChanged = events.uploadsChangedPayload.listen((event) => {
      trackStatuses(event.payload.entries, true);
      setEntries(event.payload.entries);
    });
    const unlistenProgress = events.uploadProgressPayload.listen((event) => {
      setProgressById((prev) => ({
        ...prev,
        [event.payload.id]: event.payload.progress,
      }));
    });
    const unlistenAuth = listen("livestt://auth-changed", () => {
      refreshAuth();
    });

    return () => {
      cancelled = true;
      unlistenChanged.then((fn) => fn());
      unlistenProgress.then((fn) => fn());
      unlistenAuth.then((fn) => fn());
    };
  }, [refreshAuth, trackStatuses]);

  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "enter" || event.payload.type === "over") {
        setDragActive(true);
      } else if (event.payload.type === "leave") {
        setDragActive(false);
      } else if (event.payload.type === "drop") {
        setDragActive(false);
        addFiles(event.payload.paths);
      }
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, [addFiles]);

  useEffect(() => {
    if (!fullscreenId) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setFullscreenId(null);
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [fullscreenId]);

  const pickFiles = async () => {
    try {
      const selected = await open({
        multiple: true,
        filters: [
          {
            name: t("settings.transcripts.filePickerFilter"),
            extensions: AUDIO_EXTENSIONS,
          },
        ],
      });
      if (!selected) return;
      const paths = Array.isArray(selected) ? selected : [selected];
      await addFiles(paths);
    } catch (error) {
      console.error("Failed to pick files:", error);
    }
  };

  const copyTranscript = async (entry: UploadEntry) => {
    if (!entry.transcript) return;
    try {
      await navigator.clipboard.writeText(entry.transcript);
      toast.success(t("settings.transcripts.copied"));
    } catch (error) {
      console.error("Failed to copy transcript:", error);
    }
  };

  const cancelUpload = async (entry: UploadEntry) => {
    const result = await commands.uploadsCancel(entry.id);
    if (result.status !== "ok") {
      toast.error(t("settings.transcripts.errors.generic"));
    }
  };

  const retryUpload = async (entry: UploadEntry) => {
    const result = await commands.uploadsRetry(entry.id);
    if (result.status !== "ok") {
      const key = `settings.transcripts.errors.${result.error}`;
      toast.error(
        i18n.exists(key) ? t(key) : t("settings.transcripts.errors.generic"),
      );
    }
  };

  const deleteUpload = async (entry: UploadEntry) => {
    const confirmed = await ask(t("settings.transcripts.deleteLocalConfirm"), {
      title: t("settings.transcripts.delete"),
      kind: "warning",
    });
    if (!confirmed) return;
    const result = await commands.uploadsDelete(entry.id);
    if (result.status !== "ok") {
      toast.error(t("settings.transcripts.errors.deleteFailed"));
      return;
    }
    if (fullscreenId === entry.id) setFullscreenId(null);
    if (expandedId === entry.id) setExpandedId(null);
  };

  const loadAudioUrl = async (entry: UploadEntry): Promise<string | null> => {
    if (!entry.source_path) {
      toast.error(t("settings.transcripts.errors.audioUnavailable"));
      return null;
    }

    try {
      if (osType === "linux") {
        const fileData = await readFile(entry.source_path);
        const blob = new Blob([fileData], { type: "audio/*" });
        return URL.createObjectURL(blob);
      }

      return convertFileSrc(entry.source_path, "asset");
    } catch (error) {
      console.error("Failed to load transcript audio:", error);
      toast.error(t("settings.transcripts.errors.audioUnavailable"));
      return null;
    }
  };

  const fullscreenEntry = useMemo(
    () => entries.find((entry) => entry.id === fullscreenId) ?? null,
    [entries, fullscreenId],
  );

  const statusChip = (entry: UploadEntry) => {
    const styles: Record<UploadStatus, string> = {
      queued: "bg-slate-100 text-slate-600",
      uploading: "bg-sky-100 text-sky-700",
      processing: "bg-amber-100 text-amber-700",
      completed: "bg-emerald-100 text-emerald-700",
      failed: "bg-rose-100 text-rose-700",
    };

    return (
      <span
        className={`inline-flex shrink-0 items-center gap-1 rounded-full px-2.5 py-1 text-xs leading-none font-medium ${styles[entry.status]}`}
      >
        {(entry.status === "processing" || entry.status === "uploading") && (
          <Spinner size={12} />
        )}
        {t(`settings.transcripts.status.${entry.status}`)}
      </span>
    );
  };

  const errorLabel = (entry: UploadEntry) => {
    if (!entry.error) return null;
    const key = `settings.transcripts.errors.${entry.error}`;
    return i18n.exists(key) ? t(key) : entry.error;
  };

  const renderEntry = (entry: UploadEntry) => {
    const isExpanded = expandedId === entry.id;
    const isActive = entry.status === "queued" || entry.status === "uploading";
    const hasTranscript = Boolean(entry.transcript?.trim());
    const hasAudio = Boolean(entry.source_path);
    const canExpand = hasTranscript || hasAudio;
    const progress =
      entry.status === "uploading"
        ? (progressById[entry.id] ?? entry.progress)
        : entry.progress;

    return (
      <div key={entry.id} className="px-4 py-3">
        <div className="flex items-center gap-3">
          <FileAudio size={18} className="shrink-0 text-mid-gray" />
          <button
            type="button"
            className={`min-w-0 flex-1 text-left ${canExpand ? "cursor-pointer" : "cursor-default"}`}
            onClick={() =>
              canExpand && setExpandedId(isExpanded ? null : entry.id)
            }
          >
            <p className="truncate text-sm font-medium text-text">
              {entry.file_name}
            </p>
            <p className="truncate text-xs text-text/50">
              {new Date(entry.created_at_ms).toLocaleString(i18n.language)}
              {" · "}
              {formatBytes(entry.size_bytes)}
              {entry.status === "failed" && entry.error && (
                <span className="text-rose-600"> · {errorLabel(entry)}</span>
              )}
            </p>
          </button>

          {statusChip(entry)}

          <div className="flex shrink-0 items-center gap-1">
            {isActive && (
              <button
                type="button"
                className="rounded-md p-1.5 text-mid-gray hover:bg-slate-100 hover:text-text"
                title={t("settings.transcripts.cancel")}
                onClick={() => cancelUpload(entry)}
              >
                <X size={16} />
              </button>
            )}
            {entry.status === "failed" && (
              <button
                type="button"
                className="rounded-md p-1.5 text-mid-gray hover:bg-slate-100 hover:text-text"
                title={t("settings.transcripts.retry")}
                onClick={() => retryUpload(entry)}
              >
                <RefreshCw size={16} />
              </button>
            )}
            {!isActive && (
              <button
                type="button"
                className="rounded-md p-1.5 text-rose-500 hover:bg-rose-50"
                title={t("settings.transcripts.delete")}
                onClick={() => deleteUpload(entry)}
              >
                <Trash2 size={16} />
              </button>
            )}
            {canExpand && (
              <button
                type="button"
                className="rounded-md p-1.5 text-mid-gray hover:bg-slate-100 hover:text-text"
                title={
                  isExpanded
                    ? t("settings.transcripts.collapse")
                    : t("settings.transcripts.expand")
                }
                onClick={() => setExpandedId(isExpanded ? null : entry.id)}
              >
                {isExpanded ? (
                  <ChevronUp size={16} />
                ) : (
                  <ChevronDown size={16} />
                )}
              </button>
            )}
          </div>
        </div>

        {entry.status === "uploading" && (
          <div className="mt-2 flex items-center gap-2 ps-8">
            <div className="h-1 flex-1 overflow-hidden rounded-full bg-slate-200">
              <div
                className="h-1 rounded-full bg-sky-500 transition-[width] duration-200"
                style={{ width: `${progress}%` }}
              />
            </div>
            <span className="w-9 text-end text-xs tabular-nums text-text/50">
              {progress}%
            </span>
          </div>
        )}

        {isExpanded && canExpand && (
          <div className="mt-3 space-y-2 rounded-lg bg-slate-50 p-3 ps-4">
            {hasAudio && (
              <AudioPlayer
                onLoadRequest={() => loadAudioUrl(entry)}
                className="w-full"
                tone="red"
              />
            )}
            {hasTranscript ? (
              <p className="max-h-64 overflow-y-auto text-sm leading-6 whitespace-pre-wrap text-text/90">
                {entry.transcript}
              </p>
            ) : (
              <p className="text-sm text-text/50">
                {t("settings.transcripts.noTranscript")}
              </p>
            )}
            <div className="flex items-center gap-2 pt-1">
              {hasTranscript && (
                <>
                  <button
                    type="button"
                    className="flex items-center gap-1.5 rounded-md border border-mid-gray/30 px-2.5 py-1 text-xs font-medium text-text/70 hover:bg-white"
                    onClick={() => copyTranscript(entry)}
                  >
                    <Copy size={13} />
                    {t("settings.transcripts.copy")}
                  </button>
                  <button
                    type="button"
                    className="flex items-center gap-1.5 rounded-md border border-mid-gray/30 px-2.5 py-1 text-xs font-medium text-text/70 hover:bg-white"
                    onClick={() => setFullscreenId(entry.id)}
                  >
                    <Maximize2 size={13} />
                    {t("settings.transcripts.fullscreen")}
                  </button>
                </>
              )}
            </div>
          </div>
        )}
      </div>
    );
  };

  return (
    <div className="mx-auto w-full max-w-3xl space-y-6">
      {dragActive && (
        <div className="pointer-events-none fixed inset-0 z-40 flex items-center justify-center bg-background/90">
          <div className="flex flex-col items-center gap-3 rounded-2xl border-2 border-dashed border-sky-400 px-16 py-12">
            <CloudUpload size={40} className="text-sky-500" />
            <p className="text-lg font-medium text-text">
              {t("settings.transcripts.dropOverlay")}
            </p>
          </div>
        </div>
      )}

      <div className="space-y-2">
        <div className="px-4">
          <h2 className="text-xs font-medium tracking-wide text-mid-gray uppercase">
            {t("settings.transcripts.title")}
          </h2>
        </div>

        {isAuthenticated === false && (
          <div className="rounded-lg border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-800">
            {t("settings.transcripts.authRequired")}
          </div>
        )}

        <button
          type="button"
          className="w-full cursor-pointer rounded-lg border-2 border-dashed border-mid-gray/30 bg-background px-4 py-8 text-center transition hover:border-sky-400 hover:bg-sky-50/40"
          onClick={pickFiles}
        >
          <CloudUpload size={28} className="mx-auto text-mid-gray" />
          <p className="mt-2 text-sm font-medium text-text">
            {t("settings.transcripts.dropTitle")}
          </p>
          <p className="mt-1 text-xs text-text/50">
            {t("settings.transcripts.dropHint")}
          </p>
        </button>

        <div className="overflow-visible rounded-lg border border-mid-gray/20 bg-background">
          {entries.length === 0 ? (
            <div className="px-4 py-6 text-center text-sm text-text/60">
              {t("settings.transcripts.empty")}
            </div>
          ) : (
            <div className="divide-y divide-mid-gray/20">
              {entries.map(renderEntry)}
            </div>
          )}
        </div>
      </div>

      {fullscreenEntry && (
        <div className="fixed inset-0 z-50 flex flex-col bg-background">
          <div className="flex h-14 shrink-0 items-center gap-3 border-b border-mid-gray/20 px-4">
            <FileAudio size={18} className="shrink-0 text-mid-gray" />
            <p className="min-w-0 flex-1 truncate text-sm font-medium text-text">
              {fullscreenEntry.file_name}
            </p>
            <button
              type="button"
              className="flex items-center gap-1.5 rounded-md border border-mid-gray/30 px-2.5 py-1.5 text-xs font-medium text-text/70 hover:bg-slate-100"
              onClick={() => copyTranscript(fullscreenEntry)}
            >
              <Copy size={14} />
              {t("settings.transcripts.copy")}
            </button>
            <button
              type="button"
              className="rounded-md p-2 text-mid-gray hover:bg-slate-100 hover:text-text"
              title={t("settings.transcripts.close")}
              onClick={() => setFullscreenId(null)}
            >
              <X size={18} />
            </button>
          </div>
          {fullscreenEntry.source_path && (
            <div className="shrink-0 border-b border-mid-gray/20 px-4 py-2">
              <div className="mx-auto w-full max-w-3xl">
                <AudioPlayer
                  onLoadRequest={() => loadAudioUrl(fullscreenEntry)}
                  className="w-full"
                  tone="red"
                />
              </div>
            </div>
          )}
          <div className="flex-1 overflow-y-auto">
            <div className="mx-auto w-full max-w-3xl px-6 py-6">
              <p className="text-base leading-8 whitespace-pre-wrap text-text">
                {fullscreenEntry.transcript}
              </p>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};
