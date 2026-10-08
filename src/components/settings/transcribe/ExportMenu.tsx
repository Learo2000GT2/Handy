import React, { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, Download } from "lucide-react";
import type { TranscriptSource } from "@/bindings";
import { Button } from "../../ui/Button";
import { IconButton } from "../../ui/IconButton";
import {
  EXPORT_FORMATS,
  exportTranscript,
  type ExportFormat,
} from "./exportTranscript";

interface ExportMenuProps {
  source: TranscriptSource;
  /** Original file name; the suggested export name is derived from it. */
  sourceName: string;
  disabled?: boolean;
  /** "icon" for compact rows, "button" for a labelled button. */
  variant?: "icon" | "button";
  /** Open the menu above the trigger (e.g. in a dialog footer). */
  placement?: "top" | "bottom";
}

/** Menu button offering TXT, Markdown, SRT and VTT exports. */
export const ExportMenu: React.FC<ExportMenuProps> = ({
  source,
  sourceName,
  disabled = false,
  variant = "button",
  placement = "bottom",
}) => {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [exporting, setExporting] = useState(false);
  const menuId = useId();
  const rootRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  useEffect(() => {
    if (!open) return;
    itemRefs.current[0]?.focus();

    const handleClickOutside = (event: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [open]);

  const close = () => {
    setOpen(false);
    rootRef.current
      ?.querySelector<HTMLButtonElement>("[aria-haspopup='menu']")
      ?.focus();
  };

  const handleSelect = async (format: ExportFormat) => {
    close();
    setExporting(true);
    try {
      await exportTranscript(source, format, sourceName);
    } finally {
      setExporting(false);
    }
  };

  const handleMenuKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const items = itemRefs.current.filter(
      (item): item is HTMLButtonElement => item !== null,
    );
    const current = items.indexOf(document.activeElement as HTMLButtonElement);

    switch (event.key) {
      case "Escape":
        // Close only the menu, not a dialog around it.
        event.preventDefault();
        event.stopPropagation();
        close();
        break;
      case "ArrowDown":
        event.preventDefault();
        items[(current + 1) % items.length]?.focus();
        break;
      case "ArrowUp":
        event.preventDefault();
        items[(current - 1 + items.length) % items.length]?.focus();
        break;
      case "Home":
        event.preventDefault();
        items[0]?.focus();
        break;
      case "End":
        event.preventDefault();
        items[items.length - 1]?.focus();
        break;
      case "Tab":
        setOpen(false);
        break;
    }
  };

  const label = t("settings.transcribeFiles.export.button");
  const triggerProps = {
    onClick: () => setOpen((value) => !value),
    disabled: disabled || exporting,
    "aria-haspopup": "menu" as const,
    "aria-expanded": open,
    "aria-controls": open ? menuId : undefined,
  };

  return (
    <div ref={rootRef} className="relative">
      {variant === "icon" ? (
        <IconButton title={label} {...triggerProps}>
          <Download width={16} height={16} />
        </IconButton>
      ) : (
        <Button
          type="button"
          variant="secondary"
          size="sm"
          className="flex items-center gap-1.5"
          {...triggerProps}
        >
          <Download className="w-3.5 h-3.5" aria-hidden="true" />
          <span>{label}</span>
          <ChevronDown className="w-3.5 h-3.5" aria-hidden="true" />
        </Button>
      )}
      {open && (
        <div
          id={menuId}
          role="menu"
          aria-label={label}
          onKeyDown={handleMenuKeyDown}
          className={`absolute end-0 z-50 min-w-48 py-1 bg-background border border-mid-gray/80 rounded-md shadow-lg ${
            placement === "top" ? "bottom-full mb-1" : "top-full mt-1"
          }`}
        >
          {EXPORT_FORMATS.map((format, index) => (
            <button
              key={format}
              ref={(element) => {
                itemRefs.current[index] = element;
              }}
              type="button"
              role="menuitem"
              tabIndex={-1}
              onClick={() => handleSelect(format)}
              className="w-full px-3 py-1.5 text-sm text-start whitespace-nowrap cursor-pointer transition-colors hover:bg-logo-primary/10 focus:bg-logo-primary/10 focus:outline-none"
            >
              {t(`settings.transcribeFiles.export.formats.${format}`)}
            </button>
          ))}
        </div>
      )}
    </div>
  );
};
