import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { useSettings } from "../../hooks/useSettings";
import { Dialog } from "../ui/Dialog";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import {
  MAX_ENTRY_LENGTH,
  MAX_SPELLING_WORDS,
  SpellingMap,
  isUsableSpelling,
  normalizeCustomWord,
  normalizeSpelling,
  phraseKey,
  spellingOwner,
  spellingsOf,
} from "@/lib/customWords";

// Mirrors ADD_CUSTOM_WORD_EVENT in src-tauri/src/actions.rs.
const ADD_CUSTOM_WORD_EVENT = "add-custom-word-requested";

// A selection longer than one custom word, or spanning lines, was not meant
// as a word to add; open the dialog empty instead.
const asSelectedWord = (selection: string) => {
  const text = selection.trim();
  return text.length > 0 && text.length <= MAX_ENTRY_LENGTH && !/\n/.test(text)
    ? text
    : "";
};

/**
 * "Add word" dialog opened by the Add Custom Word shortcut, Handy's take on
 * Dragon's "correct that": the selected text arrives as what Handy wrote, the
 * user types what it should be, and the pair is saved as a custom word with
 * that spelling to replace. With "Should be" left empty, the selected text
 * itself is added as a custom word.
 */
export const AddWordDialog: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();
  const [open, setOpen] = useState(false);
  const [heard, setHeard] = useState("");
  const [written, setWritten] = useState("");
  const [saving, setSaving] = useState(false);
  const heardRef = useRef<HTMLInputElement>(null);
  const writtenRef = useRef<HTMLInputElement>(null);
  const [focusWritten, setFocusWritten] = useState(false);

  useEffect(() => {
    const unlisten = listen<string>(ADD_CUSTOM_WORD_EVENT, (event) => {
      const selected = asSelectedWord(event.payload ?? "");
      setHeard(selected);
      setWritten("");
      setFocusWritten(selected !== "");
      setOpen(true);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  const customWords = getSetting("custom_words") || [];
  const spellingMap: SpellingMap = getSetting("custom_word_variants") || {};

  const fail = (key: string, params: Record<string, unknown>) => {
    toast.error(t(`settings.advanced.customWords.${key}`, params));
  };

  // Validates and saves; returns whether the dialog can close.
  const save = async (): Promise<boolean> => {
    const word = normalizeCustomWord(written);
    const spelling = normalizeSpelling(heard);

    // Nothing typed under "Should be": add the selected text as a word.
    if (!word) {
      const newWord = normalizeCustomWord(heard);
      if (!newWord || newWord.length > MAX_ENTRY_LENGTH) return false;
      if (customWords.includes(newWord)) {
        fail("duplicate", { word: newWord });
        return false;
      }
      const owner = spellingOwner(customWords, spellingMap, newWord);
      if (owner) {
        fail("spellingDuplicate", { spelling: newWord, word: owner });
        return false;
      }
      await updateSetting("custom_words", [...customWords, newWord]);
      toast.success(
        t("settings.advanced.customWords.addWord.addedWord", { word: newWord }),
      );
      return true;
    }

    if (word.length > MAX_ENTRY_LENGTH) return false;
    const wordOwner = spellingOwner(customWords, spellingMap, word);
    if (wordOwner) {
      fail("spellingDuplicate", { spelling: word, word: wordOwner });
      return false;
    }

    const addSpelling =
      spelling !== "" && phraseKey(spelling) !== phraseKey(word);
    if (addSpelling) {
      if (!isUsableSpelling(spelling) || spelling.length > MAX_ENTRY_LENGTH) {
        return false;
      }
      if (spelling.split(" ").length > MAX_SPELLING_WORDS) {
        fail("spellingTooManyWords", { max: MAX_SPELLING_WORDS });
        return false;
      }
      if (
        customWords.some(
          (existing) => phraseKey(existing) === phraseKey(spelling),
        )
      ) {
        fail("spellingIsCustomWord", { spelling });
        return false;
      }
      const owner = spellingOwner(customWords, spellingMap, spelling);
      if (owner && owner !== word) {
        fail("spellingDuplicate", { spelling, word: owner });
        return false;
      }
    }

    // The word goes in first: the backend drops spellings whose word is
    // not a custom word.
    if (!customWords.includes(word)) {
      await updateSetting("custom_words", [...customWords, word]);
    }
    const existing = spellingsOf(spellingMap, word);
    const alreadyListed = existing.some(
      (listed) => phraseKey(listed) === phraseKey(spelling),
    );
    if (addSpelling && !alreadyListed) {
      await updateSetting("custom_word_variants", {
        ...spellingMap,
        [word]: [...existing, spelling],
      });
    }

    toast.success(
      addSpelling
        ? t("settings.advanced.customWords.addWord.addedSpelling", {
            spelling,
            word,
          })
        : t("settings.advanced.customWords.addWord.addedWord", { word }),
    );
    return true;
  };

  const handleSave = async () => {
    if (saving) return;
    setSaving(true);
    try {
      if (await save()) setOpen(false);
    } finally {
      setSaving(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void handleSave();
    }
  };

  const canSave =
    !saving &&
    (normalizeCustomWord(written) !== "" || normalizeCustomWord(heard) !== "");

  return (
    <Dialog
      open={open}
      onOpenChange={setOpen}
      title={t("settings.advanced.customWords.addWord.title")}
      description={t("settings.advanced.customWords.addWord.description")}
      closeLabel={t("common.close")}
      initialFocusRef={focusWritten ? writtenRef : heardRef}
      footer={
        <div className="flex justify-end gap-2">
          <Button variant="secondary" size="md" onClick={() => setOpen(false)}>
            {t("settings.advanced.customWords.addWord.cancel")}
          </Button>
          <Button
            variant="primary"
            size="md"
            onClick={() => void handleSave()}
            disabled={!canSave}
          >
            {t("settings.advanced.customWords.addWord.save")}
          </Button>
        </div>
      }
    >
      <div className="flex flex-col gap-3">
        <label className="flex flex-col gap-1 text-sm">
          <span className="font-medium">
            {t("settings.advanced.customWords.addWord.heardLabel")}
          </span>
          <Input
            ref={heardRef}
            type="text"
            value={heard}
            onChange={(e) => setHeard(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={t(
              "settings.advanced.customWords.addWord.heardPlaceholder",
            )}
            variant="compact"
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="font-medium">
            {t("settings.advanced.customWords.addWord.writtenLabel")}
          </span>
          <Input
            ref={writtenRef}
            type="text"
            value={written}
            onChange={(e) => setWritten(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={t(
              "settings.advanced.customWords.addWord.writtenPlaceholder",
            )}
            variant="compact"
          />
        </label>
        <p className="text-xs text-mid-gray">
          {t("settings.advanced.customWords.addWord.hint")}
        </p>
      </div>
    </Dialog>
  );
};
