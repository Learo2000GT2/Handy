import React, {
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, Download } from "lucide-react";
import type { TranscriptSource } from "@/bindings";
import { menuKeyAction, placeMenu } from "@/lib/utils/transcript";
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
  /** Side to open on when there is room (e.g. "top" in a dialog footer). */
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
  const menuRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const getTrigger = () =>
    rootRef.current?.querySelector<HTMLButtonElement>("[aria-haspopup='menu']");

  // The menu is fixed to the viewport so that a clipping container, such as
  // the dialog box, cannot cut it off. It is placed before paint, and before
  // the effect below focuses its first item.
  useLayoutEffect(() => {
    if (!open) return;

    const updatePosition = () => {
      const menu = menuRef.current;
      const trigger = getTrigger();
      if (!menu || !trigger) return;
      const position = placeMenu(
        trigger.getBoundingClientRect(),
        {
          width: menu.offsetWidth,
          // Full height, even while an earlier maxHeight caps it.
          height: menu.scrollHeight + menu.offsetHeight - menu.clientHeight,
        },
        { width: window.innerWidth, height: window.innerHeight },
        placement,
        window.getComputedStyle(menu).direction === "rtl",
      );
      menu.style.top = `${position.top}px`;
      menu.style.left = `${position.left}px`;
      menu.style.maxHeight = `${position.maxHeight}px`;
    };

    updatePosition();
    window.addEventListener("scroll", updatePosition, true);
    window.addEventListener("resize", updatePosition);
    return () => {
      window.removeEventListener("scroll", updatePosition, true);
      window.removeEventListener("resize", updatePosition);
    };
  }, [open, placement]);

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
    getTrigger()?.focus();
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
    const action = menuKeyAction(
      event.key,
      items.indexOf(document.activeElement as HTMLButtonElement),
      items.length,
    );
    if (!action) return;

    event.preventDefault();
    if (action.kind === "close") {
      // Keep Escape from closing, and Tab from moving focus out of, a dialog
      // around the menu.
      event.stopPropagation();
      close();
    } else {
      items[action.index]?.focus();
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
    <div ref={rootRef}>
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
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label={label}
          onKeyDown={handleMenuKeyDown}
          className="fixed z-50 min-w-48 py-1 overflow-y-auto bg-background border border-mid-gray/80 rounded-md shadow-lg"
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
              className="block w-full px-3 py-1.5 text-sm text-start whitespace-nowrap cursor-pointer transition-colors hover:bg-logo-primary/10 focus:bg-logo-primary/10 focus:outline-none"
            >
              {t(`settings.transcribeFiles.export.formats.${format}`)}
            </button>
          ))}
        </div>
      )}
    </div>
  );
};
