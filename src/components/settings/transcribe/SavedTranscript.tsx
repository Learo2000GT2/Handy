import React from "react";
import { useTranslation } from "react-i18next";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { FileCheck } from "lucide-react";
import { toast } from "sonner";
import { commands, type FileJobSnapshot } from "@/bindings";
import { isolateLtr } from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";

interface SavedTranscriptProps {
  job: Pick<FileJobSnapshot, "job_id" | "saved_file" | "save_error">;
  /** Says that changes made since (speaker names) are not in the file. */
  showExportHint?: boolean;
}

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

/**
 * Where a job's transcript was saved automatically, with Open and Show in
 * Folder; or why it couldn't be saved. Nothing when it wasn't to be saved.
 */
export const SavedTranscript: React.FC<SavedTranscriptProps> = ({
  job,
  showExportHint = false,
}) => {
  const { t } = useTranslation();
  const saved = job.saved_file;
  if (!saved) {
    if (job.save_error === null) return null;
    return (
      <p className="text-xs text-yellow-500" title={job.save_error}>
        {t("settings.transcribeFiles.saved.failed")}
      </p>
    );
  }

  const handleOpen = async () => {
    let error: string | null = null;
    try {
      const result = await commands.openSavedTranscript(job.job_id);
      if (result.status === "error") error = result.error;
    } catch (caught) {
      error = errorText(caught);
    }
    if (error !== null) {
      toast.error(t("settings.transcribeFiles.saved.openError", { error }));
    }
  };

  const handleShow = async () => {
    try {
      await revealItemInDir(saved.path);
    } catch (error) {
      console.error("Failed to reveal the saved transcript:", error);
      toast.error(t("settings.transcribeFiles.saved.notFound"));
    }
  };

  return (
    <div className="flex items-start gap-2 text-xs">
      <FileCheck
        className="w-3.5 h-3.5 mt-px shrink-0 text-green-500"
        aria-hidden="true"
      />
      <div className="min-w-0 flex-1 flex flex-col gap-1">
        <p className="break-all text-text/70 select-text cursor-text">
          {t("settings.transcribeFiles.saved.savedTo", {
            path: isolateLtr(saved.path),
          })}
        </p>
        <div className="flex flex-wrap items-center gap-1.5">
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => void handleOpen()}
          >
            {t("settings.transcribeFiles.saved.open")}
          </Button>
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => void handleShow()}
          >
            {t("settings.transcribeFiles.saved.showInFolder")}
          </Button>
        </div>
        {saved.unwritable_folder !== null && (
          <p className="text-yellow-500">
            {t("settings.transcribeFiles.saved.fallback", {
              folder: isolateLtr(saved.unwritable_folder),
            })}
          </p>
        )}
        {showExportHint && (
          <p className="text-text/50">
            {t("settings.transcribeFiles.saved.exportAgain")}
          </p>
        )}
      </div>
    </div>
  );
};
