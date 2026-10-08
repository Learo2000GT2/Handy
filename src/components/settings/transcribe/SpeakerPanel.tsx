import React, { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Pencil } from "lucide-react";
import { toast } from "sonner";
import {
  commands,
  type FileTranscript,
  type TranscriptSource,
  type TranscriptSpeakers,
} from "@/bindings";
import { speakerName, speakerStyle } from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";
import { IconButton } from "../../ui/IconButton";
import { Input } from "../../ui/Input";

/** Longest speaker name the backend accepts. */
const MAX_NAME_LENGTH = 60;

interface SpeakerPanelProps {
  source: TranscriptSource;
  speakers: TranscriptSpeakers;
  /** The transcript as stored after a rename. */
  onRenamed: (transcript: FileTranscript) => void;
}

/**
 * The speakers of a labelled transcript, each in its colour, with inline
 * renaming. An empty name goes back to "Speaker N".
 */
export const SpeakerPanel: React.FC<SpeakerPanelProps> = ({
  source,
  speakers,
  onRenamed,
}) => {
  const { t } = useTranslation();
  const titleId = useId();
  const [editing, setEditing] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  const formRef = useRef<HTMLFormElement>(null);
  const chipRefs = useRef(new Map<number, HTMLLIElement>());
  // The speaker whose rename button gets focus back when editing ends.
  const returnFocusRef = useRef<number | null>(null);

  const defaultName = (id: number) =>
    t("settings.transcribeFiles.speakers.defaultName", { number: id });
  const nameOf = (id: number) => speakerName(speakers, id, defaultName);

  useEffect(() => {
    if (editing !== null || returnFocusRef.current === null) return;
    chipRefs.current
      .get(returnFocusRef.current)
      ?.querySelector<HTMLButtonElement>("button")
      ?.focus();
    returnFocusRef.current = null;
  }, [editing]);

  const startEditing = (id: number) => {
    const current = speakers.speakers.find((speaker) => speaker.id === id);
    setDraft(current?.name ?? "");
    setEditing(id);
  };

  const stopEditing = () => {
    returnFocusRef.current = editing;
    setEditing(null);
  };

  const save = async () => {
    if (editing === null || saving) return;
    const id = editing;
    const name = draft.trim();
    const current =
      speakers.speakers.find((speaker) => speaker.id === id)?.name?.trim() ??
      "";
    if (name === current) {
      stopEditing();
      return;
    }

    setSaving(true);
    let error: string | null = null;
    try {
      const result = await commands.renameTranscriptSpeaker(
        source,
        id,
        name === "" ? null : name,
      );
      if (result.status === "ok") {
        onRenamed(result.data);
      } else {
        error = result.error;
      }
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught);
    } finally {
      setSaving(false);
    }

    if (error === null) {
      stopEditing();
    } else {
      toast.error(
        t("settings.transcribeFiles.speakers.renameError", { error }),
      );
      formRef.current?.querySelector("input")?.focus();
    }
  };

  return (
    <div role="group" aria-labelledby={titleId} className="space-y-1.5">
      <h3
        id={titleId}
        className="text-xs font-medium text-mid-gray uppercase tracking-wide"
      >
        {t("settings.transcribeFiles.speakers.title")}
      </h3>
      <ul className="flex flex-wrap items-center gap-2">
        {speakers.speakers.map((speaker) => {
          const name = nameOf(speaker.id);
          if (editing === speaker.id) {
            return (
              <li key={speaker.id}>
                <form
                  ref={formRef}
                  className="flex flex-wrap items-center gap-1.5"
                  onSubmit={(event) => {
                    event.preventDefault();
                    void save();
                  }}
                  onKeyDown={(event) => {
                    if (event.key !== "Escape") return;
                    // Cancel the rename only, not the dialog around it.
                    event.preventDefault();
                    event.stopPropagation();
                    if (!saving) stopEditing();
                  }}
                >
                  <Input
                    autoFocus
                    value={draft}
                    onChange={(event) => setDraft(event.target.value)}
                    readOnly={saving}
                    maxLength={MAX_NAME_LENGTH}
                    placeholder={defaultName(speaker.id)}
                    aria-label={t(
                      "settings.transcribeFiles.speakers.nameLabel",
                      {
                        name,
                      },
                    )}
                    spellCheck={false}
                    autoComplete="off"
                    variant="compact"
                    className="w-44"
                  />
                  {/* Never disabled while saving: a disabled button drops focus out of the dialog. */}
                  <Button
                    type="submit"
                    variant="primary-soft"
                    size="sm"
                    aria-disabled={saving}
                  >
                    {t("settings.transcribeFiles.speakers.save")}
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    aria-disabled={saving}
                    onClick={() => {
                      if (!saving) stopEditing();
                    }}
                  >
                    {t("settings.transcribeFiles.speakers.cancel")}
                  </Button>
                </form>
              </li>
            );
          }
          return (
            <li
              key={speaker.id}
              ref={(element) => {
                if (element) chipRefs.current.set(speaker.id, element);
                else chipRefs.current.delete(speaker.id);
              }}
              className="flex items-center gap-1 rounded-full border border-mid-gray/20 bg-mid-gray/5 ps-2.5 pe-0.5 py-0.5"
            >
              <span
                className={`flex items-center gap-1.5 text-sm font-medium ${speakerStyle(speaker.id)}`}
              >
                <span
                  className="w-2 h-2 rounded-full bg-current shrink-0"
                  aria-hidden="true"
                />
                <span dir="auto" className="break-all">
                  {name}
                </span>
              </span>
              <IconButton
                onClick={() => startEditing(speaker.id)}
                disabled={editing !== null}
                title={t("settings.transcribeFiles.speakers.rename", { name })}
              >
                <Pencil width={13} height={13} />
              </IconButton>
            </li>
          );
        })}
      </ul>
    </div>
  );
};
