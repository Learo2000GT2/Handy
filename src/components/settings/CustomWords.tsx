import React, { useRef, useState } from "react";
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

// Spellings keep their apostrophes ("Dunkin' Donuts", "nothin'"), which are
// part of the words. Quotes and angle brackets never are.
const normalizeSpelling = (spelling: string) =>
  spelling
    .replace(/[<>"\u201C\u201D\u00AB\u00BB]/g, "")
    .replace(/\s+/g, " ")
    .trim();

const WORD_CHAR = /[\p{Alphabetic}\p{N}]/u;
const EDGE_PUNCTUATION = /^[^\p{Alphabetic}\p{N}]+|[^\p{Alphabetic}\p{N}]+$/gu;

// Mirrors phrase_key in src-tauri/src/audio_toolkit/text.rs: case and
// punctuation inside or around words are ignored, punctuation between words
// is kept, and words stay separate ("Charge Bee" is not "ChargeBee").
const phraseKey = (text: string) =>
  text
    .split(/\s+/)
    .filter(Boolean)
    .map((word) => {
      const chars = Array.from(word);
      const first = chars.findIndex((c) => WORD_CHAR.test(c));
      if (first === -1) return word;
      let last = chars.length - 1;
      while (!WORD_CHAR.test(chars[last])) last -= 1;
      const core = chars
        .slice(first, last + 1)
        .filter((c) => WORD_CHAR.test(c))
        .join("")
        .toLowerCase();
      // Straight and curly apostrophes read the same.
      const fold = (edge: string[]) =>
        edge.join("").replace(/[\u2018\u2019]/g, "'");
      return fold(chars.slice(0, first)) + core + fold(chars.slice(last + 1));
    })
    .join(" ")
    .replace(EDGE_PUNCTUATION, "");

// Mirrors build_variant_lookup: a spelling must start and end on a word, or
// it can never match as typed ("& Co").
const isUsableSpelling = (spelling: string) => {
  const words = spelling.split(" ");
  return (
    phraseKey(spelling) !== "" &&
    WORD_CHAR.test(words[0]) &&
    WORD_CHAR.test(words[words.length - 1])
  );
};

// Own entries only: a word such as "constructor" must not read
// Object.prototype members.
const hasWord = (map: SpellingMap, word: string) =>
  Object.prototype.hasOwnProperty.call(map, word);

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
    const chipToggles = useRef(new Map<string, HTMLButtonElement>());
    const customWords = getSetting("custom_words") || [];
    const spellingMap: SpellingMap = getSetting("custom_word_variants") || {};
    const normalizedWord = normalizeCustomWord(newWord);
    const normalizedSpelling = normalizeSpelling(newSpelling);
    const isSaving =
      isUpdating("custom_words") || isUpdating("custom_word_variants");
    const activeWord =
      selectedWord && customWords.includes(selectedWord) ? selectedWord : null;

    const spellingsFor = (word: string) =>
      (hasWord(spellingMap, word) ? spellingMap[word] : undefined) ?? [];

    // The custom word whose spelling list already holds `text`, if any.
    const spellingOwner = (text: string) => {
      const key = phraseKey(text);
      return customWords.find((word) =>
        spellingsFor(word).some((spelling) => phraseKey(spelling) === key),
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
                    ref={(element) => {
                      if (element) chipToggles.current.set(word, element);
                      else chipToggles.current.delete(word);
                    }}
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
