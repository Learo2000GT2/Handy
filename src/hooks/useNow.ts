import { useEffect, useState } from "react";

/** The time (Unix ms), updated every `intervalMs` while `active`. */
export const useNow = (active: boolean, intervalMs = 1000): number => {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(timer);
  }, [active, intervalMs]);
  return now;
};
