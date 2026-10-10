import React from "react";
import { Trans, useTranslation } from "react-i18next";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Download } from "lucide-react";
import type { SpeakerModelStatus } from "@/bindings";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import { downloadPercent } from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { PROGRESS_CLASSES } from "./progress";

/** Model card of the speaker model, which shows its license. */
const SPEAKER_MODEL_PAGE_URL =
  "https://huggingface.co/handy-computer/diar_streaming_sortformer_4spk-v2.1-gguf";

/** Size of the speaker model until the backend has reported it. */
const SPEAKER_MODEL_SIZE_BYTES = 139_310_336;

/** The speaker model's download size in whole MB. */
export const speakerModelMegabytes = (
  model: SpeakerModelStatus | null,
): number =>
  Math.round((model?.size_bytes || SPEAKER_MODEL_SIZE_BYTES) / 1_000_000);

const openModelPage = async () => {
  try {
    await openUrl(SPEAKER_MODEL_PAGE_URL);
  } catch (error) {
    console.error("Failed to open the speaker model page:", error);
  }
};

interface SpeakerModelDownloadProps {
  /** Padding of the row; the settings list insets its rows. */
  className?: string;
}

/**
 * Download of the speaker model: the button with the license shown first,
 * any failure of the last download, and while it runs its progress with
 * Cancel Download. The parent decides when the model is needed.
 */
export const SpeakerModelDownload: React.FC<SpeakerModelDownloadProps> = ({
  className = "px-4 py-3",
}) => {
  const { t } = useTranslation();
  const model = useFileTranscriptionStore((state) => state.speakerModel);
  const download = useFileTranscriptionStore((state) => state.speakerDownload);
  const downloadError = useFileTranscriptionStore(
    (state) => state.speakerDownloadError,
  );
  const downloadSpeakerModel = useFileTranscriptionStore(
    (state) => state.downloadSpeakerModel,
  );
  const cancelSpeakerModelDownload = useFileTranscriptionStore(
    (state) => state.cancelSpeakerModelDownload,
  );
  const size = speakerModelMegabytes(model);

  if (download !== null) {
    const percent = downloadPercent(download.downloaded, download.total);
    const progressText = download.verifying
      ? t("settings.transcribeFiles.speakers.verifying")
      : t("settings.transcribeFiles.speakers.downloading", { percent });
    return (
      <div className={`${className} flex flex-col gap-2`}>
        <div className="flex items-center justify-between gap-3">
          <p className="text-xs text-text/70 tabular-nums" role="status">
            {progressText}
          </p>
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => void cancelSpeakerModelDownload()}
          >
            {t("settings.transcribeFiles.speakers.cancelDownload")}
          </Button>
        </div>
        <progress
          className={`${PROGRESS_CLASSES} ${download.verifying ? "animate-pulse" : ""}`}
          max={100}
          value={download.verifying ? 100 : percent}
          aria-label={t("settings.transcribeFiles.speakers.label")}
          aria-valuetext={progressText}
        />
      </div>
    );
  }

  // The license is shown before anything is downloaded.
  return (
    <div className={`${className} flex flex-col gap-2`}>
      {downloadError !== null && (
        <Alert variant="error">
          {t("settings.transcribeFiles.speakers.downloadFailed", {
            error: downloadError,
          })}
        </Alert>
      )}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <Button
          type="button"
          variant="primary-soft"
          size="sm"
          className="flex items-center gap-1.5"
          onClick={() => void downloadSpeakerModel()}
        >
          <Download className="w-3.5 h-3.5" aria-hidden="true" />
          <span>
            {t("settings.transcribeFiles.speakers.download", { size })}
          </span>
        </Button>
        <p className="text-xs text-text/60">
          <Trans
            i18nKey="settings.transcribeFiles.speakers.license"
            components={{
              page: (
                <button
                  type="button"
                  onClick={() => void openModelPage()}
                  className="underline cursor-pointer hover:text-logo-primary focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary rounded-sm"
                />
              ),
            }}
          />
        </p>
      </div>
    </div>
  );
};
