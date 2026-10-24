import React from "react";
import { useTranslation } from "react-i18next";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { CircleCheck } from "lucide-react";
import { toast } from "sonner";
import type { FileJobSnapshot } from "@/bindings";
import { isolateLtr } from "@/lib/utils/transcript";

// How long the toast stays.
const READY_TOAST_MS = 15_000;

const BUTTON_CLASSES =
  "px-2 py-1 text-xs font-medium rounded-lg border bg-mid-gray/10 border-mid-gray/20 hover:bg-background-ui/30 hover:border-logo-primary cursor-pointer whitespace-nowrap";

interface TranscriptReadyToastProps {
  toastId: string | number;
  job: FileJobSnapshot;
  onOpen: () => void;
}

/** "Transcript ready: FILE", where it was saved, Open in Handy, Show File. */
const TranscriptReadyToast: React.FC<TranscriptReadyToastProps> = ({
  toastId,
  job,
  onOpen,
}) => {
  const { t } = useTranslation();
  const saved = job.saved_file;

  const handleShowFile = () => {
    toast.dismiss(toastId);
    if (!saved) return;
    revealItemInDir(saved.path).catch((error) => {
      console.error("Failed to reveal the transcript:", error);
      toast.error(t("settings.transcribeFiles.saved.notFound"));
    });
  };

  return (
    // The Toaster's toast style (App.tsx) draws the box around this.
    <div className="w-full flex items-start gap-3 text-sm text-text">
      <CircleCheck
        className="w-4 h-4 mt-0.5 shrink-0 text-green-500"
        aria-hidden="true"
      />
      <div className="min-w-0 flex-1 flex flex-col gap-2">
        <div className="flex flex-col gap-0.5">
          <p className="font-medium break-words">
            {t("settings.transcribeFiles.ready.title", {
              file: isolateLtr(job.file_name),
            })}
          </p>
          {saved ? (
            <p className="text-xs text-mid-gray break-all">
              {t("settings.transcribeFiles.saved.savedTo", {
                path: isolateLtr(saved.path),
              })}
            </p>
          ) : (
            job.save_error !== null && (
              <p className="text-xs text-yellow-500">
                {t("settings.transcribeFiles.saved.failed")}
              </p>
            )
          )}
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            className={BUTTON_CLASSES}
            onClick={() => {
              toast.dismiss(toastId);
              onOpen();
            }}
          >
            {t("settings.transcribeFiles.ready.open")}
          </button>
          {saved && (
            <button
              type="button"
              className={BUTTON_CLASSES}
              onClick={handleShowFile}
            >
              {t("settings.transcribeFiles.ready.showFile")}
            </button>
          )}
        </div>
      </div>
    </div>
  );
};

/**
 * Tells the user a job's transcript is ready, on whatever page they are:
 * with Open in Handy (`onOpen`) and, when it was saved as a file, Show File.
 */
export const showTranscriptReady = (
  job: FileJobSnapshot,
  onOpen: () => void,
): void => {
  toast.custom(
    (toastId) => (
      <TranscriptReadyToast toastId={toastId} job={job} onOpen={onOpen} />
    ),
    { duration: READY_TOAST_MS },
  );
};
