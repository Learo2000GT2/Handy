import React, { useLayoutEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import type { TranscriptSegment, TranscriptSpeakers } from "@/bindings";
import { formatClock, speakerName, speakerStyle } from "@/lib/utils/transcript";

// How close to the bottom (px) still counts as "following" a live transcript.
const FOLLOW_THRESHOLD_PX = 48;

interface TranscriptSegmentsProps {
  segments: TranscriptSegment[];
  /** Keep the newest segment in view while the user is at the bottom. */
  follow?: boolean;
  /**
   * Names for the segments' speakers. Given for speaker turns (labelled
   * paragraphs): each turn then starts with its speaker's name.
   */
  speakers?: TranscriptSpeakers;
}

export const TranscriptSegments: React.FC<TranscriptSegmentsProps> = ({
  segments,
  follow = false,
  speakers,
}) => {
  const { t } = useTranslation();
  const listRef = useRef<HTMLDivElement>(null);
  const atBottomRef = useRef(true);

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
            {speakers && segment.speaker != null ? (
              <div className="min-w-0">
                <p
                  dir="auto"
                  className={`text-sm font-medium ${speakerStyle(segment.speaker)}`}
                >
                  {speakerName(speakers, segment.speaker, (id) =>
                    t("settings.transcribeFiles.speakers.defaultName", {
                      number: id,
                    }),
                  )}
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
