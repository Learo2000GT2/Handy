import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import {
  commands,
  type FileTranscriptionEngine,
  type LemonadeCheck,
  type LemonadeModel,
  type LemonadeSettings,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useSettingsStore } from "@/stores/settingsStore";
import {
  DEFAULT_LEMONADE_SETTINGS,
  isLoopbackUrl,
  isolateLtr,
  isValidServerUrl,
  lemonadeErrorKey,
} from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Dropdown } from "../../ui/Dropdown";
import { Input } from "../../ui/Input";
import { Select, type SelectOption } from "../../ui/Select";
import { SettingContainer } from "../../ui/SettingContainer";
import { SettingsGroup } from "../../ui/SettingsGroup";

type CheckState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "done"; check: LemonadeCheck; baseUrl: string }
  /** The check itself could not run (not a server problem). */
  | { phase: "failed"; error: string };

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

/**
 * Chooses the engine for file transcription: Handy's selected model or a
 * Lemonade Server, with the server's address, API key and model. Dictation is
 * not affected.
 */
export const EngineCard: React.FC = () => {
  const { t } = useTranslation();
  const { settings, updateSetting } = useSettings();
  const engine: FileTranscriptionEngine =
    settings?.file_transcription_engine ?? "handy";
  const lemonade = settings?.lemonade ?? DEFAULT_LEMONADE_SETTINGS;

  const [urlDraft, setUrlDraft] = useState(lemonade.base_url);
  const [keyDraft, setKeyDraft] = useState(lemonade.api_key);
  const [urlInvalid, setUrlInvalid] = useState(false);
  const [check, setCheck] = useState<CheckState>({ phase: "idle" });
  // Transcription models from the last finished check; kept while the next
  // check runs so the model list doesn't empty and refill.
  const [models, setModels] = useState<LemonadeModel[]>([]);
  // Only the newest check may update the card.
  const checkIdRef = useRef(0);

  // Follow saved values (another window, or a rejected save rolling back;
  // handleUrlBlur then puts the refused address back).
  useEffect(() => {
    setUrlDraft(lemonade.base_url);
    setUrlInvalid(false);
  }, [lemonade.base_url]);
  useEffect(() => setKeyDraft(lemonade.api_key), [lemonade.api_key]);

  const runCheck = useCallback(async (baseUrl: string, apiKey: string) => {
    const id = ++checkIdRef.current;
    setCheck({ phase: "checking" });
    let next: CheckState;
    try {
      const result = await commands.checkLemonadeServer(baseUrl, apiKey);
      next =
        result.status === "ok"
          ? { phase: "done", check: result.data, baseUrl }
          : { phase: "failed", error: result.error };
    } catch (error) {
      next = { phase: "failed", error: errorText(error) };
    }
    if (id !== checkIdRef.current) return;
    setCheck(next);
    setModels(next.phase === "done" && next.check.ok ? next.check.models : []);
  }, []);

  // Check once when Lemonade is shown, and again whenever the address or key
  // changes, so the model list matches the server.
  const checkable =
    settings !== null &&
    engine === "lemonade" &&
    isValidServerUrl(lemonade.base_url);
  useEffect(() => {
    if (!checkable) {
      setCheck({ phase: "idle" });
      setModels([]);
      return;
    }
    void runCheck(lemonade.base_url, lemonade.api_key);
    return () => {
      checkIdRef.current += 1;
    };
  }, [checkable, lemonade.base_url, lemonade.api_key, runCheck]);

  /**
   * Saves a change on top of the latest stored Lemonade settings. Resolves
   * false when the backend refused it.
   */
  const saveLemonade = async (
    change: Partial<LemonadeSettings>,
  ): Promise<boolean> => {
    const current =
      useSettingsStore.getState().settings?.lemonade ??
      DEFAULT_LEMONADE_SETTINGS;
    const next = { ...current, ...change };
    if (
      next.base_url === current.base_url &&
      next.api_key === current.api_key &&
      next.model === current.model
    ) {
      return true;
    }
    return updateSetting("lemonade", next);
  };

  const handleUrlBlur = async () => {
    const value = urlDraft.trim();
    if (!isValidServerUrl(value)) {
      setUrlInvalid(true);
      return;
    }
    setUrlDraft(value);
    if (!(await saveLemonade({ base_url: value }))) {
      // The rollback has put the saved address back in the field: keep what
      // was typed so it can be corrected, and say what is wrong with it.
      setUrlDraft(value);
      setUrlInvalid(true);
    }
  };

  const handleKeyBlur = () => {
    const value = keyDraft.trim();
    setKeyDraft(value);
    void saveLemonade({ api_key: value });
  };

  const handleTest = () => {
    const current =
      useSettingsStore.getState().settings?.lemonade ??
      DEFAULT_LEMONADE_SETTINGS;
    if (!isValidServerUrl(current.base_url)) {
      setUrlInvalid(true);
      return;
    }
    void runCheck(current.base_url, current.api_key);
  };

  const engineOptions = [
    { value: "handy", label: t("settings.transcribeFiles.engine.handy") },
    { value: "lemonade", label: t("settings.transcribeFiles.engine.lemonade") },
  ];

  const engineRow = (
    <SettingContainer
      title={t("settings.transcribeFiles.engine.label")}
      description={t("settings.transcribeFiles.engine.description")}
      descriptionMode="tooltip"
      grouped={true}
    >
      <Dropdown
        options={engineOptions}
        selectedValue={engine}
        onSelect={(value) =>
          void updateSetting(
            "file_transcription_engine",
            value as FileTranscriptionEngine,
          )
        }
        disabled={settings === null}
      />
    </SettingContainer>
  );

  if (engine !== "lemonade") {
    return (
      <SettingsGroup title={t("settings.transcribeFiles.engine.title")}>
        {engineRow}
      </SettingsGroup>
    );
  }

  const modelOptions: SelectOption[] = models.map((model) => ({
    value: model.id,
    label:
      model.downloaded === false
        ? t("settings.transcribeFiles.lemonade.model.notDownloaded", {
            model: model.id,
          })
        : model.id,
  }));

  let status: React.ReactNode = null;
  if (urlInvalid) {
    status = (
      <p className="text-xs text-red-400">
        {t("settings.transcribeFiles.lemonade.errors.invalidUrl")}
      </p>
    );
  } else if (check.phase === "checking") {
    status = (
      <p className="text-xs text-text/60 flex items-center gap-1.5">
        <Loader2 className="w-3.5 h-3.5 animate-spin" aria-hidden="true" />
        <span>{t("settings.transcribeFiles.lemonade.testing")}</span>
      </p>
    );
  } else if (check.phase === "failed") {
    status = (
      <p className="text-xs text-red-400">
        {t("settings.transcribeFiles.lemonade.checkFailed", {
          error: check.error,
        })}
      </p>
    );
  } else if (check.phase === "done" && !check.check.ok) {
    status = (
      <p
        className="text-xs text-red-400"
        title={check.check.detail ?? undefined}
      >
        {t(
          `settings.transcribeFiles.lemonade.errors.${lemonadeErrorKey(
            check.check.error ?? "bad_response",
          )}`,
          { url: isolateLtr(check.baseUrl) },
        )}
      </p>
    );
  } else if (check.phase === "done" && models.length === 0) {
    status = (
      <p className="text-xs text-yellow-500">
        {t("settings.transcribeFiles.lemonade.model.none")}
      </p>
    );
  } else if (check.phase === "done") {
    status = (
      <p
        className="text-xs text-green-500"
        title={check.check.api_url ?? undefined}
      >
        {t("settings.transcribeFiles.lemonade.connected", {
          number: models.length,
        })}
      </p>
    );
  }

  const remote =
    isValidServerUrl(lemonade.base_url) && !isLoopbackUrl(lemonade.base_url);

  return (
    <SettingsGroup title={t("settings.transcribeFiles.engine.title")}>
      {engineRow}

      <SettingContainer
        title={t("settings.transcribeFiles.lemonade.url.label")}
        description={t("settings.transcribeFiles.lemonade.url.description")}
        descriptionMode="tooltip"
        grouped={true}
      >
        <Input
          type="url"
          value={urlDraft}
          onChange={(event) => {
            setUrlDraft(event.target.value);
            setUrlInvalid(false);
          }}
          onBlur={() => void handleUrlBlur()}
          onKeyDown={(event) => {
            if (event.key === "Enter") event.currentTarget.blur();
          }}
          placeholder={DEFAULT_LEMONADE_SETTINGS.base_url}
          aria-label={t("settings.transcribeFiles.lemonade.url.label")}
          aria-invalid={urlInvalid}
          spellCheck={false}
          autoComplete="off"
          variant="compact"
          className="w-72"
        />
      </SettingContainer>

      <SettingContainer
        title={t("settings.transcribeFiles.lemonade.apiKey.label")}
        description={t("settings.transcribeFiles.lemonade.apiKey.description")}
        descriptionMode="tooltip"
        grouped={true}
      >
        <Input
          type="password"
          value={keyDraft}
          onChange={(event) => setKeyDraft(event.target.value)}
          onBlur={handleKeyBlur}
          onKeyDown={(event) => {
            if (event.key === "Enter") event.currentTarget.blur();
          }}
          placeholder={t(
            "settings.transcribeFiles.lemonade.apiKey.placeholder",
          )}
          aria-label={t("settings.transcribeFiles.lemonade.apiKey.label")}
          autoComplete="off"
          variant="compact"
          className="w-72"
        />
      </SettingContainer>

      <SettingContainer
        title={t("settings.transcribeFiles.lemonade.model.label")}
        description={t("settings.transcribeFiles.lemonade.model.description")}
        descriptionMode="tooltip"
        grouped={true}
      >
        <Select
          className="w-72 text-sm"
          value={lemonade.model || null}
          options={modelOptions}
          onChange={(value) => void saveLemonade({ model: value ?? "" })}
          isCreatable
          onCreateOption={(value) => {
            const model = value.trim();
            if (model !== "") void saveLemonade({ model });
          }}
          formatCreateLabel={(input) =>
            t("settings.transcribeFiles.lemonade.model.use", {
              model: input.trim(),
            })
          }
          isClearable={false}
          isLoading={check.phase === "checking"}
          placeholder={t("settings.transcribeFiles.lemonade.model.placeholder")}
        />
      </SettingContainer>

      <div className="px-4 py-3 flex flex-wrap items-center gap-x-3 gap-y-2">
        <Button
          type="button"
          variant="secondary"
          size="sm"
          onClick={handleTest}
        >
          {t("settings.transcribeFiles.lemonade.test")}
        </Button>
        <div role="status" aria-live="polite" className="min-w-0">
          {status}
        </div>
      </div>

      {remote && (
        <Alert variant="warning" contained>
          {t("settings.transcribeFiles.lemonade.notes.remote")}
        </Alert>
      )}
      {settings?.translate_to_english && (
        <Alert variant="info" contained>
          {t("settings.transcribeFiles.lemonade.notes.noTranslate")}
        </Alert>
      )}
    </SettingsGroup>
  );
};
