import { Plus, Trash2 } from "lucide-react";
import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type LiveSttGeneralEntry } from "@/bindings";
import { useSettings } from "../../../hooks/useSettings";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { SettingContainer } from "../../ui/SettingContainer";
import { TagInput, type TagInputAddRejection } from "../../ui/TagInput";
import { Textarea } from "../../ui/Textarea";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import {
  MAX_FINALIZE_TIMEOUT_MS,
  MAX_LIVESTT_GENERAL_ENTRIES,
  MAX_LIVESTT_GENERAL_KEY_CHARS,
  MAX_LIVESTT_GENERAL_VALUE_CHARS,
  MAX_LIVESTT_TERM_CHARS,
  MAX_LIVESTT_TERMS,
  MAX_LIVESTT_TEXT_CHARS,
  MIN_FINALIZE_TIMEOUT_MS,
  normalizeLiveSttGeneralEntries,
  normalizeLiveSttTextInput,
  parseConsultationIdInput,
  validateFinalizeTimeoutInput,
  type GeneralValidationError,
} from "./livesttValidation";

interface LiveSttAdvancedSettingsProps {
  consultationId: string;
  dictationEnabled: boolean;
  finalizeTimeoutMs: number;
  text: string;
  terms: string[];
  general: LiveSttGeneralEntry[];
}

type FieldElement = HTMLInputElement | HTMLTextAreaElement;

function useSyncedTextField<TElement extends FieldElement>(
  externalValue: string,
) {
  const [value, setValue] = useState(externalValue);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setValue(externalValue);
  }, [externalValue]);

  const onChange = useCallback((event: React.ChangeEvent<TElement>) => {
    setValue(event.target.value);
    setError(null);
  }, []);

  return {
    value,
    setValue,
    error,
    setError,
    onChange,
  };
}

const entriesEqual = (
  a: readonly LiveSttGeneralEntry[],
  b: readonly LiveSttGeneralEntry[],
): boolean => {
  if (a.length !== b.length) {
    return false;
  }
  for (let i = 0; i < a.length; i += 1) {
    if (a[i].key !== b[i].key || a[i].value !== b[i].value) {
      return false;
    }
  }
  return true;
};

export const LiveSttAdvancedSettings: React.FC<
  LiveSttAdvancedSettingsProps
