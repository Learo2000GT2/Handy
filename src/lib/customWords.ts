// Rules shared by the Custom Words panel and the "Add word" dialog, so both
// accept exactly what the backend matcher in
// src-tauri/src/audio_toolkit/text.rs can use.

export type SpellingMap = Partial<{ [key in string]: string[] }>;

export const MAX_ENTRY_LENGTH = 50;
// Mirrors MAX_VARIANT_WORDS in src-tauri/src/audio_toolkit/text.rs.
export const MAX_SPELLING_WORDS = 6;

export const normalizeCustomWord = (word: string) =>
  word
    .replace(/[<>"']/g, "")
    .replace(/\s+/g, " ")
    .trim();

// Spellings keep their apostrophes ("Dunkin' Donuts", "nothin'"), which are
// part of the words. Quotes and angle brackets never are.
export const normalizeSpelling = (spelling: string) =>
  spelling
    .replace(/[<>"\u201C\u201D\u00AB\u00BB]/g, "")
    .replace(/\s+/g, " ")
    .trim();

const WORD_CHAR = /[\p{Alphabetic}\p{N}]/u;
const EDGE_PUNCTUATION = /^[^\p{Alphabetic}\p{N}]+|[^\p{Alphabetic}\p{N}]+$/gu;

// Mirrors phrase_key in src-tauri/src/audio_toolkit/text.rs: case and
// punctuation inside or around words are ignored, punctuation between words
// is kept, and words stay separate ("Charge Bee" is not "ChargeBee").
export const phraseKey = (text: string) =>
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
export const isUsableSpelling = (spelling: string) => {
  const words = spelling.split(" ");
  return (
    phraseKey(spelling) !== "" &&
    WORD_CHAR.test(words[0]) &&
    WORD_CHAR.test(words[words.length - 1])
  );
};

// Own entries only: a word such as "constructor" must not read
// Object.prototype members.
export const hasWord = (map: SpellingMap, word: string) =>
  Object.prototype.hasOwnProperty.call(map, word);

export const withoutWord = (map: SpellingMap, word: string): SpellingMap =>
  Object.fromEntries(Object.entries(map).filter(([key]) => key !== word));

/** The spellings listed under `word`. */
export const spellingsOf = (map: SpellingMap, word: string): string[] =>
  (hasWord(map, word) ? map[word] : undefined) ?? [];

/** The custom word whose spelling list already holds `text`, if any. */
export const spellingOwner = (
  customWords: string[],
  map: SpellingMap,
  text: string,
): string | undefined => {
  const key = phraseKey(text);
  return customWords.find((word) =>
    spellingsOf(map, word).some((spelling) => phraseKey(spelling) === key),
  );
};
