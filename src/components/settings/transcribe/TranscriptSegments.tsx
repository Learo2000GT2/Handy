import React, { useLayoutEffect, useRef } from "react";
import type { TranscriptSegment } from "@/bindings";
import { formatClock } from "@/lib/utils/transcript";

// How close to the bottom (px) still counts as "following" a live transcript.
const FOLLOW_THRESHOLD_PX = 48;

interface TranscriptSegmentsProps {
  segments: TranscriptSegment[];
  /** Keep the newest segment in view while the user is at the bottom. */
  follow?: boolean;
}

export const TranscriptSegments: React.FC<TranscriptSegmentsProps> = ({
  segments,
  follow = false,
}) => {
  const listRef = useRef<HTMLOListElement>(null);
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

  return (
    <ol
      ref={listRef}
      onScroll={handleScroll}
      className="max-h-[60vh] overflow-y-auto select-text cursor-text space-y-2 pe-1"
    >
      {segments.map((segment, index) => (
        <li key={`${segment.start_ms}-${index}`} className="flex gap-3">
          <span
            dir="ltr"
            className={`font-mono text-xs tabular-nums text-text/50 shrink-0 pt-0.5 text-end ${timeWidth}`}
          >
            {formatClock(segment.start_ms)}
          </span>
          <p
            dir="auto"
            className="text-sm text-text/90 whitespace-pre-wrap break-words min-w-0"
          >
            {segment.text}
          </p>
        </li>
      ))}
    </ol>
  );
};