> = ({
  consultationId,
  dictationEnabled,
  finalizeTimeoutMs,
  text,
  terms,
  general,
}) => {
  const { t } = useTranslation();
  const { isUpdating, updateSetting, refreshSettings } = useSettings();
  const [termsError, setTermsError] = useState<string | null>(null);

  const consultationIdField =
    useSyncedTextField<HTMLInputElement>(consultationId);

  const finalizeTimeoutField = useSyncedTextField<HTMLInputElement>(
    String(finalizeTimeoutMs),
  );

  const textField = useSyncedTextField<HTMLTextAreaElement>(text);

  const [generalRows, setGeneralRows] =
    useState<LiveSttGeneralEntry[]>(general);
  const [generalError, setGeneralError] = useState<{
    error: GeneralValidationError;
    rowIndex: number | null;
    key: string | null;
  } | null>(null);

  useEffect(() => {
    setGeneralRows(general);
    setGeneralError(null);
  }, [general]);

  const handleConsultationIdBlur = useCallback(async () => {
    const parsedValue = parseConsultationIdInput(consultationIdField.value);

    if (parsedValue === null) {
      consultationIdField.setError(
        t("settings.transcriptionBackend.livestt.consultationId.error"),
      );
      return;
    }

    consultationIdField.setError(null);

    if (parsedValue !== consultationIdField.value) {
      consultationIdField.setValue(parsedValue);
    }

    if (parsedValue === consultationId) {
      return;
    }

    await updateSetting(
      "livestt_consultation_id",
      parsedValue === "" ? null : parsedValue,
    );
  }, [consultationId, consultationIdField, t, updateSetting]);

  const handleTextBlur = useCallback(async () => {
    const { trimmed, isValid } = normalizeLiveSttTextInput(textField.value);

    if (!isValid) {
      textField.setError(t("settings.transcriptionBackend.livestt.text.error"));
      return;
    }

    textField.setError(null);

    if (trimmed === text) {
      if (trimmed !== textField.value) {
        textField.setValue(trimmed);
      }

      return;
    }

    const result = await commands.changeLivesttTextSetting(
      trimmed === "" ? null : trimmed,
    );

    if (result.status === "error") {
      textField.setError(result.error);
      return;
    }

    textField.setValue(trimmed);
    await refreshSettings();
  }, [text, textField, refreshSettings, t]);

  const handleFinalizeTimeoutBlur = useCallback(async () => {
    const parsedValue = validateFinalizeTimeoutInput(
      finalizeTimeoutField.value,
    );

    if (parsedValue === null) {
      finalizeTimeoutField.setError(
        t("settings.transcriptionBackend.livestt.finalizeTimeout.error"),
      );
      return;
    }

    finalizeTimeoutField.setError(null);

    const normalizedValue = String(parsedValue);

    if (normalizedValue !== finalizeTimeoutField.value) {
      finalizeTimeoutField.setValue(normalizedValue);
    }

    if (parsedValue === finalizeTimeoutMs) {
      return;
    }

    await updateSetting("livestt_finalize_timeout_ms", parsedValue);
  }, [finalizeTimeoutField, finalizeTimeoutMs, t, updateSetting]);

  const textCharCount = [...textField.value].length;

  const handleTermsChange = useCallback(
    async (next: string[]) => {
      setTermsError(null);
      await updateSetting("livestt_terms", next);
    },
    [updateSetting],
  );

  const handleTermsAddRejected = useCallback(
    (reason: TagInputAddRejection) => {
      if (reason === "empty") {
        return;
      }
      setTermsError(
        t(`settings.transcriptionBackend.livestt.terms.errors.${reason}`),
      );
    },
    [t],
  );

  const updateGeneralRow = useCallback(
    (index: number, field: "key" | "value", nextValue: string) => {
      setGeneralRows((prev) => {
        const next = prev.slice();
        next[index] = { ...next[index], [field]: nextValue };
        return next;
      });
      setGeneralError(null);
    },
    [],
  );

  const commitGeneralRows = useCallback(
    async (rows: LiveSttGeneralEntry[]) => {
      const validation = normalizeLiveSttGeneralEntries(rows);

      if (validation.error) {
        setGeneralError({
          error: validation.error,
          rowIndex: validation.errorRowIndex,
          key: validation.errorKey,
        });
        return false;
      }

      setGeneralError(null);

      if (entriesEqual(validation.entries, general)) {
        return true;
      }

      const result = await commands.changeLivesttGeneralSetting(
        validation.entries,
      );

      if (result.status === "error") {
        setGeneralError({
          error: "tooMany",
          rowIndex: null,
          key: null,
        });
        return false;
      }

      await refreshSettings();
      return true;
    },
    [general, refreshSettings],
  );

  const handleGeneralBlur = useCallback(async () => {
    await commitGeneralRows(generalRows);
  }, [commitGeneralRows, generalRows]);

  const handleGeneralRemove = useCallback(
    async (index: number) => {
      const next = generalRows.slice();
      next.splice(index, 1);
      setGeneralRows(next);
      await commitGeneralRows(next);
    },
    [commitGeneralRows, generalRows],
  );

  const handleGeneralAdd = useCallback(() => {
    if (generalRows.length >= MAX_LIVESTT_GENERAL_ENTRIES) {
      return;
    }
    setGeneralRows((prev) => [...prev, { key: "", value: "" }]);
    setGeneralError(null);
  }, [generalRows.length]);

  const generalErrorMessage = (() => {
    if (!generalError) {
      return null;
    }
    if (generalError.error === "duplicateKey" && generalError.key !== null) {
      return t(
        "settings.transcriptionBackend.livestt.general.errors.duplicateKey",
        { key: generalError.key },
      );
    }
    return t(
      `settings.transcriptionBackend.livestt.general.errors.${generalError.error}`,
    );
  })();

  const isGeneralUpdating = isUpdating("livestt_general");
  const canAddGeneral =
    generalRows.length < MAX_LIVESTT_GENERAL_ENTRIES && !isGeneralUpdating;

  return (
    <>
      <ToggleSwitch
        checked={dictationEnabled}
        onChange={(enabled) =>
          updateSetting("livestt_dictation_enabled", enabled)
        }
        isUpdating={isUpdating("livestt_dictation_enabled")}
        label={t("settings.transcriptionBackend.livestt.dictation.title")}
        description={t(
          "settings.transcriptionBackend.livestt.dictation.description",
        )}
        grouped
      />

      <SettingContainer
        title={t("settings.transcriptionBackend.livestt.text.title")}
        description={t(
          "settings.transcriptionBackend.livestt.text.description",
        )}
        descriptionMode="tooltip"
        grouped
        layout="stacked"
      >
        <Textarea
          value={textField.value}
          onChange={textField.onChange}
          onBlur={() => {
            void handleTextBlur();
          }}
          placeholder={t(
            "settings.transcriptionBackend.livestt.text.placeholder",
          )}
          disabled={isUpdating("livestt_text")}
          maxLength={MAX_LIVESTT_TEXT_CHARS}
          className="w-full"
        />

        <div className="mt-1 flex items-center justify-between text-xs text-slate-500">
          <span>
            {textField.error && (
              <span className="text-red-500">{textField.error}</span>
            )}
          </span>

          <span>
            {textCharCount}/{MAX_LIVESTT_TEXT_CHARS}
          </span>
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.transcriptionBackend.livestt.terms.title")}
        description={t(
          "settings.transcriptionBackend.livestt.terms.description",
        )}
        descriptionMode="tooltip"
        grouped
        layout="stacked"
      >
        <TagInput
          value={terms}
          onChange={(next) => {
            void handleTermsChange(next);
          }}
          onAddRejected={handleTermsAddRejected}
          placeholder={t(
            "settings.transcriptionBackend.livestt.terms.placeholder",
          )}
          disabled={isUpdating("livestt_terms")}
          maxTermLength={MAX_LIVESTT_TERM_CHARS}
          maxTerms={MAX_LIVESTT_TERMS}
          removeAriaLabel={t(
            "settings.transcriptionBackend.livestt.terms.remove",
          )}
        />

        <div className="mt-1 flex items-center justify-between text-xs text-slate-500">
          <span>
            {termsError && <span className="text-red-500">{termsError}</span>}
          </span>

          <span>
            {terms.length}/{MAX_LIVESTT_TERMS}
          </span>
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.transcriptionBackend.livestt.general.title")}
        description={t(
          "settings.transcriptionBackend.livestt.general.description",
        )}
        descriptionMode="tooltip"
        grouped
        layout="stacked"
      >
        <div className="flex flex-col gap-2">
          {generalRows.map((row, index) => (
            <div
              key={index}
              className="grid grid-cols-[1fr_1fr_auto] items-center gap-2"
            >
              <Input
                value={row.key}
                onChange={(event) =>
                  updateGeneralRow(index, "key", event.target.value)
                }
                onBlur={() => {
                  void handleGeneralBlur();
                }}
                placeholder={t(
                  "settings.transcriptionBackend.livestt.general.keyPlaceholder",
                )}
                disabled={isGeneralUpdating}
                maxLength={MAX_LIVESTT_GENERAL_KEY_CHARS}
              />
              <Input
                value={row.value}
                onChange={(event) =>
                  updateGeneralRow(index, "value", event.target.value)
                }
                onBlur={() => {
                  void handleGeneralBlur();
                }}
                placeholder={t(
                  "settings.transcriptionBackend.livestt.general.valuePlaceholder",
                )}
                disabled={isGeneralUpdating}
                maxLength={MAX_LIVESTT_GENERAL_VALUE_CHARS}
              />
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => {
                  void handleGeneralRemove(index);
                }}
                disabled={isGeneralUpdating}
                aria-label={t(
                  "settings.transcriptionBackend.livestt.general.removeAria",
                )}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          ))}

          <div>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={handleGeneralAdd}
              disabled={!canAddGeneral}
              className="inline-flex items-center justify-center gap-1 leading-none"
            >
              <Plus className="h-4 w-4 shrink-0" />
              <span>
                {t("settings.transcriptionBackend.livestt.general.addButton")}
              </span>
            </Button>
          </div>
        </div>

        <div className="mt-1 flex items-center justify-between text-xs text-slate-500">
          <span>
            {generalErrorMessage && (
              <span className="text-red-500">{generalErrorMessage}</span>
            )}
          </span>

          <span>
            {generalRows.length}/{MAX_LIVESTT_GENERAL_ENTRIES}
          </span>
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.transcriptionBackend.livestt.consultationId.title")}
        description={t(
          "settings.transcriptionBackend.livestt.consultationId.description",
        )}
        descriptionMode="tooltip"
        grouped
        layout="stacked"
      >
        <Input
          type="number"
          min="1"
          step="1"
          value={consultationIdField.value}
          onChange={consultationIdField.onChange}
          onBlur={() => {
            void handleConsultationIdBlur();
          }}
          placeholder={t(
            "settings.transcriptionBackend.livestt.consultationId.placeholder",
          )}
          disabled={isUpdating("livestt_consultation_id")}
          className="w-full"
        />

        {consultationIdField.error && (
          <p className="mt-2 text-xs text-red-500">
            {consultationIdField.error}
          </p>
        )}
      </SettingContainer>

      <SettingContainer
        title={t("settings.transcriptionBackend.livestt.finalizeTimeout.title")}
        description={t(
          "settings.transcriptionBackend.livestt.finalizeTimeout.description",
        )}
        descriptionMode="tooltip"
        grouped
        layout="stacked"
      >
        <Input
          type="number"
          min={MIN_FINALIZE_TIMEOUT_MS}
          max={MAX_FINALIZE_TIMEOUT_MS}
          step="100"
          value={finalizeTimeoutField.value}
          onChange={finalizeTimeoutField.onChange}
          onBlur={() => {
            void handleFinalizeTimeoutBlur();
          }}
          disabled={isUpdating("livestt_finalize_timeout_ms")}
          className="w-full"
        />

        {finalizeTimeoutField.error && (
          <p className="mt-2 text-xs text-red-500">
            {finalizeTimeoutField.error}
          </p>
        )}
      </SettingContainer>
    </>
  );
};
