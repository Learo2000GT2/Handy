import React, { useEffect, useRef } from "react";
import { Trans, useTranslation } from "react-i18next";
import { ask } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Download } from "lucide-react";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import {
  downloadPercent,
  refocusTarget,
  runIfConfirmed,
} from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { PROGRESS_CLASSES } from "./progress";

/** Model card of the speaker model, which shows its license. */
const SPEAKER_MODEL_PAGE_URL =
  "https://huggingface.co/handy-computer/diar_streaming_sortformer_4spk-v2.1-gguf";

/** Size of the speaker model until the backend has reported it. */
const SPEAKER_MODEL_SIZE_BYTES = 139_310_336;

const openModelPage = async () => {
  try {
    await openUrl(SPEAKER_MODEL_PAGE_URL);
  } catch (error) {
    console.error("Failed to open the speaker model page:", error);
  }
};

/**
 * "Label Speakers" for new file jobs, with the speaker model it needs: its
 * download (license shown first), progress, failure and removal.
 */
export const SpeakerLabelsToggle: React.FC = () => {
  const { t } = useTranslation();
  const labelSpeakers = useFileTranscriptionStore(
    (state) => state.labelSpeakers,
  );
  const setLabelSpeakers = useFileTranscriptionStore(
    (state) => state.setLabelSpeakers,
  );
  const model = useFileTranscriptionStore((state) => state.speakerModel);
  const download = useFileTranscriptionStore((state) => state.speakerDownload);
  const downloadError = useFileTranscriptionStore(
    (state) => state.speakerDownloadError,
  );
  const refreshSpeakerModel = useFileTranscriptionStore(
    (state) => state.refreshSpeakerModel,
  );
  const downloadSpeakerModel = useFileTranscriptionStore(
    (state) => state.downloadSpeakerModel,
  );
  const cancelSpeakerModelDownload = useFileTranscriptionStore(
    (state) => state.cancelSpeakerModelDownload,
  );
  const deleteSpeakerModel = useFileTranscriptionStore(
    (state) => state.deleteSpeakerModel,
  );

  // The file may have been removed by hand since the status was loaded.
  useEffect(() => {
    void refreshSpeakerModel();
  }, [refreshSpeakerModel]);

  const toggleRef = useRef<HTMLDivElement>(null);
  const rowRef = useRef<HTMLDivElement>(null);
  // Whether focus is in the model row. Its buttons are replaced as the model
  // state changes (Download, then Cancel, then Remove), and a removed button
  // takes focus with it, so focus then moves to the new row's button, or to
  // the toggle when the row is gone (Remove with Label Speakers off).
  const focusInRowRef = useRef(false);
  const rowKind =
    download !== null
      ? "downloading"
      : model?.downloaded
        ? "installed"
        : labelSpeakers || downloadError !== null
          ? "download"
          : "none";
  useEffect(() => {
    const active = document.activeElement;
    const target = refocusTarget(
      focusInRowRef.current,
      active === null || active === document.body,
      rowRef.current?.querySelector<HTMLElement>("button"),
      toggleRef.current?.querySelector<HTMLElement>("input"),
    );
    target?.focus();
    // Unmounting the row doesn't blur it, so it can't still hold focus.
    if (rowRef.current === null) focusInRowRef.current = false;
  }, [rowKind]);

  const size = Math.round(
    (model?.size_bytes || SPEAKER_MODEL_SIZE_BYTES) / 1_000_000,
  );

  // Asks first, like deleting a dictation model: it takes a new download to
  // label speakers again.
  const removeModel = () =>
    runIfConfirmed(
      () =>
        ask(t("settings.transcribeFiles.speakers.removeConfirm", { size }), {
          title: t("settings.transcribeFiles.speakers.remove"),
          kind: "warning",
        }),
      deleteSpeakerModel,
    );

  let modelRow: React.ReactNode = null;
  if (rowKind === "downloading" && download !== null) {
    const percent = downloadPercent(download.downloaded, download.total);
    const progressText = download.verifying
      ? t("settings.transcribeFiles.speakers.verifying")
      : t("settings.transcribeFiles.speakers.downloading", { percent });
    modelRow = (
      <div className="px-4 py-3 flex flex-col gap-2">
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
  } else if (rowKind === "installed") {
    modelRow = (
      <div className="px-4 py-2 flex items-center justify-between gap-3">
        <p className="text-xs text-text/60">
          {t("settings.transcribeFiles.speakers.installed", { size })}
        </p>
        <Button
          type="button"
          variant="danger-ghost"
          size="sm"
          onClick={() => void removeModel()}
        >
          {t("settings.transcribeFiles.speakers.remove")}
        </Button>
      </div>
    );
  } else if (rowKind === "download") {
    // The license is shown before anything is downloaded.
    modelRow = (
      <div className="px-4 py-3 flex flex-col gap-2">
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
  }

  return (
    <>
      <div ref={toggleRef}>
        <ToggleSwitch
          checked={labelSpeakers}
          onChange={setLabelSpeakers}
          label={t("settings.transcribeFiles.speakers.label")}
          description={t("settings.transcribeFiles.speakers.description", {
            size,
          })}
          descriptionMode="tooltip"
          grouped={true}
        />
      </div>
      {modelRow !== null && (
        <div
          ref={rowRef}
          onFocus={() => {
            focusInRowRef.current = true;
          }}
          onBlur={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget)) {
              focusInRowRef.current = false;
            }
          }}
        >
          {modelRow}
        </div>
      )}
    </>
  );
};
