import React from "react";
import { useTranslation } from "react-i18next";
import { useMeetingStore } from "@/stores/meetingStore";
import { Button } from "../../ui/Button";
import { Dialog } from "../../ui/Dialog";

/** Shown when quitting while a meeting records. */
export const MeetingQuitDialog: React.FC = () => {
  const { t } = useTranslation();
  const quitRequested = useMeetingStore((state) => state.quitRequested);
  const recording = useMeetingStore((state) => state.status !== null);
  const dismissQuit = useMeetingStore((state) => state.dismissQuit);
  const stopAndQuit = useMeetingStore((state) => state.stopAndQuit);
  const stopping = useMeetingStore((state) => state.stopping);
  const prefix = "settings.meetings.quit";

  return (
    <Dialog
      open={quitRequested && recording}
      onOpenChange={(open) => {
        if (!open) dismissQuit();
      }}
      title={t(`${prefix}.title`)}
      closeLabel={t("common.close")}
      dismissible={!stopping}
      footer={
        <div className="flex justify-end gap-2">
          <Button
            variant="secondary"
            size="md"
            onClick={dismissQuit}
            disabled={stopping}
          >
            {t(`${prefix}.keepRecording`)}
          </Button>
          <Button
            variant="danger"
            size="md"
            onClick={() => void stopAndQuit()}
            disabled={stopping}
          >
            {stopping ? t(`${prefix}.saving`) : t(`${prefix}.stopAndQuit`)}
          </Button>
        </div>
      }
    >
      <p className="text-sm">{t(`${prefix}.body`)}</p>
    </Dialog>
  );
};
