import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { FileUp } from "lucide-react";
import { toast } from "sonner";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import {
  DISPLAY_FORMATS,
  isSupportedPath,
  pickerExtensions,
} from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";

interface FileDropZoneProps {
  /** No usable model: files can't be added. */
  disabled: boolean;
}

/**
 * Adds files by drag and drop or with the file picker. Drops are taken from
 * Tauri's native handler (HTML5 drop events carry no paths), which covers the
 * whole window while this page is shown.
 */
export const FileDropZone: React.FC<FileDropZoneProps> = ({ disabled }) => {
  const { t } = useTranslation();
  const formats = useFileTranscriptionStore((state) => state.formats);
  const enqueue = useFileTranscriptionStore((state) => state.enqueue);
  const [dragActive, setDragActive] = useState(false);
  const [picking, setPicking] = useState(false);

  // The drag-drop listener is registered once; it reads the latest values
  // from here.
  const latest = useRef({ disabled, formats, enqueue, t });
  latest.current = { disabled, formats, enqueue, t };

  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const payload = event.payload;
      const current = latest.current;

      if (payload.type === "enter") {
        setDragActive(!current.disabled);
      } else if (payload.type === "leave") {
        setDragActive(false);
      } else if (payload.type === "drop") {
        setDragActive(false);
        if (current.disabled) {
          toast.error(current.t("settings.transcribeFiles.noModel"));
          return;
        }
        // Until the format list has loaded, let the backend decide.
        const formats = current.formats;
        const accepted = formats
          ? payload.paths.filter((path) => isSupportedPath(path, formats))
          : payload.paths;
        const skipped = payload.paths.length - accepted.length;
        if (skipped > 0) {
          toast.info(
            current.t("settings.transcribeFiles.dropzone.skipped", {
              number: skipped,
            }),
          );
        }
        void current.enqueue(accepted);
      }
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const chooseFiles = async () => {
    const extensions = formats ? pickerExtensions(formats) : [];
    setPicking(true);
    try {
      const selected = await open({
        multiple: true,
        directory: false,
        title: t("settings.transcribeFiles.picker.title"),
        filters:
          extensions.length > 0
            ? [
                {
                  name: t("settings.transcribeFiles.picker.filterName"),
                  extensions,
                },
              ]
            : undefined,
      });
      if (selected !== null) {
        await enqueue(selected);
      }
    } catch (error) {
      console.error("Failed to choose files:", error);
      toast.error(
        t("settings.transcribeFiles.errors.enqueue", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    } finally {
      setPicking(false);
    }
  };

  const borderClasses = dragActive
    ? "border-logo-primary bg-logo-primary/10"
    : "border-mid-gray/40 bg-mid-gray/5";

  return (
    <div
      className={`flex flex-col items-center gap-3 rounded-lg border-2 border-dashed px-6 py-8 text-center transition-colors ${borderClasses} ${
        disabled ? "opacity-50" : ""
      }`}
    >
      <FileUp
        className={`w-10 h-10 ${dragActive ? "text-logo-primary" : "text-text/40"}`}
        aria-hidden="true"
      />
      <div className="space-y-1" aria-live="polite">
        <p className="text-sm font-medium">
          {dragActive
            ? t("settings.transcribeFiles.dropzone.active")
            : t("settings.transcribeFiles.dropzone.title")}
        </p>
        <p className="text-xs text-text/60">
          {t("settings.transcribeFiles.dropzone.hint")}
        </p>
      </div>
      <Button
        type="button"
        variant="primary-soft"
        onClick={chooseFiles}
        disabled={disabled || picking}
      >
        {t("settings.transcribeFiles.dropzone.choose")}
      </Button>
      <div className="space-y-0.5 text-xs text-text/50">
        <p>
          {t("settings.transcribeFiles.formats.native", {
            formats: DISPLAY_FORMATS.join(", "),
          })}
        </p>
        {formats && (
          <p>
            {formats.ffmpeg_available
              ? t("settings.transcribeFiles.formats.ffmpegAvailable")
              : t("settings.transcribeFiles.formats.ffmpegMissing")}
          </p>
        )}
      </div>
    </div>
  );
};
