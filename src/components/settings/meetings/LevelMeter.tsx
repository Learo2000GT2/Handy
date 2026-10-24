import React from "react";

/** Quietest level shown, dBFS. */
const FLOOR_DB = -60;

const fraction = (db: number) =>
  Math.min(1, Math.max(0, (db - FLOOR_DB) / -FLOOR_DB));

interface LevelMeterProps {
  rmsDb: number;
  peakDb: number;
  /** Greyed out (paused, or no microphone). */
  inactive?: boolean;
  label: string;
}

/** Horizontal sound level bar with a peak mark, from -60 to 0 dBFS. */
export const LevelMeter: React.FC<LevelMeterProps> = ({
  rmsDb,
  peakDb,
  inactive = false,
  label,
}) => {
  const level = fraction(rmsDb);
  const peak = fraction(peakDb);
  const color =
    peakDb >= -1
      ? "bg-red-500"
      : peakDb >= -6
        ? "bg-yellow-500"
        : "bg-green-500";
  return (
    <div
      role="meter"
      aria-label={label}
      aria-valuemin={FLOOR_DB}
      aria-valuemax={0}
      aria-valuenow={Math.round(Math.max(FLOOR_DB, rmsDb))}
      className={`relative h-2 w-full rounded-full bg-mid-gray/20 overflow-hidden ${inactive ? "opacity-40" : ""}`}
    >
      <div
        className={`absolute inset-y-0 start-0 rounded-full transition-[width] duration-150 ${color}`}
        style={{ width: `${level * 100}%` }}
      />
      <div
        className="absolute inset-y-0 w-0.5 bg-text/70"
        style={{ insetInlineStart: `calc(${peak * 100}% - 1px)` }}
      />
    </div>
  );
};
