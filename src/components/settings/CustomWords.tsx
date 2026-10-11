import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { Button } from "../ui/Button";
import { SettingContainer } from "../ui/SettingContainer";

interface CustomWordsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

type SpellingMap = Partial<{ [key in string]: string[] }>;

const MAX_ENTRY_LENGTH = 50;
// Mirrors MAX_VARIANT_WORDS in src-tauri/src/audio_toolkit/text.rs.
const MAX_SPELLING_WORDS = 6;

const normalizeCustomWord = (word: string) =>
  word
    .replace(/[<>"']/g, "")
    .replace(/\s+/g, " ")
    .trim();

// Mirrors build_match_key in src-tauri/src/audio_toolkit/text.rs: spellings
// are compared without case or punctuation.
const matchKey = (text: string) =>
  text.replace(/[^\p{L}\p{N}]/gu, "").toLowerCase();

const withoutWord = (map: SpellingMap, word: string): SpellingMap =>
  Object.fromEntries(Object.entries(map).filter(([key]) => key !== word));

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
    const [newWord, setNewWord] = useState("");
    const [selectedWord, setSelectedWord] = useState<string | null>(null);
    const [newSpelling, setNewSpelling] = useState("");
    const customWords = getSetting("custom_words") || [];
    const spellingMap: SpellingMap = getSetting("custom_word_variants") || {};
    const normalizedWord = normalizeCustomWord(newWord);
    const normalizedSpelling = normalizeCustomWord(newSpelling);
    const isSaving =
      isUpdating("custom_words") || isUpdating("custom_word_variants");
    const activeWord =
      selectedWord && customWords.includes(selectedWord) ? selectedWord : null;

    const spellingsFor = (word: string) => spellingMap[word] ?? [];

    // The custom word whose spelling list already holds `text`, if any.
    const spellingOwner = (text: string) => {
      const key = matchKey(text);
      return customWords.find((word) =>
        spellingsFor(word).some((spelling) => matchKey(spelling) === key),
      );
    };

    const handleAddWord = () => {
      if (normalizedWord && normalizedWord.length <= MAX_ENTRY_LENGTH) {
        if (customWords.includes(normalizedWord)) {
          toast.error(
            t("settings.advanced.customWords.duplicate", {
              word: normalizedWord,
            }),
          );
          return;
        }
        const owner = spellingOwner(normalizedWord);
        if (owner) {
          toast.error(
            t("settings.advanced.customWords.spellingDuplicate", {
              spelling: normalizedWord,
              word: owner,
            }),
          );
          return;
        }
        updateSetting("custom_words", [...customWords, normalizedWord]);
        setNewWord("");
      }
    };

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
      if (wordToRemove in spellingMap) {
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
      if (!activeWord) return;
      const spelling = normalizedSpelling;
      const key = matchKey(spelling);
      if (!key || spelling.length > MAX_ENTRY_LENGTH) return;

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
      if (customWords.some((word) => matchKey(word) === key)) {
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

    const handleWordKeyPress = (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleAddWord();
      }
    };

    const handleSpellingKeyPress = (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleAddSpelling();
      } else if (e.key === "Escape") {
        e.preventDefault();
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
          <div className="flex items-center gap-2">
            <Input
              type="text"
              className="max-w-40"
              value={newWord}
              onChange={(e) => setNewWord(e.target.value)}
              onKeyDown={handleWordKeyPress}
              placeholder={t("settings.advanced.customWords.placeholder")}
              variant="compact"
              disabled={isUpdating("custom_words")}
            />
            <Button
              onClick={handleAddWord}
              disabled={
                !normalizedWord ||
                normalizedWord.length > MAX_ENTRY_LENGTH ||
                isUpdating("custom_words")
              }
              variant="primary"
              size="md"
            >
              {t("settings.advanced.customWords.add")}
            </Button>
          </div>
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
                    onClick={() => handleToggleWord(word)}
                    disabled={isSaving}
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
                    className="ps-2 pe-1 py-1 rounded-s-lg cursor-pointer hover:bg-background-ui/30 focus:outline-none disabled:opacity-50 disabled:cursor-not-allowed"
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
                    className="ps-1 pe-2 py-1 self-stretch inline-flex items-center rounded-e-lg cursor-pointer hover:bg-background-ui/30 focus:outline-none disabled:opacity-50 disabled:cursor-not-allowed"
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
                disabled={isSaving}
                autoFocus
              />
              <Button
                onClick={handleAddSpelling}
                disabled={
                  !matchKey(normalizedSpelling) ||
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
