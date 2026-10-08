import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { FileJobSnapshot, ModelInfo } from "@/bindings";
import { useSettings } from "@/hooks/useSettings";
import { useModelStore } from "@/stores/modelStore";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import { getTranslatedModelName } from "@/lib/utils/modelTranslation";
import { isFinishedStatus, notReadyKey } from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { EngineCard } from "./EngineCard";
import { FileDropZone } from "./FileDropZone";
import { FileJobRow } from "./FileJobRow";
import { SpeakerLabelsToggle } from "./SpeakerLabelsToggle";
import { TranscriptDialog } from "./TranscriptDialog";

interface ViewedJob {
  jobId: number;
  fileName: string;
}

export const TranscribeFilesSettings: React.FC = () => {
  const { t } = useTranslation();
  const { models, currentModel, loading: modelsLoading } = useModelStore();
  const { settings, isLoading: settingsLoading } = useSettings();
  const jobs = useFileTranscriptionStore((state) => state.jobs);
  const order = useFileTranscriptionStore((state) => state.order);
  const noSpeech = useFileTranscriptionStore((state) => state.noSpeech);
  const saveToHistory = useFileTranscriptionStore(
    (state) => state.saveToHistory,
  );
  const setSaveToHistory = useFileTranscriptionStore(
    (state) => state.setSaveToHistory,
  );
  const refreshFormats = useFileTranscriptionStore(
    (state) => state.refreshFormats,
  );
  const cancelAll = useFileTranscriptionStore((state) => state.cancelAll);
  const clearFinished = useFileTranscriptionStore(
    (state) => state.clearFinished,
  );
  const [viewed, setViewed] = useState<ViewedJob | null>(null);

  // ffmpeg may have been installed since the list was loaded.
  useEffect(() => {
    void refreshFormats();
  }, [refreshFormats]);

  const modelInfo = models.find(
    (model: ModelInfo) => model.id === currentModel,
  );
  const modelReady = modelInfo?.is_downloaded ?? false;
  const lemonade = settings?.file_transcription_engine === "lemonade";
  const lemonadeModel = settings?.lemonade?.model ?? "";
  const notReady = notReadyKey(
    lemonade ? "lemonade" : "handy",
    lemonadeModel,
    modelReady,
  );
  const notReadyText =
    notReady === null ? null : t(`settings.transcribeFiles.${notReady}`);
  // The page alert waits for what it depends on, so it doesn't flash.
  const notReadyMessage = (lemonade ? settingsLoading : modelsLoading)
    ? null
    : notReadyText;

  const queue = order
    .map((id) => jobs[id])
    .filter((job): job is FileJobSnapshot => job !== undefined);
  const hasActive = queue.some((job) => !isFinishedStatus(job.status));
  const hasFinished = queue.some((job) => isFinishedStatus(job.status));

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div>
        <h1 className="text-xl font-semibold mb-2">
          {t("settings.transcribeFiles.title")}
        </h1>
        <p className="text-sm text-text/60">
          {t("settings.transcribeFiles.description")}
        </p>
        <p className="mt-2 text-xs text-text/50">
          {lemonade
            ? t("settings.transcribeFiles.settingsNoteLemonade")
            : t("settings.transcribeFiles.settingsNote")}
        </p>
        {lemonade
          ? lemonadeModel !== "" && (
              <p className="text-xs text-text/50">
                {t("settings.transcribeFiles.modelLemonade", {
                  model: lemonadeModel,
                })}
              </p>
            )
          : modelInfo &&
            modelReady && (
              <p className="text-xs text-text/50">
                {t("settings.transcribeFiles.model", {
                  model: getTranslatedModelName(modelInfo, t),
                })}
              </p>
            )}
      </div>

      <EngineCard />

      {notReadyMessage !== null && (
        <Alert variant="warning">{notReadyMessage}</Alert>
      )}

      <FileDropZone disabledReason={notReadyText} />

      <SettingsGroup>
        <ToggleSwitch
          checked={saveToHistory}
          onChange={setSaveToHistory}
          label={t("settings.transcribeFiles.saveToHistory.label")}
          description={t("settings.transcribeFiles.saveToHistory.description")}
          descriptionMode="tooltip"
          grouped={true}
        />
        <SpeakerLabelsToggle />
      </SettingsGroup>

      {queue.length > 0 && (
        <div className="space-y-2">
          <div className="px-4 flex items-center justify-between gap-2">
            <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
              {t("settings.transcribeFiles.queue.title")}
            </h2>
            <div className="flex items-center gap-2">
              {hasActive && (
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  onClick={() => void cancelAll()}
                >
                  {t("settings.transcribeFiles.queue.cancelAll")}
                </Button>
              )}
              {hasFinished && (
                <Button
                  type="button"
                  variant="secondary"
                  size="sm"
                  onClick={() => void clearFinished()}
                >
                  {t("settings.transcribeFiles.queue.clearFinished")}
                </Button>
              )}
            </div>
          </div>
          <div className="bg-background border border-mid-gray/20 rounded-lg overflow-visible">
            <div className="divide-y divide-mid-gray/20">
              {queue.map((job) => (
                <FileJobRow
                  key={job.job_id}
                  job={job}
                  noSpeech={noSpeech[job.job_id] === true}
                  onView={(viewedJob) =>
                    setViewed({
                      jobId: viewedJob.job_id,
                      fileName: viewedJob.file_name,
                    })
                  }
                />
              ))}
            </div>
          </div>
        </div>
      )}

      <TranscriptDialog
        source={viewed ? { kind: "job", job_id: viewed.jobId } : null}
        fileName={viewed?.fileName ?? ""}
        onClose={() => setViewed(null)}
      />
    </div>
  );
};
