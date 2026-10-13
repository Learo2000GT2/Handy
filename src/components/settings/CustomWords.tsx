import React, { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import { SettingContainer } from "../ui/SettingContainer";
import { openAddWordDialog } from "./AddWordDialog";
import {
  MAX_ENTRY_LENGTH,
  MAX_SPELLING_WORDS,
  SpellingMap,
  hasWord,
  isUsableSpelling,
  normalizeSpelling,
  phraseKey,
  spellingOwner as findSpellingOwner,
  spellingsOf,
  withoutWord,
} from "@/lib/customWords";

interface CustomWordsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

const RemoveIcon: React.FC = () => (
  <svg
    className="w-3 h-3"
    fill="none"
    stroke="currentColor"
    viewBox="0 0 24 24"
  >
    <path
      strokeLinecap="round"
      strokeLinejoin="round"
      strokeWidth={2}
      d="M6 18L18 6M6 6l12 12"
    />
  </svg>
);

export const CustomWords: React.FC<CustomWordsProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();
    const [selectedWord, setSelectedWord] = useState<string | null>(null);
    const [newSpelling, setNewSpelling] = useState("");
    const chipToggles = useRef(new Map<string, HTMLButtonElement>());
    const customWords = getSetting("custom_words") || [];
    const spellingMap: SpellingMap = getSetting("custom_word_variants") || {};
    const normalizedSpelling = normalizeSpelling(newSpelling);
    const isSaving =
      isUpdating("custom_words") || isUpdating("custom_word_variants");
    const activeWord =
      selectedWord && customWords.includes(selectedWord) ? selectedWord : null;

    const spellingsFor = (word: string) => spellingsOf(spellingMap, word);
    const spellingOwner = (text: string) =>
      findSpellingOwner(customWords, spellingMap, text);

    const handleRemoveWord = async (wordToRemove: string) => {
      if (activeWord === wordToRemove) {
        setSelectedWord(null);
        setNewSpelling("");
      }
      await updateSetting(
        "custom_words",
        customWords.filter((word) => word !== wordToRemove),
      );
      // The backend retires the word's spellings with it; mirror that here.
      if (hasWord(spellingMap, wordToRemove)) {
        await updateSetting(
          "custom_word_variants",
          withoutWord(spellingMap, wordToRemove),
        );
      }
    };

    const handleToggleWord = (word: string) => {
      setSelectedWord(activeWord === word ? null : word);
      setNewSpelling("");
    };

    const handleAddSpelling = () => {
      // The input stays enabled while saving so it keeps focus between
      // entries; a second Enter during the save is ignored instead.
      if (!activeWord || isSaving) return;
      const spelling = normalizedSpelling;
      const key = phraseKey(spelling);
      if (!isUsableSpelling(spelling) || spelling.length > MAX_ENTRY_LENGTH) {
        return;
      }

      if (spelling.split(" ").length > MAX_SPELLING_WORDS) {
        toast.error(
          t("settings.advanced.customWords.spellingTooManyWords", {
            max: MAX_SPELLING_WORDS,
          }),
        );
        return;
      }
      // A spelling that reads as a custom word is never rewritten, so it
      // would have no effect.
      if (customWords.some((word) => phraseKey(word) === key)) {
        toast.error(
          t("settings.advanced.customWords.spellingIsCustomWord", {
            spelling,
          }),
        );
        return;
      }
      const owner = spellingOwner(spelling);
      if (owner) {
        toast.error(
          t("settings.advanced.customWords.spellingDuplicate", {
            spelling,
            word: owner,
          }),
        );
        return;
      }

      updateSetting("custom_word_variants", {
        ...spellingMap,
        [activeWord]: [...spellingsFor(activeWord), spelling],
      });
      setNewSpelling("");
    };

    const handleRemoveSpelling = (word: string, spellingToRemove: string) => {
      const remaining = spellingsFor(word).filter(
        (spelling) => spelling !== spellingToRemove,
      );
      updateSetting(
        "custom_word_variants",
        remaining.length > 0
          ? { ...spellingMap, [word]: remaining }
          : withoutWord(spellingMap, word),
      );
    };

    const handleSpellingKeyPress = (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleAddSpelling();
      } else if (e.key === "Escape" && activeWord) {
        e.preventDefault();
        // Hand focus back to the chip that opened the editor before the
        // editor (and the focused input) unmounts.
        chipToggles.current.get(activeWord)?.focus();
        setSelectedWord(null);
        setNewSpelling("");
      }
    };

    const activeSpellings = activeWord ? spellingsFor(activeWord) : [];

    return (
      <>
        <SettingContainer
          title={t("settings.advanced.customWords.title")}
          description={t("settings.advanced.customWords.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        >
          <Button
            onClick={() => openAddWordDialog()}
            disabled={isSaving}
            variant="primary"
            size="md"
          >
            {t("settings.advanced.customWords.addWordButton")}
          </Button>
        </SettingContainer>
        {customWords.length > 0 && (
          <div
            className={`px-4 p-2 ${grouped ? "" : "rounded-lg border border-mid-gray/20"} flex flex-wrap gap-1`}
          >
            {customWords.map((word) => {
              const spellings = spellingsFor(word);
              const isActive = activeWord === word;
              return (
                <span
                  key={word}
                  className={`inline-flex items-center rounded-lg border text-xs bg-mid-gray/10 transition-colors ${isActive ? "border-logo-primary" : "border-mid-gray/20 hover:border-logo-primary"}`}
                >
                  <button
                    type="button"
                    ref={(element) => {
                      if (element) chipToggles.current.set(word, element);
                      else chipToggles.current.delete(word);
                    }}
                    onClick={() => handleToggleWord(word)}
                    aria-expanded={isActive}
                    aria-label={t(
                      "settings.advanced.customWords.editSpellings",
                      {
                        word,
                      },
                    )}
                    title={t("settings.advanced.customWords.editSpellings", {
                      word,
                    })}
                    className="ps-2 pe-1 py-1 rounded-s-lg cursor-pointer hover:bg-background-ui/30 focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary"
                  >
                    <span>{word}</span>
                    {spellings.length > 0 && (
                      <span className="ms-1 ps-1 border-s border-mid-gray/30 text-mid-gray">
                        {spellings.join(", ")}
                      </span>
                    )}
                  </button>
                  <button
                    type="button"
                    onClick={() => handleRemoveWord(word)}
                    disabled={isSaving}
                    aria-label={t("settings.advanced.customWords.remove", {
                      word,
                    })}
                    className="ps-1 pe-2 py-1 self-stretch inline-flex items-center rounded-e-lg cursor-pointer hover:bg-background-ui/30 focus:outline-none focus-visible:ring-1 focus-visible:ring-logo-primary disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    <RemoveIcon />
                  </button>
                </span>
              );
            })}
          </div>
        )}
        {activeWord && (
          <div
            className={`px-4 p-2 ${grouped ? "" : "rounded-lg border border-mid-gray/20"} flex flex-col gap-2`}
          >
            <p className="text-xs text-mid-gray">
              {t("settings.advanced.customWords.spellingsHint", {
                word: activeWord,
              })}
            </p>
            <div className="flex items-center gap-2">
              <Input
                // A new input per word, so autoFocus also runs when the user
                // switches from one word's spellings to another's.
                key={activeWord}
                type="text"
                className="max-w-40"
                value={newSpelling}
                onChange={(e) => setNewSpelling(e.target.value)}
                onKeyDown={handleSpellingKeyPress}
                placeholder={t(
                  "settings.advanced.customWords.spellingPlaceholder",
                )}
                aria-label={t(
                  "settings.advanced.customWords.spellingPlaceholder",
                )}
                variant="compact"
                autoFocus
              />
              <Button
                onClick={handleAddSpelling}
                disabled={
                  !isUsableSpelling(normalizedSpelling) ||
                  normalizedSpelling.length > MAX_ENTRY_LENGTH ||
                  isSaving
                }
                variant="primary"
                size="md"
              >
                {t("settings.advanced.customWords.add")}
              </Button>
            </div>
            {activeSpellings.length > 0 && (
              <div className="flex flex-wrap gap-1">
                {activeSpellings.map((spelling) => (
                  <Button
                    key={spelling}
                    onClick={() => handleRemoveSpelling(activeWord, spelling)}
                    disabled={isSaving}
                    variant="secondary"
                    size="sm"
                    className="inline-flex items-center gap-1 cursor-pointer"
                    aria-label={t(
                      "settings.advanced.customWords.removeSpelling",
                      { spelling },
                    )}
                  >
                    <span>{spelling}</span>
                    <RemoveIcon />
                  </Button>
                ))}
              </div>
            )}
          </div>
        )}
      </>
    );
  },
);
