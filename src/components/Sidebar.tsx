import React from "react";
import { useTranslation } from "react-i18next";
import {
  Cog,
  FileAudio,
  FlaskConical,
  History,
  Info,
  Loader2,
  Sparkles,
  Cpu,
  Users,
} from "lucide-react";
import HandyTextLogo from "./icons/HandyTextLogo";
import HandyHand from "./icons/HandyHand";
import { useSettings } from "../hooks/useSettings";
import { useFileTranscriptionStore } from "../stores/fileTranscriptionStore";
import {
  formatPercent,
  transcribeBadge,
  transcribeBadgeSpeakers,
} from "../lib/utils/transcript";
import {
  GeneralSettings,
  TranscribeFilesSettings,
  MeetingsSettings,
  AdvancedSettings,
  HistorySettings,
  DebugSettings,
  AboutSettings,
  PostProcessingSettings,
  ModelsSettings,
} from "./settings";

export type SidebarSection = keyof typeof SECTIONS_CONFIG;

interface IconProps {
  width?: number | string;
  height?: number | string;
  size?: number | string;
  className?: string;
  [key: string]: any;
}

interface SectionConfig {
  labelKey: string;
  icon: React.ComponentType<IconProps>;
  component: React.ComponentType;
  enabled: (settings: any) => boolean;
}

export const SECTIONS_CONFIG = {
  general: {
    labelKey: "sidebar.general",
    icon: HandyHand,
    component: GeneralSettings,
    enabled: () => true,
  },
  transcribe: {
    labelKey: "sidebar.transcribe",
    icon: FileAudio,
    component: TranscribeFilesSettings,
    enabled: () => true,
  },
  meetings: {
    labelKey: "sidebar.meetings",
    icon: Users,
    component: MeetingsSettings,
    enabled: () => true,
  },
  history: {
    labelKey: "sidebar.history",
    icon: History,
    component: HistorySettings,
    enabled: () => true,
  },
  models: {
    labelKey: "sidebar.models",
    icon: Cpu,
    component: ModelsSettings,
    enabled: () => true,
  },
  advanced: {
    labelKey: "sidebar.advanced",
    icon: Cog,
    component: AdvancedSettings,
    enabled: () => true,
  },
  postprocessing: {
    labelKey: "sidebar.postProcessing",
    icon: Sparkles,
    component: PostProcessingSettings,
    enabled: (settings) => settings?.post_process_enabled ?? false,
  },
  debug: {
    labelKey: "sidebar.debug",
    icon: FlaskConical,
    component: DebugSettings,
    enabled: (settings) => settings?.debug_mode ?? false,
  },
  about: {
    labelKey: "sidebar.about",
    icon: Info,
    component: AboutSettings,
    enabled: () => true,
  },
} as const satisfies Record<string, SectionConfig>;

interface SidebarProps {
  activeSection: SidebarSection;
  onSectionChange: (section: SidebarSection) => void;
}

/**
 * The running transcription's percent under "Transcribe", or a spinner
 * while it prepares, so its progress shows from every page. Its label says
 * when the percent is the speaker step's.
 */
const TranscribeBadge: React.FC = () => {
  const { t, i18n } = useTranslation();
  const badge = useFileTranscriptionStore((state) =>
    transcribeBadge(state.jobs),
  );
  const speakers = useFileTranscriptionStore((state) =>
    transcribeBadgeSpeakers(state.jobs),
  );
  if (badge === null) return null;
  const percent = badge === "busy" ? null : formatPercent(badge, i18n.language);
  let label: string;
  if (speakers) {
    label =
      percent === null
        ? t("sidebar.speakersRunning")
        : t("sidebar.speakersProgress", { percent });
  } else {
    label =
      percent === null
        ? t("sidebar.transcribeRunning")
        : t("sidebar.transcribeProgress", { percent });
  }
  return (
    <span
      className="self-start inline-flex items-center gap-1 rounded-full border border-mid-gray/30 bg-background px-1.5 py-0.5 text-[11px] font-semibold leading-none tabular-nums text-text"
      title={label}
      role="status"
      aria-label={label}
    >
      {percent === null ? (
        <Loader2 className="w-3 h-3 animate-spin" aria-hidden="true" />
      ) : (
        percent
      )}
    </span>
  );
};

export const Sidebar: React.FC<SidebarProps> = ({
  activeSection,
  onSectionChange,
}) => {
  const { t } = useTranslation();
  const { settings } = useSettings();

  const availableSections = Object.entries(SECTIONS_CONFIG)
    .filter(([_, config]) => config.enabled(settings))
    .map(([id, config]) => ({ id: id as SidebarSection, ...config }));

  return (
    <div className="flex flex-col w-40 h-full border-e border-mid-gray/20 items-center px-2">
      <HandyTextLogo width={120} className="m-4" />
      <div className="flex flex-col w-full items-center gap-1 pt-2 border-t border-mid-gray/20">
        {availableSections.map((section) => {
          const Icon = section.icon;
          const isActive = activeSection === section.id;

          return (
            <div
              key={section.id}
              className={`flex gap-2 items-center p-2 w-full rounded-lg cursor-pointer transition-colors ${
                isActive
                  ? "bg-logo-primary/80"
                  : "hover:bg-mid-gray/20 hover:opacity-100 opacity-85"
              }`}
              onClick={() => onSectionChange(section.id)}
            >
              <Icon width={24} height={24} className="shrink-0" />
              <div className="min-w-0 flex flex-col gap-0.5">
                <p
                  className="text-sm font-medium truncate"
                  title={t(section.labelKey)}
                >
                  {t(section.labelKey)}
                </p>
                {section.id === "transcribe" && <TranscribeBadge />}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
};
