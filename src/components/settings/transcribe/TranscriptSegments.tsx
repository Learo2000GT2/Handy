import React, { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Plus } from "lucide-react";
import type { TranscriptSegment, TranscriptSpeakers } from "@/bindings";
import { formatClock, speakerName, speakerStyle } from "@/lib/utils/transcript";
import { Button } from "../../ui/Button";

// How close to the bottom (px) still counts as "following" a live transcript.
const FOLLOW_THRESHOLD_PX = 48;

/** Greys out a control that is aria-disabled (it keeps focus, unlike disabled). */
const ARIA_DISABLED_CLASSES =
  "aria-disabled:cursor-not-allowed aria-disabled:opacity-50";

/** A speaker turn as the user saw it when changing its speaker. */
export interface TurnRef {
  index: number;
  startMs: number;
  fromSpeaker: number | null;
}

interface TranscriptSegmentsProps {
  segments: TranscriptSegment[];
  /** Keep the newest segment in view while the user is at the bottom. */
  follow?: boolean;
  /**
   * Names for the segments' speakers. Given for speaker turns (labelled
   * paragraphs): each turn then starts with its speaker's name.
   */
  speakers?: TranscriptSpeakers;
  /**
   * Gives a turn to speaker `to` (null: a new speaker). With `speakers`,
   * each turn's name becomes a button that opens a speaker picker. Resolves
   * whether it was saved.
   */
  onChangeSpeaker?: (turn: TurnRef, to: number | null) => Promise<boolean>;
  /** Speaker labels are being made: the name buttons do nothing. */
  disabled?: boolean;
  /** A speaker edit is being saved: the name buttons do nothing. */
  saving?: boolean;
}

