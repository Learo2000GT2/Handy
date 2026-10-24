import React from "react";
import { useTranslation } from "react-i18next";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { useSettings } from "../../../hooks/useSettings";
import { useOsType } from "../../../hooks/useOsType";
import { formatKeyCombination } from "../../../lib/utils/keyboard";
import { isolateLtr } from "@/lib/utils/transcript";

/**
 * Explains when post-processing runs (with the post-processing shortcut, as
 * it is bound now) and how to set it up, with the option to clean up
 * dictations made with the normal shortcut too.
 */
export const HowItWorks: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const osType = useOsType();
  const bindings = getSetting("bindings") ?? {};
  const everyDictation = getSetting("post_process_every_dictation") ?? false;

  const shortcutText = (id: string): string => {
    const binding = bindings[id]?.current_binding ?? "";
    return binding
      ? isolateLtr(formatKeyCombination(binding, osType))
      : t("settings.postProcessing.howItWorks.notSet");
  };
  const shortcuts = {
    shortcut: shortcutText("transcribe_with_post_process"),
    normal: shortcutText("transcribe"),
  };

  return (
    <SettingsGroup title={t("settings.postProcessing.howItWorks.title")}>
      <div className="px-4 py-3 space-y-2 text-sm">
        <p>{t("settings.postProcessing.howItWorks.intro")}</p>
        <p className="font-medium">
          {everyDictation
            ? t("settings.postProcessing.howItWorks.shortcutEvery", shortcuts)
            : t("settings.postProcessing.howItWorks.shortcut", shortcuts)}
        </p>
        <ol className="list-decimal ps-5 space-y-1 text-mid-gray">
          <li>{t("settings.postProcessing.howItWorks.stepProvider")}</li>
          <li>{t("settings.postProcessing.howItWorks.stepModel")}</li>
          <li>{t("settings.postProcessing.howItWorks.stepPrompt")}</li>
          <li>{t("settings.postProcessing.howItWorks.stepTest")}</li>
        </ol>
        <p className="text-mid-gray">
          {t("settings.postProcessing.howItWorks.fallback")}
        </p>
      </div>
      <ToggleSwitch
        checked={everyDictation}
        onChange={(enabled) =>
          void updateSetting("post_process_every_dictation", enabled)
        }
        isUpdating={isUpdating("post_process_every_dictation")}
        label={t("settings.postProcessing.howItWorks.everyDictation.label")}
        description={t(
          "settings.postProcessing.howItWorks.everyDictation.description",
        )}
        descriptionMode="inline"
        grouped={true}
      />
    </SettingsGroup>
  );
};
