import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { Mic } from "lucide-react";
import type { MicCheckVerdict } from "@/bindings";
import { useMeetingStore } from "@/stores/meetingStore";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { MicrophoneSelector } from "../MicrophoneSelector";
import { SpeakerLabelsToggle } from "../transcribe/SpeakerLabelsToggle";
import { LevelMeter } from "./LevelMeter";

const CHECK_SECONDS = 10;

const verdictVariant = (verdict: MicCheckVerdict) =>
  verdict === "good"
    ? "success"
    : verdict === "no_signal"
      ? "error"
      : "warning";

/** New meeting: title, microphone and its check, options, consent, Start. */
export const MeetingSetup: React.FC = () => {
  const { t } = useTranslation();
  const [title, setTitle] = useState("");
  const [consent, setConsent] = useState(false);
  const checkRunning = useMeetingStore((state) => state.checkRunning);
  const checkLevel = useMeetingStore((state) => state.checkLevel);
  const checkResult = useMeetingStore((state) => state.checkResult);
  const checkError = useMeetingStore((state) => state.checkError);
  const runCheck = useMeetingStore((state) => state.runCheck);
  const cancelCheck = useMeetingStore((state) => state.cancelCheck);
  const start = useMeetingStore((state) => state.start);
  const starting = useMeetingStore((state) => state.starting);
  const liveTranscript = useMeetingStore((state) => state.liveTranscript);
  const setLiveTranscript = useMeetingStore((state) => state.setLiveTranscript);
  const labelSpeakers = useFileTranscriptionStore(
    (state) => state.labelSpeakers,
  );
  const speakerModel = useFileTranscriptionStore((state) => state.speakerModel);

  const secondsLeft = Math.max(
    0,
    CHECK_SECONDS - Math.floor((checkLevel?.elapsed_ms ?? 0) / 1000),
  );
  const prefix = "settings.meetings.setup";

  const handleStart = async () => {
    const started = await start({
      title,
      consent_confirmed: consent,
      label_speakers: labelSpeakers && speakerModel?.downloaded === true,
      live_transcript: liveTranscript,
    });
    if (started) {
      setTitle("");
      setConsent(false);
    }
  };

  return (
    <div className="space-y-4">
      <SettingsGroup title={t(`${prefix}.title`)}>
        <div className="px-4 py-3 flex flex-col gap-1">
          <label
            htmlFor="meeting-title"
            className="text-sm font-medium text-text"
          >
            {t(`${prefix}.meetingTitle`)}
          </label>
          <Input
            id="meeting-title"
            type="text"
            value={title}
            maxLength={60}
            onChange={(e) => setTitle(e.target.value)}
            placeholder={t(`${prefix}.meetingTitlePlaceholder`)}
            variant="compact"
          />
          <p className="text-xs text-mid-gray">
            {t(`${prefix}.meetingTitleHint`)}
          </p>
        </div>
        <MicrophoneSelector descriptionMode="tooltip" grouped={true} />
        <div className="px-4 py-3 flex flex-col gap-2">
          <div className="flex items-center justify-between gap-3">
            <div>
              <p className="text-sm font-medium text-text">
                {t(`${prefix}.check.title`)}
              </p>
              <p className="text-xs text-mid-gray">
                {t(`${prefix}.check.description`, { seconds: CHECK_SECONDS })}
              </p>
            </div>
            {checkRunning ? (
              <Button
                variant="secondary"
                size="md"
                onClick={() => void cancelCheck()}
              >
                {t(`${prefix}.check.stop`)}
              </Button>
            ) : (
              <Button
                variant="secondary"
                size="md"
                onClick={() => void runCheck()}
                className="inline-flex items-center gap-2 shrink-0"
              >
                <Mic className="w-4 h-4" aria-hidden="true" />
                {checkResult || checkError
                  ? t(`${prefix}.check.again`)
                  : t(`${prefix}.check.start`)}
              </Button>
            )}
          </div>
          {checkRunning && (
            <div
              className="flex flex-col gap-1"
              role="status"
              aria-live="polite"
            >
              <LevelMeter
                rmsDb={checkLevel?.rms_db ?? -100}
                peakDb={checkLevel?.peak_db ?? -100}
                label={t(`${prefix}.check.level`)}
              />
              <p className="text-xs text-mid-gray">
                {checkLevel === null
                  ? t(`${prefix}.check.opening`)
                  : secondsLeft > 0
                    ? t(`${prefix}.check.speak`, { seconds: secondsLeft })
                    : t(`${prefix}.check.checking`)}
              </p>
            </div>
          )}
          {!checkRunning && checkResult && (
            <Alert variant={verdictVariant(checkResult.stats.verdict)}>
              {t(`${prefix}.check.verdict.${checkResult.stats.verdict}`)}
              {checkResult.heard !== null && checkResult.heard !== "" && (
                <> {t(`${prefix}.check.heard`, { text: checkResult.heard })}</>
              )}
            </Alert>
          )}
          {!checkRunning && checkError && (
            <Alert variant="error">{checkError}</Alert>
          )}
        </div>
      </SettingsGroup>

      <SettingsGroup>
        <ToggleSwitch
          checked={liveTranscript}
          onChange={setLiveTranscript}
          label={t(`${prefix}.liveTranscript.label`)}
          description={t(`${prefix}.liveTranscript.description`)}
          descriptionMode="tooltip"
          grouped={true}
        />
        <SpeakerLabelsToggle />
      </SettingsGroup>

      <div className="rounded-lg border border-mid-gray/20 px-4 py-3 flex flex-col gap-3">
        <label className="flex items-start gap-2 text-sm cursor-pointer">
          <input
            type="checkbox"
            checked={consent}
            onChange={(e) => setConsent(e.target.checked)}
            className="mt-0.5 accent-logo-primary"
          />
          <span>
            {t(`${prefix}.consent.label`)}
            <span className="block text-xs text-mid-gray">
              {t(`${prefix}.consent.note`)}
            </span>
          </span>
        </label>
        <div className="flex items-center justify-between gap-3">
          <p className="text-xs text-mid-gray">{t(`${prefix}.startHint`)}</p>
          <Button
            variant="primary"
            size="lg"
            onClick={() => void handleStart()}
            disabled={!consent || starting || checkRunning}
            className="shrink-0"
          >
            {starting ? t(`${prefix}.starting`) : t(`${prefix}.start`)}
          </Button>
        </div>
      </div>
    </div>
  );
};
