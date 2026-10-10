import { save } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { toast } from "sonner";
import i18n from "@/i18n";
import {
  commands,
  type TranscriptFormat,
  type TranscriptSource,
} from "@/bindings";
import { defaultExportName } from "@/lib/utils/transcript";

/** Formats offered in the UI. JSON export is CLI-only. */
export type ExportFormat = Exclude<TranscriptFormat, "json">;

export const EXPORT_FORMATS: readonly ExportFormat[] = [
  "docx",
  "txt",
  "md",
  "srt",
  "vtt",
];

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

/**
 * Asks where to save, then has the backend render and write the file. Rust
 * owns the formats, so exports match the CLI byte for byte.
 */
export const exportTranscript = async (
  source: TranscriptSource,
  format: ExportFormat,
  sourceName: string,
): Promise<void> => {
  const t = i18n.t.bind(i18n);
  try {
    const path = await save({
      title: t("settings.transcribeFiles.export.dialogTitle"),
      defaultPath: defaultExportName(sourceName, format),
      filters: [
        {
          name: t(`settings.transcribeFiles.export.formats.${format}`),
          extensions: [format],
        },
      ],
    });
    if (path === null) return;

    const result = await commands.exportFileTranscript(source, format, path);
    if (result.status === "error") {
      toast.error(
        t("settings.transcribeFiles.export.error", { error: result.error }),
      );
      return;
    }

    const writtenPath = result.data;
    toast.success(t("settings.transcribeFiles.export.success"), {
      action: {
        label: t("settings.transcribeFiles.export.showInFolder"),
        onClick: () => {
          revealItemInDir(writtenPath).catch((error) => {
            console.error("Failed to reveal exported transcript:", error);
          });
        },
      },
    });
  } catch (error) {
    toast.error(
      t("settings.transcribeFiles.export.error", { error: errorText(error) }),
    );
  }
};
