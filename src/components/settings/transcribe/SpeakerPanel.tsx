import React, { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ask } from "@tauri-apps/plugin-dialog";
import { Merge, Pencil, RotateCcw, Undo2 } from "lucide-react";
import { toast } from "sonner";
import {
  commands,
  type FileTranscript,
  type TranscriptSource,
  type TranscriptSpeakers,
} from "@/bindings";
import {
  runIfConfirmed,
  speakerName,
  speakerStyle,
} from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";
import { IconButton } from "../../ui/IconButton";
import { Input } from "../../ui/Input";

/** Longest speaker name the backend accepts. */
const MAX_NAME_LENGTH = 60;

/** Greys out a control that is aria-disabled (it keeps focus, unlike disabled). */
const ARIA_DISABLED_CLASSES =
  "aria-disabled:cursor-not-allowed aria-disabled:opacity-50";

/** A button on a speaker's chip. */
interface ChipButton {
  id: number;
  action: "rename" | "merge";
}

interface SpeakerPanelProps {
  source: TranscriptSource;
  speakers: TranscriptSpeakers;
  /** The transcript as stored after a rename or merge. */
  onChanged: (transcript: FileTranscript) => void;
  /** Speaker labels are being made for this transcript: no edits. */
  busy: boolean;
  /** An edit of this transcript (here or in its turns) is being saved. */
  saving: boolean;
  onSavingChange: (saving: boolean) => void;
  /** An edit made in this dialog can be undone. */
  canUndo: boolean;
  onUndo: () => Promise<void>;
  /** Labels the speakers again (after the user confirmed). */
  onRelabel: () => void;
  /** Opens the rename form of this speaker (a new one) when it changes. */
  autoRename: number | null;
}

/**
 * The speakers of a labelled transcript, each in its colour, with inline
 * renaming (an empty name goes back to "Speaker N") and merging into
 * another speaker; Undo for the last edit, and Label Again.
 */
