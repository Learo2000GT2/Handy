import React, { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { ask } from "@tauri-apps/plugin-dialog";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import { refocusTarget, runIfConfirmed } from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import {
  SpeakerModelDownload,
  speakerModelMegabytes,
} from "./SpeakerModelDownload";

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

  const size = speakerModelMegabytes(model);

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
  if (rowKind === "downloading" || rowKind === "download") {
    modelRow = <SpeakerModelDownload />;
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
