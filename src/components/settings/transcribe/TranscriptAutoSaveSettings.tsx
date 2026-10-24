import React from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import type {
  TranscriptAutoSave,
  TranscriptSaveFormat,
  TranscriptSaveLocation,
} from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useSettingsStore } from "@/stores/settingsStore";
import {
  DEFAULT_TRANSCRIPT_AUTOSAVE,
  isolateLtr,
} from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";
import { Dropdown } from "../../ui/Dropdown";
import { SettingContainer } from "../../ui/SettingContainer";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { ToggleSwitch } from "../../ui/ToggleSwitch";

const FORMATS: readonly TranscriptSaveFormat[] = ["docx", "txt", "md"];

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

/**
 * Whether finished transcripts (files and meetings) are saved as files
 * automatically, where (next to the recording or in a chosen folder) and
 * in which format.
 */
export const TranscriptAutoSaveSettings: React.FC = () => {
  const { t } = useTranslation();
  const { settings, updateSetting } = useSettings();
  const autosave = settings?.transcript_autosave ?? DEFAULT_TRANSCRIPT_AUTOSAVE;
  const disabled = settings === null;
  const inFolder = autosave.location === "folder" && autosave.folder !== "";

  /** Saves a change on top of the latest stored auto-save settings. */
  const save = (change: Partial<TranscriptAutoSave>): Promise<boolean> => {
    const current =
      useSettingsStore.getState().settings?.transcript_autosave ??
      DEFAULT_TRANSCRIPT_AUTOSAVE;
    return updateSetting("transcript_autosave", { ...current, ...change });
  };

  const chooseFolder = async () => {
    try {
      const folder = await open({
        directory: true,
        multiple: false,
        title: t("settings.transcribeFiles.autosave.folderPicker"),
        defaultPath: autosave.folder || undefined,
      });
      if (typeof folder !== "string") return;
      await save({ location: "folder", folder });
    } catch (error) {
      toast.error(
        t("settings.transcribeFiles.autosave.saveError", {
          error: errorText(error),
        }),
      );
    }
  };

  const handleLocation = (value: string) => {
    const location = value as TranscriptSaveLocation;
    if (location === "folder" && autosave.folder === "") {
      // Nothing chosen yet: ask, and stay next to the recording if cancelled.
      void chooseFolder();
      return;
    }
    void save({ location });
  };

  const locationOptions = [
    {
      value: "next_to_recording",
      label: t("settings.transcribeFiles.autosave.location.nextToRecording"),
    },
    {
      value: "folder",
      label: t("settings.transcribeFiles.autosave.location.folder"),
    },
  ];
  const formatOptions = FORMATS.map((format) => ({
    value: format,
    label: t(`settings.transcribeFiles.export.formats.${format}`),
  }));

  return (
    <SettingsGroup title={t("settings.transcribeFiles.autosave.title")}>
      <ToggleSwitch
        checked={autosave.enabled}
        onChange={(enabled) => void save({ enabled })}
        label={t("settings.transcribeFiles.autosave.enabled.label")}
        description={t("settings.transcribeFiles.autosave.enabled.description")}
        descriptionMode="tooltip"
        grouped={true}
        disabled={disabled}
      />
      {autosave.enabled && (
        <>
          <SettingContainer
            title={t("settings.transcribeFiles.autosave.location.label")}
            description={t(
              "settings.transcribeFiles.autosave.location.description",
            )}
            descriptionMode="tooltip"
            grouped={true}
          >
            <Dropdown
              options={locationOptions}
              selectedValue={inFolder ? "folder" : "next_to_recording"}
              onSelect={handleLocation}
              disabled={disabled}
            />
          </SettingContainer>
          {inFolder && (
            <div className="px-4 py-2 flex items-center gap-2">
              <p className="min-w-0 flex-1 text-xs font-mono break-all text-text/70 select-text cursor-text">
                {isolateLtr(autosave.folder)}
              </p>
              <Button
                type="button"
                variant="secondary"
                size="sm"
                onClick={() => void chooseFolder()}
                disabled={disabled}
              >
                {t("settings.transcribeFiles.autosave.chooseFolder")}
              </Button>
            </div>
          )}
          <SettingContainer
            title={t("settings.transcribeFiles.autosave.format.label")}
            description={t(
              "settings.transcribeFiles.autosave.format.description",
            )}
            descriptionMode="tooltip"
            grouped={true}
          >
            <Dropdown
              options={formatOptions}
              selectedValue={autosave.format}
              onSelect={(value) =>
                void save({ format: value as TranscriptSaveFormat })
              }
              disabled={disabled}
            />
          </SettingContainer>
          <p className="px-4 py-2 text-xs text-text/60">
            {t("settings.transcribeFiles.autosave.note")}
          </p>
        </>
      )}
    </SettingsGroup>
  );
};