export const SpeakerPanel: React.FC<SpeakerPanelProps> = ({
  source,
  speakers,
  onChanged,
  busy,
  saving,
  onSavingChange,
  canUndo,
  onUndo,
  onRelabel,
  autoRename,
}) => {
  const { t } = useTranslation();
  const titleId = useId();
  const [editingId, setEditing] = useState<number | null>(null);
  const [mergingId, setMerging] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  const formRef = useRef<HTMLFormElement>(null);
  const chipRefs = useRef(new Map<number, HTMLLIElement>());
  // The chip button that gets focus back when a form closes.
  const returnFocusRef = useRef<ChipButton | null>(null);

  const listed = (id: number | null) =>
    id !== null && speakers.speakers.some((speaker) => speaker.id === id);
  // A form closes when its speaker leaves the list (merged away, undone).
  const editing = listed(editingId) ? editingId : null;
  const merging = listed(mergingId) ? mergingId : null;
  const locked = busy || saving;
  const runningTitle = t("settings.transcribeFiles.speakers.state.running");

  const defaultName = (id: number) =>
    t("settings.transcribeFiles.speakers.defaultName", { number: id });
  const nameOf = (id: number) => speakerName(speakers, id, defaultName);

  useEffect(() => {
    if (editing !== null || merging !== null) return;
    const target = returnFocusRef.current;
    if (target === null) return;
    // The merge button is gone when one speaker is left.
    const chip = chipRefs.current.get(target.id);
    (
      chip?.querySelector<HTMLButtonElement>(
        `button[data-action="${target.action}"]`,
      ) ?? chip?.querySelector<HTMLButtonElement>("button")
    )?.focus();
    returnFocusRef.current = null;
  }, [editing, merging, speakers]);

  const startEditing = (id: number) => {
    const current = speakers.speakers.find((speaker) => speaker.id === id);
    setDraft(current?.name ?? "");
    setMerging(null);
    setEditing(id);
  };

  // A new speaker made from a passage is named at once.
  useEffect(() => {
    if (autoRename !== null && listed(autoRename)) startEditing(autoRename);
    // Only a new request opens the form; later list changes don't.
  }, [autoRename]);

  const stopEditing = () => {
    returnFocusRef.current =
      editing === null ? null : { id: editing, action: "rename" };
    setEditing(null);
  };

  /** Closes the merge form, focusing the merge button of speaker `focusOn`. */
  const stopMerging = (focusOn: number) => {
    returnFocusRef.current = { id: focusOn, action: "merge" };
    setMerging(null);
  };

  const save = async () => {
    if (editing === null || locked) return;
    const id = editing;
    const name = draft.trim();
    const current =
      speakers.speakers.find((speaker) => speaker.id === id)?.name?.trim() ??
      "";
    if (name === current) {
      stopEditing();
      return;
    }

    onSavingChange(true);
    let error: string | null = null;
    try {
      const result = await commands.renameTranscriptSpeaker(
        source,
        id,
        name === "" ? null : name,
      );
      if (result.status === "ok") {
        onChanged(result.data);
      } else {
        error = result.error;
      }
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught);
    } finally {
      onSavingChange(false);
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

  const merge = async (from: number, into: number) => {
    if (locked) return;
    const names = { from: nameOf(from), into: nameOf(into) };
    await runIfConfirmed(
      () =>
        ask(t("settings.transcribeFiles.speakers.mergeConfirm", names), {
          title: t("settings.transcribeFiles.speakers.mergeTitle"),
          kind: "warning",
        }),
      async () => {
        onSavingChange(true);
        let error: string | null = null;
        try {
          const result = await commands.mergeTranscriptSpeakers(
            source,
            from,
            into,
          );
          if (result.status === "ok") {
            onChanged(result.data);
            stopMerging(into);
          } else {
            error = result.error;
          }
        } catch (caught) {
          error = caught instanceof Error ? caught.message : String(caught);
        } finally {
          onSavingChange(false);
        }
        if (error !== null) {
          toast.error(
            t("settings.transcribeFiles.speakers.mergeError", { error }),
          );
        }
      },
    );
  };

  const undo = async () => {
    if (locked) return;
    onSavingChange(true);
    try {
      await onUndo();
    } finally {
      onSavingChange(false);
    }
  };

  const relabel = () => {
    if (locked) return;
    void runIfConfirmed(
      () =>
        ask(t("settings.transcribeFiles.speakers.relabelConfirm"), {
          title: t("settings.transcribeFiles.speakers.relabelTitle"),
          kind: "warning",
        }),
      async () => onRelabel(),
    );
  };

  /** Props of a control that is off while labelling or saving. */
  const lockable = (action: string) => ({
    "aria-disabled": locked,
    "aria-label": action,
    title: busy ? runningTitle : action,
  });

  // Cancels the open form only, not the dialog around it.
  const escapeCloses =
    (close: () => void) => (event: React.KeyboardEvent<HTMLElement>) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      if (!saving) close();
    };

  let chips: React.ReactNode;
  if (merging !== null) {
    const others = speakers.speakers.filter(
      (speaker) => speaker.id !== merging,
    );
    chips = (
      <div
        role="group"
        aria-label={t("settings.transcribeFiles.speakers.mergePrompt", {
          name: nameOf(merging),
        })}
        className="flex flex-wrap items-center gap-2"
        onKeyDown={escapeCloses(() => stopMerging(merging))}
      >
        <span className="text-sm text-text/70">
          {t("settings.transcribeFiles.speakers.mergePrompt", {
            name: nameOf(merging),
          })}
        </span>
        {others.map((speaker, index) => (
          <button
            key={speaker.id}
            type="button"
            autoFocus={index === 0}
            aria-disabled={locked}
            onClick={() => void merge(merging, speaker.id)}
            className={`flex items-center gap-1.5 rounded-full border border-mid-gray/20 bg-mid-gray/5 px-2.5 py-0.5 text-sm font-medium cursor-pointer hover:border-logo-primary focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary ${ARIA_DISABLED_CLASSES} ${speakerStyle(speaker.id)}`}
          >
            <span
              className="w-2 h-2 rounded-full bg-current shrink-0"
              aria-hidden="true"
            />
            <span dir="auto" className="break-all">
              {nameOf(speaker.id)}
            </span>
          </button>
        ))}
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-disabled={saving}
          onClick={() => {
            if (!saving) stopMerging(merging);
          }}
        >
          {t("settings.transcribeFiles.speakers.cancel")}
        </Button>
      </div>
    );
  } else {
    chips = (
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
                  onKeyDown={escapeCloses(stopEditing)}
                >
                  <Input
                    autoFocus
                    value={draft}
                    onChange={(event) => setDraft(event.target.value)}
                    readOnly={locked}
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
                    aria-disabled={locked}
                    title={busy ? runningTitle : undefined}
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
          const formOpen = editing !== null;
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
                data-action="rename"
                onClick={() => {
                  if (!locked) startEditing(speaker.id);
                }}
                disabled={formOpen}
                className={ARIA_DISABLED_CLASSES}
                {...lockable(
                  t("settings.transcribeFiles.speakers.rename", { name }),
                )}
              >
                <Pencil width={13} height={13} />
              </IconButton>
              {speakers.speakers.length >= 2 && (
                <IconButton
                  data-action="merge"
                  onClick={() => {
                    if (locked) return;
                    setEditing(null);
                    setMerging(speaker.id);
                  }}
                  disabled={formOpen}
                  className={ARIA_DISABLED_CLASSES}
                  {...lockable(
                    t("settings.transcribeFiles.speakers.mergeInto", { name }),
                  )}
                >
                  <Merge width={13} height={13} />
                </IconButton>
              )}
            </li>
          );
        })}
      </ul>
    );
  }

  return (
    <div role="group" aria-labelledby={titleId} className="space-y-1.5">
      <div className="flex items-center justify-between gap-2">
        <h3
          id={titleId}
          className="text-xs font-medium text-mid-gray uppercase tracking-wide"
        >
          {t("settings.transcribeFiles.speakers.title")}
        </h3>
        <div className="flex items-center gap-1">
          {canUndo && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className={`flex items-center gap-1 ${ARIA_DISABLED_CLASSES}`}
              {...lockable(t("settings.transcribeFiles.speakers.undo"))}
              onClick={() => void undo()}
            >
              <Undo2 className="w-3.5 h-3.5" aria-hidden="true" />
              <span>{t("settings.transcribeFiles.speakers.undo")}</span>
            </Button>
          )}
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className={`flex items-center gap-1 ${ARIA_DISABLED_CLASSES}`}
            {...lockable(t("settings.transcribeFiles.speakers.relabel"))}
            onClick={relabel}
          >
            <RotateCcw className="w-3.5 h-3.5" aria-hidden="true" />
            <span>{t("settings.transcribeFiles.speakers.relabel")}</span>
          </Button>
        </div>
      </div>
      {chips}
    </div>
  );
};
