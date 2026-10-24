/** Classes for the thin `<progress>` bars on the Transcribe page. */
export const PROGRESS_CLASSES =
  "w-full h-1.5 [&::-webkit-progress-bar]:rounded-full [&::-webkit-progress-bar]:bg-mid-gray/20 [&::-webkit-progress-value]:rounded-full [&::-webkit-progress-value]:bg-logo-primary";

/** Classes for a running job's bar: thicker, so progress is easy to see. */
export const JOB_PROGRESS_CLASSES =
  "w-full h-3 [&::-webkit-progress-bar]:rounded-full [&::-webkit-progress-bar]:bg-mid-gray/20 [&::-webkit-progress-value]:rounded-full [&::-webkit-progress-value]:bg-logo-primary [&::-webkit-progress-value]:transition-all";