export const TranscriptSegments: React.FC<TranscriptSegmentsProps> = ({
  segments,
  follow = false,
  speakers,
  onChangeSpeaker,
  disabled = false,
  saving = false,
}) => {
  const { t } = useTranslation();
  const listRef = useRef<HTMLDivElement>(null);
  const atBottomRef = useRef(true);
  const nameRefs = useRef(new Map<number, HTMLButtonElement>());
  // The one name button in the tab order (roving tabindex).
  const [activeIndex, setActiveIndex] = useState(0);
  const [picking, setPicking] = useState<TurnRef | null>(null);
  // Where the passage being moved started: its new turn's name gets focus.
  const movedFromRef = useRef<number | null>(null);
  const editable = speakers !== undefined && onChangeSpeaker !== undefined;
  const locked = disabled || saving;

  const handleScroll = () => {
    const list = listRef.current;
    if (!list) return;
    atBottomRef.current =
      list.scrollHeight - list.scrollTop - list.clientHeight <=
      FOLLOW_THRESHOLD_PX;
  };

  // Monospace width of the longest time ("59:59" or "1:00:00"), so the text
  // column lines up.
  const timeWidth = segments.some((segment) => segment.start_ms >= 3_600_000)
    ? "min-w-[7ch]"
    : "min-w-[5ch]";

  useLayoutEffect(() => {
    const list = listRef.current;
    if (follow && list && atBottomRef.current) {
      list.scrollTop = list.scrollHeight;
    }
  }, [follow, segments.length]);

  // Any change of the transcript closes the picker: the turn it was opened
  // for may be gone. After a change made here, focus goes to the name of the
  // turn that now holds the passage.
  useEffect(() => {
    setPicking(null);
    const movedFrom = movedFromRef.current;
    if (movedFrom === null) return;
    movedFromRef.current = null;
    let index = 0;
    segments.forEach((segment, i) => {
      if (segment.start_ms <= movedFrom) index = i;
    });
    setActiveIndex(index);
    nameRefs.current.get(index)?.focus();
  }, [segments]);

  const defaultName = (id: number) =>
    t("settings.transcribeFiles.speakers.defaultName", { number: id });
  const runningTitle = t("settings.transcribeFiles.speakers.state.running");
  const tabIndex = Math.min(activeIndex, segments.length - 1);

  const focusName = (index: number) => {
    if (index < 0 || index >= segments.length) return;
    setActiveIndex(index);
    nameRefs.current.get(index)?.focus();
  };

  const pick = async (turn: TurnRef, to: number | null) => {
    if (!onChangeSpeaker || locked) return;
    movedFromRef.current = turn.startMs;
    if (!(await onChangeSpeaker(turn, to))) movedFromRef.current = null;
  };

  const closePicker = (index: number) => {
    setPicking(null);
    focusName(index);
  };

  const nameButton = (segment: TranscriptSegment, index: number) => {
    const speaker = segment.speaker ?? null;
    const name =
      speaker === null
        ? t("settings.transcribeFiles.speakers.unknownSpeaker")
        : speakerName(speakers, speaker, defaultName);
    const action = t("settings.transcribeFiles.speakers.changeSpeaker", {
      name,
    });
    return (
      <button
        type="button"
        ref={(element) => {
          if (element) nameRefs.current.set(index, element);
          else nameRefs.current.delete(index);
        }}
        dir="auto"
        tabIndex={index === tabIndex ? 0 : -1}
        aria-disabled={locked}
        aria-label={action}
        aria-expanded={picking?.index === index}
        title={disabled ? runningTitle : action}
        onFocus={() => setActiveIndex(index)}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            focusName(index + (event.key === "ArrowDown" ? 1 : -1));
          }
        }}
        onClick={() => {
          if (locked) return;
          setPicking({
            index,
            startMs: segment.start_ms,
            fromSpeaker: speaker,
          });
        }}
        className={`text-sm font-medium text-start rounded-sm cursor-pointer hover:underline focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary ${ARIA_DISABLED_CLASSES} ${
          speaker === null ? "text-text/50 italic" : speakerStyle(speaker)
        }`}
      >
        {name}
      </button>
    );
  };

  const picker = (turn: TurnRef) => {
    const others = (speakers?.speakers ?? []).filter(
      (speaker) => speaker.id !== turn.fromSpeaker,
    );
    return (
      <div
        role="group"
        aria-label={t("settings.transcribeFiles.speakers.changeSpeakerPrompt")}
        className="flex flex-wrap items-center gap-1.5 py-1"
        onKeyDown={(event) => {
          if (event.key !== "Escape") return;
          // Cancels the picker only, not the dialog around it.
          event.preventDefault();
          event.stopPropagation();
          closePicker(turn.index);
        }}
      >
        <span className="text-xs text-text/70">
          {t("settings.transcribeFiles.speakers.changeSpeakerPrompt")}
        </span>
        {others.map((speaker, position) => (
          <button
            key={speaker.id}
            type="button"
            autoFocus={position === 0}
            aria-disabled={locked}
            onClick={() => void pick(turn, speaker.id)}
            className={`flex items-center gap-1.5 rounded-full border border-mid-gray/20 bg-mid-gray/5 px-2.5 py-0.5 text-sm font-medium cursor-pointer hover:border-logo-primary focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary ${ARIA_DISABLED_CLASSES} ${speakerStyle(speaker.id)}`}
          >
            <span
              className="w-2 h-2 rounded-full bg-current shrink-0"
              aria-hidden="true"
            />
            <span dir="auto" className="break-all">
              {speakerName(speakers, speaker.id, defaultName)}
            </span>
          </button>
        ))}
        <Button
          type="button"
          variant="ghost"
          size="sm"
          autoFocus={others.length === 0}
          aria-disabled={locked}
          className={`flex items-center gap-1 ${ARIA_DISABLED_CLASSES}`}
          onClick={() => void pick(turn, null)}
        >
          <Plus className="w-3.5 h-3.5" aria-hidden="true" />
          <span>{t("settings.transcribeFiles.speakers.newSpeaker")}</span>
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onClick={() => closePicker(turn.index)}
        >
          {t("settings.transcribeFiles.speakers.cancel")}
        </Button>
      </div>
    );
  };

  // Focusable so that arrow and page keys scroll it: WebKit (Linux and macOS)
  // scrolls only the focused element, and nothing inside can take focus.
  return (
    <div
      ref={listRef}
      onScroll={handleScroll}
      role="region"
      aria-label={t("settings.transcribeFiles.transcript.segmentsLabel")}
      tabIndex={0}
      className="max-h-[60vh] overflow-y-auto select-text cursor-text pe-1 rounded-sm focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary"
    >
      <ol className={speakers ? "space-y-3" : "space-y-2"}>
        {segments.map((segment, index) => (
          <li key={`${segment.start_ms}-${index}`} className="flex gap-3">
            <span
              dir="ltr"
              className={`font-mono text-xs tabular-nums text-text/50 shrink-0 pt-0.5 text-end ${timeWidth}`}
            >
              {formatClock(segment.start_ms)}
            </span>
            {editable ? (
              <div className="min-w-0">
                {nameButton(segment, index)}
                {picking?.index === index && picker(picking)}
                <p
                  dir="auto"
                  className="text-sm text-text/90 whitespace-pre-wrap break-words"
                >
                  {segment.text}
                </p>
              </div>
            ) : speakers && segment.speaker != null ? (
              <div className="min-w-0">
                <p
                  dir="auto"
                  className={`text-sm font-medium ${speakerStyle(segment.speaker)}`}
                >
                  {speakerName(speakers, segment.speaker, defaultName)}
                </p>
                <p
                  dir="auto"
                  className="text-sm text-text/90 whitespace-pre-wrap break-words"
                >
                  {segment.text}
                </p>
              </div>
            ) : (
              <p
                dir="auto"
                className="text-sm text-text/90 whitespace-pre-wrap break-words min-w-0"
              >
                {segment.text}
              </p>
            )}
          </li>
        ))}
      </ol>
    </div>
  );
};
