use natural::phonetics::soundex;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use strsim::levenshtein;

/// Alternate spellings a model may write for a custom word, keyed by the
/// custom word they should be rewritten to (`"Stacey"` -> `["Stacy", "Stacie"]`).
///
/// This is the written-form / spoken-form split of dictation tools such as
/// Dragon: the key is what the user wants on screen, the values are what the
/// recognizer tends to produce. A spelling may span words (`"new in"` for
/// `"Nguyen"`).
pub type CustomWordVariants = HashMap<String, Vec<String>>;

/// Longest alternate spelling, in whitespace-separated words, that
/// [`apply_custom_word_variants`] will match.
const MAX_VARIANT_WORDS: usize = 6;

/// Builds an n-gram string by cleaning and concatenating words
///
/// Strips punctuation from each word, lowercases, and joins without spaces.
/// This allows matching "Charge B" against "ChargeBee".
fn build_ngram(words: &[&str]) -> String {
    words
        .iter()
        .map(|w| build_match_key(w))
        .collect::<Vec<_>>()
        .concat()
}

fn build_match_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

struct CustomWordMatchKey {
    word_index: usize,
    key: String,
}

fn build_custom_word_match_keys(word: &str, word_index: usize) -> Vec<CustomWordMatchKey> {
    let primary_key = build_match_key(word);
    let mut keys = Vec::with_capacity(2);

    // The fallback matcher is intentionally limited to ASCII terms. Its
    // whitespace tokenization and Soundex scoring are not suitable for CJK
    // scripts. Unicode custom words remain available to models that accept
    // them as native decode prompts; they are simply skipped by this fallback.
    if is_supported_fuzzy_key(&primary_key) {
        keys.push(CustomWordMatchKey {
            word_index,
            key: primary_key.clone(),
        });
    }

    if word.contains('&') {
        let expanded_key = build_match_key(&word.replace('&', " and "));
        if is_supported_fuzzy_key(&expanded_key) && expanded_key != primary_key {
            keys.push(CustomWordMatchKey {
                word_index,
                key: expanded_key,
            });
        }
    }

    keys
}

fn is_supported_fuzzy_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric())
}

fn supports_soundex(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic())
}

/// Finds the best matching custom word for a candidate string
///
/// Uses Levenshtein distance and Soundex phonetic matching to find
/// the best match above the given threshold.
///
/// # Arguments
/// * `candidate` - The cleaned/lowercased candidate string to match
/// * `custom_words` - Original custom words (for returning the replacement)
/// * `custom_word_match_keys` - Normalized custom-word keys for comparison
/// * `threshold` - Maximum similarity score to accept
///
/// # Returns
/// The best matching custom word and its score, if any match was found
fn find_best_match<'a>(
    candidate: &str,
    custom_words: &'a [String],
    custom_word_match_keys: &[CustomWordMatchKey],
    threshold: f64,
) -> Option<(&'a String, f64)> {
    if !is_supported_fuzzy_key(candidate) || candidate.chars().count() > 50 {
        return None;
    }

    let mut best_match: Option<&String> = None;
    let mut best_score = f64::MAX;

    for custom_word_key in custom_word_match_keys {
        // Skip if lengths are too different (optimization + prevents over-matching)
        // Use percentage-based check: max 25% length difference (prevents n-grams from
        // matching significantly shorter custom words, e.g., "openaigpt" vs "openai")
        let candidate_len = candidate.chars().count();
        let custom_word_len = custom_word_key.key.chars().count();
        let len_diff = candidate_len.abs_diff(custom_word_len) as f64;
        let max_len = candidate_len.max(custom_word_len) as f64;
        let max_allowed_diff = (max_len * 0.25).max(2.0); // At least 2 chars difference allowed
        if len_diff > max_allowed_diff {
            continue;
        }

        // Calculate Levenshtein distance (normalized by length)
        let levenshtein_dist = levenshtein(candidate, &custom_word_key.key);
        let levenshtein_score = if max_len > 0.0 {
            levenshtein_dist as f64 / max_len
        } else {
            1.0
        };

        // Soundex is an English/ASCII phonetic algorithm. Numeric terms can
        // still use edit distance, but must not receive a phonetic boost.
        let phonetic_match = supports_soundex(candidate)
            && supports_soundex(&custom_word_key.key)
            && soundex(candidate, &custom_word_key.key);

        // Combine scores: favor phonetic matches, but also consider string similarity
        let combined_score = if phonetic_match {
            levenshtein_score * 0.3 // Give significant boost to phonetic matches
        } else {
            levenshtein_score
        };

        // Accept if the score is good enough (configurable threshold)
        if combined_score < threshold && combined_score < best_score {
            best_match = Some(&custom_words[custom_word_key.word_index]);
            best_score = combined_score;
        }
    }

    best_match.map(|m| (m, best_score))
}

/// Applies custom word corrections to transcribed text using fuzzy matching
///
/// This function corrects words in the input text by finding the best matches
/// from a list of custom words using a combination of:
/// - Levenshtein distance for string similarity
/// - Soundex phonetic matching for pronunciation similarity
/// - N-gram matching for multi-word speech artifacts (e.g., "Charge B" -> "ChargeBee")
///
/// # Arguments
/// * `text` - The input text to correct
/// * `custom_words` - List of custom words to match against
/// * `threshold` - Maximum similarity score to accept (0.0 = exact match, 1.0 = any match)
///
/// # Returns
/// The corrected text with custom words applied
pub fn apply_custom_words(text: &str, custom_words: &[String], threshold: f64) -> String {
    if custom_words.is_empty() {
        return text.to_string();
    }

    // Pre-compute normalized comparison keys to avoid repeated allocations.
    let custom_word_match_keys: Vec<CustomWordMatchKey> = custom_words
        .iter()
        .enumerate()
        .flat_map(|(index, word)| build_custom_word_match_keys(word, index))
        .collect();

    let words: Vec<&str> = text.split_whitespace().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let mut best_match: Option<(usize, &String, f64)> = None;

        // Consider n-grams up to three words and choose the closest match. A
        // longest-first match can consume a following ordinary word when both
        // candidates happen to share a Soundex code (for example,
        // "Charge B, che" matching "ChargeBee").
        for n in (1..=3).rev() {
            if i + n > words.len() {
                continue;
            }

            let ngram_words = &words[i..i + n];
            // Do not consume across a punctuation boundary. In
            // "Charge B, che", the comma closes the candidate at "B,".
            if ngram_words[..n.saturating_sub(1)]
                .iter()
                .any(|word| !extract_punctuation(word).1.is_empty())
            {
                continue;
            }
            let ngram = build_ngram(ngram_words);

            if let Some((replacement, score)) =
                find_best_match(&ngram, custom_words, &custom_word_match_keys, threshold)
            {
                let is_better = best_match
                    .as_ref()
                    .is_none_or(|(_, _, best_score)| score < *best_score);
                if is_better {
                    best_match = Some((n, replacement, score));
                }
            }
        }

        if let Some((n, replacement, _)) = best_match {
            let ngram_words = &words[i..i + n];
            // Extract punctuation from first and last words of the n-gram.
            let (prefix, _) = extract_punctuation(ngram_words[0]);
            let (_, suffix) = extract_punctuation(ngram_words[n - 1]);

            // Preserve case from first word.
            let corrected = preserve_case_pattern(ngram_words[0], replacement);

            result.push(format!("{}{}{}", prefix, corrected, suffix));
            i += n;
        } else {
            result.push(words[i].to_string());
            i += 1;
        }
    }

    result.join(" ")
}

/// Comparison key for a phrase: lowercase, punctuation inside a word dropped
/// ("T.J" -> "tj", "O'Brian" -> "obrian"), punctuation between words kept
/// ("St. Clair" -> "st. clair") and punctuation around the phrase ignored.
/// Words stay separate, so "just in" never reads as "justin".
fn phrase_key<S: AsRef<str>>(words: &[S]) -> String {
    let key = words
        .iter()
        .map(|word| {
            let word = word.as_ref();
            let (prefix, suffix) = extract_punctuation(word);
            if prefix.len() == word.len() {
                // No letters or digits: a token such as "&" or "-".
                word.to_string()
            } else {
                format!("{prefix}{}{suffix}", build_match_key(word))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    key.trim_matches(|c: char| !c.is_alphanumeric()).to_string()
}

/// What a phrase key in the variant lookup stands for.
#[derive(Clone, Copy)]
enum LookupTarget<'a> {
    /// The text already reads as a custom word and is kept as it is.
    CustomWord,
    /// A listed spelling to replace with `word`.
    Spelling {
        word: &'a str,
        /// The spelling is written in capitals ("KC", "OK"), so capitals in
        /// the text are its normal form rather than shouting.
        in_capitals: bool,
        /// Punctuation typed around the spelling ("'90s", "Ave."). It belongs
        /// to the spelling, not to the sentence.
        leading: &'a str,
        trailing: &'a str,
    },
}

/// Phrase key -> what it stands for, plus the longest entry in words so the
/// caller knows how many window sizes to try. `None` when no usable spelling
/// is listed.
///
/// Custom words are entered first, so text that already reads as one ("Stacy
/// Miller", "Wendy's") is kept even when a listed spelling ("Stacy",
/// "Wendy") would match part of it. Only spellings listed under a word that is
/// still in `custom_words` are used, so removing a custom word also retires
/// its spellings.
fn build_variant_lookup<'a>(
    custom_words: &'a [String],
    variants: &'a CustomWordVariants,
) -> Option<(HashMap<String, LookupTarget<'a>>, usize)> {
    let mut lookup = HashMap::new();
    let mut max_words = 1;

    for word in custom_words {
        let words: Vec<&str> = word.split_whitespace().collect();
        let key = phrase_key(&words);
        if !key.is_empty() {
            lookup.entry(key).or_insert(LookupTarget::CustomWord);
            max_words = max_words.max(words.len());
        }
    }

    let mut has_spelling = false;
    // Walk the custom words in their stored order so a spelling listed under
    // two words always resolves to the first one, whatever the map order.
    for word in custom_words {
        let Some(spellings) = variants.get(word) else {
            continue;
        };
        for spelling in spellings {
            let words: Vec<&str> = spelling.split_whitespace().collect();
            let key = phrase_key(&words);
            if key.is_empty() || words.len() > MAX_VARIANT_WORDS {
                continue;
            }
            // An existing entry is a custom word or an earlier spelling;
            // either way it wins.
            if let Entry::Vacant(slot) = lookup.entry(key) {
                let (leading, _) = extract_punctuation(words[0]);
                let (_, trailing) = extract_punctuation(words[words.len() - 1]);
                let mut letters = spelling.chars().filter(|c| c.is_alphabetic()).peekable();
                let in_capitals = letters.peek().is_some() && letters.all(char::is_uppercase);
                slot.insert(LookupTarget::Spelling {
                    word: word.as_str(),
                    in_capitals,
                    leading,
                    trailing,
                });
                max_words = max_words.max(words.len());
                has_spelling = true;
            }
        }
    }

    has_spelling.then_some((lookup, max_words))
}

/// Rewrites listed alternate spellings to the custom word they belong to.
///
/// Unlike [`apply_custom_words`] this is an exact comparison with no
/// similarity threshold. Case and the punctuation around a spelling are
/// ignored, so "Stacy," and "STACY" match a listed "Stacy"; punctuation
/// between words must match, so "new, in" does not match "new in". An English
/// possessive keeps its ending ("Stacy's" becomes "Stacey's"). Being exact,
/// it is safe to run for every model, including whisper models that already
/// received the custom words as a decode prompt: the prompt only biases the
/// decoder, and a listed spelling is the user's explicit instruction.
/// Text that reads as one of `custom_words` is never rewritten, including a
/// custom word that contains a listed spelling ("Stacy Miller" when "Stacy"
/// is listed for "Stacey").
///
/// Matching works on space-separated words, so it targets languages such as
/// English and does not reach inside unspaced Chinese or Japanese text.
///
/// # Arguments
/// * `text` - The input text to correct
/// * `custom_words` - Custom words; spellings listed under other keys are ignored
/// * `variants` - Alternate spellings keyed by the custom word to write
///
/// # Returns
/// The text with listed spellings rewritten
pub fn apply_custom_word_variants(
    text: &str,
    custom_words: &[String],
    variants: &CustomWordVariants,
) -> String {
    apply_custom_word_corrections(text, custom_words, variants, 0.0, false)
}

/// Applies [`apply_custom_word_variants`] and then, when `fuzzy` is set,
/// [`apply_custom_words`] to the text around the rewritten spellings.
///
/// The fuzzy pass never sees the words the exact pass wrote. It would
/// otherwise match them again and, for example, drop a possessive
/// ("Stacey's" -> "Stacey") or pull a short word in front into the match
/// ("a Stacey" -> "Stacey").
///
/// # Arguments
/// * `text` - The input text to correct
/// * `custom_words` - List of custom words to match against
/// * `variants` - Alternate spellings keyed by the custom word to write
/// * `threshold` - Fuzzy similarity threshold, see [`apply_custom_words`]
/// * `fuzzy` - Whether to run the fuzzy pass at all
///
/// # Returns
/// The corrected text
pub fn apply_custom_word_corrections(
    text: &str,
    custom_words: &[String],
    variants: &CustomWordVariants,
    threshold: f64,
    fuzzy: bool,
) -> String {
    if custom_words.is_empty() {
        return text.to_string();
    }

    let segments = rewrite_variant_spans(text, custom_words, variants);
    if !segments.iter().any(|(_, rewritten)| *rewritten) {
        // Nothing listed was found: leave the text exactly as it was.
        return if fuzzy {
            apply_custom_words(text, custom_words, threshold)
        } else {
            text.to_string()
        };
    }

    let mut output: Vec<String> = Vec::with_capacity(segments.len());
    let mut untouched: Vec<String> = Vec::new();
    let flush = |untouched: &mut Vec<String>, output: &mut Vec<String>| {
        if !untouched.is_empty() {
            let run = untouched.join(" ");
            output.push(if fuzzy {
                apply_custom_words(&run, custom_words, threshold)
            } else {
                run
            });
            untouched.clear();
        }
    };
    for (token, rewritten) in segments {
        if rewritten {
            flush(&mut untouched, &mut output);
            output.push(token);
        } else {
            untouched.push(token);
        }
    }
    flush(&mut untouched, &mut output);

    output.join(" ")
}

/// Splits `text` into whitespace-separated tokens, replacing every listed
/// spelling with its custom word. Each token comes with whether it was
/// written by this pass.
fn rewrite_variant_spans(
    text: &str,
    custom_words: &[String],
    variants: &CustomWordVariants,
) -> Vec<(String, bool)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let Some((lookup, max_words)) = build_variant_lookup(custom_words, variants) else {
        return words.iter().map(|word| (word.to_string(), false)).collect();
    };

    let mut result: Vec<(String, bool)> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let matched = (1..=max_words.min(words.len() - i))
            .rev()
            .find_map(|n| match_variant(&words[i..i + n], &lookup).map(|m| (n, m)));

        match matched {
            Some((n, (LookupTarget::CustomWord, _))) => {
                result.extend(words[i..i + n].iter().map(|word| (word.to_string(), false)));
                i += n;
            }
            Some((
                n,
                (
                    LookupTarget::Spelling {
                        word,
                        in_capitals,
                        leading,
                        trailing,
                    },
                    possessive,
                ),
            )) => {
                let phrase = &words[i..i + n];
                let (mut prefix, _) = extract_punctuation(phrase[0]);
                let (_, mut suffix) = extract_punctuation(phrase[n - 1]);

                // Punctuation typed as part of the spelling goes with it,
                // unless a trailing period also ends the sentence.
                if !leading.is_empty() {
                    prefix = prefix.strip_suffix(leading).unwrap_or(prefix);
                }
                let next_opens_sentence = words.get(i + n).is_none_or(|next| {
                    next.chars()
                        .find(|c| c.is_alphanumeric())
                        .is_some_and(char::is_uppercase)
                });
                if !trailing.is_empty() && possessive.is_empty() && !next_opens_sentence {
                    suffix = suffix.strip_prefix(trailing).unwrap_or(suffix);
                }

                let previous = result.last().map(|(token, _)| token.as_str());
                let corrected = written_form_case(
                    phrase,
                    possessive,
                    word,
                    in_capitals,
                    previous_ends_sentence(previous),
                );
                let written = format!("{corrected}{possessive}");

                // A custom word that ends or starts with punctuation ("Acme
                // Inc.") already carries the mark the text had there.
                if let (Some(last), Some(first)) = (written.chars().last(), suffix.chars().next()) {
                    if last == first && !last.is_alphanumeric() {
                        suffix = &suffix[first.len_utf8()..];
                    }
                }
                if let (Some(first), Some(last)) = (written.chars().next(), prefix.chars().last()) {
                    if first == last && !first.is_alphanumeric() {
                        prefix = &prefix[..prefix.len() - last.len_utf8()];
                    }
                }

                result.push((format!("{prefix}{written}{suffix}"), true));
                i += n;
            }
            None => {
                result.push((words[i].to_string(), false));
                i += 1;
            }
        }
    }

    result
}

/// Looks up `phrase`, first as written and then without an English
/// possessive ending on its last word. Returns what it stands for and the
/// possessive ending to keep ("" when there is none).
fn match_variant<'w, 'a>(
    phrase: &[&'w str],
    lookup: &HashMap<String, LookupTarget<'a>>,
) -> Option<(LookupTarget<'a>, &'w str)> {
    // A match starts and ends on a word, so it never swallows a dash or other
    // stray punctuation token next to the spelling.
    let has_word = |word: &&str| word.chars().any(char::is_alphanumeric);
    if !phrase.first().is_some_and(has_word) || !phrase.last().is_some_and(has_word) {
        return None;
    }

    if let Some(target) = lookup.get(&phrase_key(phrase)) {
        return Some((*target, ""));
    }

    let (last, leading) = phrase.split_last()?;
    let (last_prefix, last_suffix) = extract_punctuation(last);
    let last_core = &last[last_prefix.len()..last.len() - last_suffix.len()];
    let stem = ["'s", "'S", "\u{2019}s", "\u{2019}S"]
        .iter()
        .find_map(|ending| last_core.strip_suffix(ending))
        .filter(|stem| stem.chars().any(char::is_alphanumeric))?;

    let mut stem_phrase = leading.to_vec();
    stem_phrase.push(&last[..last_prefix.len() + stem.len()]);
    lookup
        .get(&phrase_key(&stem_phrase))
        .map(|target| (*target, &last_core[stem.len()..]))
}

/// Title abbreviations whose period does not end a sentence.
const TITLE_ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "st", "jr", "sr", "mt", "vs",
];

/// Whether a word written after `previous` opens a sentence: there is no
/// previous token, or it ends with ". ! ? …" (closing quotes and brackets
/// aside) and is not an abbreviation such as "Dr." or an initial such as "J.".
fn previous_ends_sentence(previous: Option<&str>) -> bool {
    let Some(token) = previous else {
        return true;
    };
    let token = token.trim_end_matches(['"', '\'', '\u{201D}', '\u{2019}', ')', ']']);
    if !token.ends_with(['.', '!', '?', '\u{2026}']) {
        return false;
    }
    if !token.ends_with('.') {
        return true;
    }
    let word: String = token
        .trim_end_matches('.')
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    word.chars().count() > 1 && !TITLE_ABBREVIATIONS.contains(&word.as_str())
}

/// Casing for a custom word written in place of `phrase`. The custom word is
/// written as the user typed it ("iPhone", "danah boyd"), in capitals when the
/// phrase was shouted ("STACY" -> "STACEY", but "KC" listed in capitals stays
/// "Casey"), and with a capital first letter when an all-lowercase custom
/// word opens a sentence.
fn written_form_case(
    phrase: &[&str],
    possessive: &str,
    replacement: &str,
    spelling_in_capitals: bool,
    at_sentence_start: bool,
) -> String {
    let mut letters: Vec<char> = phrase
        .iter()
        .flat_map(|word| word.chars())
        .filter(|c| c.is_alphabetic())
        .collect();
    if !possessive.is_empty() {
        letters.pop(); // the "s" of "'s"
    }

    // A single capital ("J", "I") is not shouting.
    if !spelling_in_capitals && letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        return replacement.to_uppercase();
    }

    let starts_capital = letters.first().is_some_and(|c| c.is_uppercase());
    if at_sentence_start && starts_capital && !replacement.chars().any(char::is_uppercase) {
        let mut chars = replacement.chars();
        if let Some(first) = chars.next() {
            return first.to_uppercase().chain(chars).collect();
        }
    }

    replacement.to_string()
}

/// Preserves the case pattern of the original word when applying a replacement
fn preserve_case_pattern(original: &str, replacement: &str) -> String {
    if original.chars().all(|c| c.is_uppercase()) {
        replacement.to_uppercase()
    } else if original.chars().next().is_some_and(|c| c.is_uppercase()) {
        let mut chars: Vec<char> = replacement.chars().collect();
        if let Some(first_char) = chars.get_mut(0) {
            *first_char = first_char.to_uppercase().next().unwrap_or(*first_char);
        }
        chars.into_iter().collect()
    } else {
        replacement.to_string()
    }
}

/// Extracts punctuation prefix and suffix from a word
fn extract_punctuation(word: &str) -> (&str, &str) {
    // String slices use byte offsets. Derive both boundaries from char_indices
    // so multibyte punctuation such as `。` and `「」` can never be split.
    let prefix_end = word
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, _)| index)
        .unwrap_or(word.len());
    let suffix_start = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, c)| index + c.len_utf8())
        .unwrap_or(0);

    let prefix = if prefix_end > 0 {
        &word[..prefix_end]
    } else {
        ""
    };

    let suffix = if suffix_start < word.len() {
        &word[suffix_start..]
    } else {
        ""
    };

    (prefix, suffix)
}

/// Evidence for the language of the text being cleaned.
///
/// This intentionally describes the transcription output, not Handy's UI
/// language. Unknown output languages fail closed: built-in filler removal is
/// skipped rather than applying a language profile speculatively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputLanguageEvidence {
    UserSelected(String),
    ModelConstrained(String),
    /// The transcription model itself identified the language (audio-based
    /// LID, e.g. Whisper in auto mode).
    ModelDetected(String),
    /// Detected from the transcribed text with high confidence, constrained to
    /// the model's supported languages. Weakest accepted evidence.
    TextDetected(String),
    TranslatedToEnglish,
    Unknown,
}

impl OutputLanguageEvidence {
    pub(crate) fn language(&self) -> Option<&str> {
        match self {
            Self::UserSelected(language)
            | Self::ModelConstrained(language)
            | Self::ModelDetected(language)
            | Self::TextDetected(language) => Some(language),
            Self::TranslatedToEnglish => Some("en"),
            Self::Unknown => None,
        }
    }
}

/// Filler tokens that are not lexical words in any language Handy's models can
/// output, so removing them cannot corrupt text regardless of the (possibly
/// unknown) output language. Kept deliberately conservative: anything that is a
/// real word somewhere ("um" pt/de, "ha" es, "ah"/"eh" interjections, "mm"
/// millimetres) belongs in the language-gated lists instead.
const UNIVERSAL_FILLER_WORDS: &[&str] = &[
    "uh", "uhm", "umm", "uhh", "uhhh", "ehh", "ehm", "ahm", "hmm", "hm", "mmm", "хм", "ммм",
];

/// Filler words that are only safe to remove with evidence for the output
/// language, because the same token is a real word elsewhere (e.g. Portuguese
/// "um" = "a/an", German "um" = "at/around", Spanish "ha" = "has").
fn gated_filler_words_for_language(lang: &str) -> &'static [&'static str] {
    let base_lang = lang.split(&['-', '_'][..]).next().unwrap_or(lang);

    match base_lang {
        "en" => &["um", "ah", "eh"],
        "de" => &["äh", "ähm"],
        "fr" => &["euh"],
        _ => &[],
    }
}

static MULTI_SPACE_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s{2,}").unwrap());

/// Collapses repeated words (3+ repetitions) to a single instance.
/// E.g., "wh wh wh wh" -> "wh", "I I I I" -> "I"
fn collapse_stutters(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return text.to_string();
    }

    let mut result: Vec<&str> = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let word = words[i];
        let word_lower = word.to_lowercase();

        if word_lower.chars().all(|c| c.is_alphabetic()) {
            // Count consecutive repetitions (case-insensitive)
            let mut count = 1;
            while i + count < words.len() && words[i + count].to_lowercase() == word_lower {
                count += 1;
            }

            // If 3+ repetitions, collapse to single instance
            if count >= 3 {
                result.push(word);
                i += count;
            } else {
                result.push(word);
                i += 1;
            }
        } else {
            result.push(word);
            i += 1;
        }
    }

    result.join(" ")
}

/// Whether a word appended to `kept` would open a sentence: nothing but
/// whitespace so far, or the last visible character ends a sentence.
fn opens_sentence(kept: &str) -> bool {
    kept.trim_end()
        .chars()
        .next_back()
        .is_none_or(|c| matches!(c, '.' | '!' | '?' | '…'))
}

/// Appends `segment` to `kept`. While `capital_owed` is set, the first
/// alphanumeric character of `segment` is uppercased and the debt is settled.
fn push_restoring_capital(kept: &mut String, segment: &str, capital_owed: &mut bool) {
    if *capital_owed {
        if let Some((index, first)) = segment.char_indices().find(|(_, c)| c.is_alphanumeric()) {
            *capital_owed = false;
            kept.push_str(&segment[..index]);
            kept.extend(first.to_uppercase());
            kept.push_str(&segment[index + first.len_utf8()..]);
            return;
        }
    }
    kept.push_str(segment);
}

/// Deletes every match of one filler pattern. A capitalized filler that opened
/// a sentence hands its capital to the word that takes its place, so
/// "Um, so I think" becomes "So I think" rather than "so I think".
fn remove_filler_matches(text: &str, pattern: &Regex) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut resume = 0;
    let mut capital_owed = false;

    for filler in pattern.find_iter(text) {
        push_restoring_capital(&mut kept, &text[resume..filler.start()], &mut capital_owed);
        let capitalized = filler.as_str().starts_with(char::is_uppercase);
        capital_owed |= capitalized && opens_sentence(&kept);
        resume = filler.end();
    }
    push_restoring_capital(&mut kept, &text[resume..], &mut capital_owed);

    kept
}

/// Removes filler words from transcription output when enabled.
///
/// Built-in removal is two-tiered: [`UNIVERSAL_FILLER_WORDS`] apply regardless
/// of language evidence, while [`gated_filler_words_for_language`] tokens are
/// only removed when the output language is known. A custom list is an
/// explicit user override and replaces both tiers without requiring language
/// evidence. `Some(empty vec)` disables removal, preserving the legacy
/// power-user setting. The master toggle takes precedence over both built-in
/// and custom lists.
///
/// # Arguments
/// * `text` - The raw transcription text to filter
/// * `language` - Evidence for the language of the transcription output
/// * `custom_filler_words` - Optional user-provided filler word list. `Some(vec)` overrides
///   language defaults; `Some(empty vec)` disables filtering; `None` uses language defaults.
/// * `enabled` - Whether filler-word removal is enabled
///
/// # Returns
/// The text with configured filler words removed
pub fn remove_filler_words(
    text: &str,
    language: &OutputLanguageEvidence,
    custom_filler_words: &Option<Vec<String>>,
    enabled: bool,
) -> String {
    if !enabled {
        return text.to_string();
    }

    // Build filler patterns from custom list or the built-in tiers
    let patterns: Vec<Regex> = match custom_filler_words {
        Some(words) => words
            .iter()
            .filter_map(|word| Regex::new(&format!(r"(?i)\b{}\b[,.]?", regex::escape(word))).ok())
            .collect(),
        None => UNIVERSAL_FILLER_WORDS
            .iter()
            .chain(
                language
                    .language()
                    .map(gated_filler_words_for_language)
                    .unwrap_or_default(),
            )
            .map(|word| Regex::new(&format!(r"(?i)\b{}\b[,.]?", regex::escape(word))).unwrap())
            .collect(),
    };

    // Remove filler words
    let mut filtered = text.to_string();
    for pattern in &patterns {
        filtered = remove_filler_matches(&filtered, pattern);
    }

    filtered
}

/// Applies non-filler transcription cleanup.
///
/// Kept separate from [`remove_filler_words`] so disabling filler deletion
/// does not also disable the existing repeated-word and whitespace cleanup.
pub fn normalize_transcription_output(text: &str) -> String {
    let mut normalized = collapse_stutters(text);

    // Clean up multiple spaces to single space
    normalized = MULTI_SPACE_PATTERN
        .replace_all(&normalized, " ")
        .to_string();

    // Trim leading/trailing whitespace
    normalized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the complete cleanup sequence with an explicitly selected
    /// language. Individual tests below predate the split between filler
    /// removal and non-filler normalization.
    fn filter_transcription_output(
        text: &str,
        language: &str,
        custom_filler_words: &Option<Vec<String>>,
    ) -> String {
        let language = OutputLanguageEvidence::UserSelected(language.to_string());
        let filtered = remove_filler_words(text, &language, custom_filler_words, true);
        normalize_transcription_output(&filtered)
    }

    #[test]
    fn test_apply_custom_words_exact_match() {
        let text = "hello world";
        let custom_words = vec!["Hello".to_string(), "World".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_apply_custom_words_fuzzy_match() {
        let text = "helo wrold";
        let custom_words = vec!["hello".to_string(), "world".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_preserve_case_pattern() {
        assert_eq!(preserve_case_pattern("HELLO", "world"), "WORLD");
        assert_eq!(preserve_case_pattern("Hello", "world"), "World");
        assert_eq!(preserve_case_pattern("hello", "WORLD"), "WORLD");
    }

    #[test]
    fn test_extract_punctuation() {
        assert_eq!(extract_punctuation("hello"), ("", ""));
        assert_eq!(extract_punctuation("!hello?"), ("!", "?"));
        assert_eq!(extract_punctuation("...hello..."), ("...", "..."));
    }

    #[test]
    fn test_extract_punctuation_uses_unicode_boundaries() {
        assert_eq!(extract_punctuation("你好。"), ("", "。"));
        assert_eq!(extract_punctuation("「你好」"), ("「", "」"));
        assert_eq!(extract_punctuation("你好！"), ("", "！"));
    }

    #[test]
    fn test_empty_custom_words() {
        let text = "hello world";
        let custom_words = vec![];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_filter_filler_words() {
        let text = "So uhm I was thinking uh about this";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "So I was thinking about this");
    }

    #[test]
    fn test_filter_filler_words_case_insensitive() {
        let text = "UHM this is UH a test";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "This is a test");
    }

    #[test]
    fn test_filter_filler_words_with_punctuation() {
        let text = "Well, uhm, I think, uh. that's right";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Well, I think, that's right");
    }

    #[test]
    fn test_filter_cleans_whitespace() {
        let text = "Hello    world   test";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Hello world test");
    }

    #[test]
    fn test_filter_trims() {
        let text = "  Hello world  ";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_filter_combined() {
        let text = "  Uhm, so I was, uh, thinking about this  ";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "So I was, thinking about this");
    }

    #[test]
    fn test_filter_leading_filler_keeps_sentence_capital() {
        let result = filter_transcription_output("Um, so I think we should ship it.", "en", &None);
        assert_eq!(result, "So I think we should ship it.");

        let result = filter_transcription_output("That works. Um, let me check.", "en", &None);
        assert_eq!(result, "That works. Let me check.");

        // Mid-sentence there is no capital to hand over.
        let result = filter_transcription_output("He said, Um, not today.", "en", &None);
        assert_eq!(result, "He said, not today.");
    }

    #[test]
    fn test_filter_preserves_valid_text() {
        let text = "This is a completely normal sentence.";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "This is a completely normal sentence.");
    }

    #[test]
    fn test_filter_stutter_collapse() {
        let text = "w wh wh wh wh wh wh wh wh wh why";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "w wh why");
    }

    #[test]
    fn test_filter_stutter_short_words() {
        let text = "I I I I think so so so so";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "I think so");
    }

    #[test]
    fn test_filter_stutter_longer_words() {
        let text = "Check data doc doc doc doc documentation.";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Check data doc documentation.");
    }

    #[test]
    fn test_filter_stutter_mixed_case() {
        let text = "No NO no NO no";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "No");
    }

    #[test]
    fn test_filter_stutter_preserves_two_repetitions() {
        let text = "no no is fine";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "no no is fine");
    }

    #[test]
    fn test_filter_english_removes_um() {
        let text = "um I think um this is good";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "I think this is good");
    }

    #[test]
    fn test_filter_portuguese_preserves_um() {
        // "um" means "a/an" in Portuguese
        let text = "um gato bonito";
        let result = filter_transcription_output(text, "pt", &None);
        assert_eq!(result, "um gato bonito");
    }

    #[test]
    fn test_filter_spanish_preserves_ha() {
        // "ha" means "has" in Spanish
        let text = "ha sido un buen día";
        let result = filter_transcription_output(text, "es", &None);
        assert_eq!(result, "ha sido un buen día");
    }

    #[test]
    fn test_filter_language_code_with_region() {
        // "pt-BR" should normalize to "pt"
        let text = "um gato bonito";
        let result = filter_transcription_output(text, "pt-BR", &None);
        assert_eq!(result, "um gato bonito");
    }

    #[test]
    fn test_filter_custom_filler_words_override() {
        let custom = Some(vec!["okay".to_string(), "right".to_string()]);
        let text = "okay so I think right this works";
        let result = filter_transcription_output(text, "en", &custom);
        assert_eq!(result, "so I think this works");
    }

    #[test]
    fn test_filter_custom_filler_words_empty_disables() {
        let custom = Some(vec![]);
        let text = "So uhm I was thinking uh about this";
        let result = filter_transcription_output(text, "en", &custom);
        // No filler words removed since custom list is empty
        assert_eq!(result, "So uhm I was thinking uh about this");
    }

    #[test]
    fn test_filter_unknown_language_still_removes_universal_fillers() {
        let text = "uh I think uhm this works";
        let result = filter_transcription_output(text, "xx", &None);
        assert_eq!(result, "I think this works");
    }

    #[test]
    fn test_filter_unknown_language_does_not_remove_um() {
        let text = "um I think this works";
        let result = filter_transcription_output(text, "xx", &None);
        assert_eq!(result, "um I think this works");
    }

    #[test]
    fn test_filter_unknown_evidence_removes_universal_keeps_gated() {
        let filtered = remove_filler_words(
            "uhh bueno hmm creo que um ha llegado",
            &OutputLanguageEvidence::Unknown,
            &None,
            true,
        );
        assert_eq!(
            normalize_transcription_output(&filtered),
            "bueno creo que um ha llegado"
        );

        let cyrillic = remove_filler_words(
            "хм я думаю ммм это работает",
            &OutputLanguageEvidence::Unknown,
            &None,
            true,
        );
        assert_eq!(
            normalize_transcription_output(&cyrillic),
            "я думаю это работает"
        );
    }

    #[test]
    fn test_filter_german_gated_fillers_require_evidence() {
        let text = "äh ich glaube ähm das passt";

        let unknown = remove_filler_words(text, &OutputLanguageEvidence::Unknown, &None, true);
        assert_eq!(normalize_transcription_output(&unknown), text);

        let result = filter_transcription_output(text, "de", &None);
        assert_eq!(result, "ich glaube das passt");
    }

    #[test]
    fn test_filter_preserves_millimetre_unit() {
        // "mm" was removed from the filler lists because it eats units.
        let text = "the screw is 5 mm long";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "the screw is 5 mm long");
    }

    #[test]
    fn test_filter_detected_evidence_unlocks_gated_fillers() {
        let model = remove_filler_words(
            "um I think this works",
            &OutputLanguageEvidence::ModelDetected("en".to_string()),
            &None,
            true,
        );
        assert_eq!(normalize_transcription_output(&model), "I think this works");

        let text = remove_filler_words(
            "euh je pense que ça marche",
            &OutputLanguageEvidence::TextDetected("fr".to_string()),
            &None,
            true,
        );
        assert_eq!(
            normalize_transcription_output(&text),
            "je pense que ça marche"
        );
    }

    #[test]
    fn test_filter_master_toggle_disables_custom_and_builtin_removal() {
        let text = "um customword I think";
        let language = OutputLanguageEvidence::UserSelected("en".to_string());
        let custom = Some(vec!["customword".to_string()]);

        let result = remove_filler_words(text, &language, &custom, false);

        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_custom_words_apply_without_language_evidence() {
        let custom = Some(vec!["customword".to_string()]);
        let text = "customword should be removed but um should remain";

        let filtered = remove_filler_words(text, &OutputLanguageEvidence::Unknown, &custom, true);
        let result = normalize_transcription_output(&filtered);

        assert_eq!(result, "should be removed but um should remain");
    }

    #[test]
    fn test_apply_custom_words_ngram_two_words() {
        let text = "il cui nome è Charge B, che permette";
        let custom_words = vec!["ChargeBee".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("ChargeBee,"), "unexpected result: {result}");
        assert!(!result.contains("Charge B"));
    }

    #[test]
    fn test_apply_custom_words_ngram_three_words() {
        let text = "use Chat G P T for this";
        let custom_words = vec!["ChatGPT".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("ChatGPT"));
    }

    #[test]
    fn test_apply_custom_words_prefers_longer_ngram() {
        let text = "Open AI GPT model";
        let custom_words = vec!["OpenAI".to_string(), "GPT".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "OpenAI GPT model");
    }

    #[test]
    fn test_apply_custom_words_ngram_preserves_case() {
        let text = "CHARGE B is great";
        let custom_words = vec!["ChargeBee".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("CHARGEBEE"));
    }

    #[test]
    fn test_apply_custom_words_ngram_with_spaces_in_custom() {
        // Custom word with space should also match against split words
        let text = "using Mac Book Pro";
        let custom_words = vec!["MacBook Pro".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "using MacBook Pro");
    }

    #[test]
    fn test_apply_custom_words_trailing_number_not_doubled() {
        // Verify that trailing non-alpha chars (like numbers) aren't double-counted
        // between build_ngram stripping them and extract_punctuation capturing them
        let text = "use GPT4 for this";
        let custom_words = vec!["GPT-4".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        // Should NOT produce "GPT-44" (double-counting the trailing 4)
        assert!(
            !result.contains("GPT-44"),
            "got double-counted result: {}",
            result
        );
    }

    #[test]
    fn test_apply_custom_words_matches_ampersand_word() {
        let text = "send it to RD for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_matches_spoken_ampersand_word() {
        let text = "send it to R and D for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_preserves_ampersand_word() {
        let text = "send it to R&D for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_handles_unicode_punctuation() {
        let text = "「Handee。」";
        let custom_words = vec!["Handy".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "「Handy。」");
    }

    #[test]
    fn test_apply_custom_words_skips_cjk_fuzzy_matching() {
        let text = "你好。";
        let custom_words = vec!["你号".to_string()];
        let result = apply_custom_words(text, &custom_words, 1.0);
        assert_eq!(result, text);
    }

    fn variants(entries: &[(&str, &[&str])]) -> CustomWordVariants {
        entries
            .iter()
            .map(|(word, spellings)| {
                (
                    (*word).to_string(),
                    spellings.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect()
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn test_apply_custom_word_variants_rewrites_listed_spelling() {
        let custom_words = words(&["Stacey"]);
        let variants = variants(&[("Stacey", &["Stacy", "Stacie"])]);

        let result =
            apply_custom_word_variants("I met Stacy and Stacie today", &custom_words, &variants);

        assert_eq!(result, "I met Stacey and Stacey today");
    }

    #[test]
    fn test_apply_custom_word_variants_keeps_punctuation_and_case() {
        let custom_words = words(&["Stacey"]);
        let variants = variants(&[("Stacey", &["Stacy"])]);

        let result = apply_custom_word_variants(
            "Hi Stacy, is STACY here? (stacy)",
            &custom_words,
            &variants,
        );

        // A lowercase hit keeps the custom word's own capitalization.
        assert_eq!(result, "Hi Stacey, is STACEY here? (Stacey)");
    }

    #[test]
    fn test_apply_custom_word_variants_keeps_possessive_ending() {
        let custom_words = words(&["Stacey", "Nguyen"]);
        let variants = variants(&[("Stacey", &["Stacy"]), ("Nguyen", &["new in"])]);

        let result = apply_custom_word_variants(
            "Stacy's car, STACY'S keys, Stacy\u{2019}s bag and new in's office.",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "Stacey's car, STACEY'S keys, Stacey\u{2019}s bag and Nguyen's office."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_matches_multi_word_spelling() {
        let custom_words = words(&["Nguyen"]);
        let variants = variants(&[("Nguyen", &["new in", "win"])]);

        let result = apply_custom_word_variants(
            "Dr. New in said hello. Thanks, Win. NEW IN agrees.",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "Dr. Nguyen said hello. Thanks, Nguyen. NGUYEN agrees."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_does_not_cross_punctuation() {
        let custom_words = words(&["Nguyen"]);
        let variants = variants(&[("Nguyen", &["new in"])]);

        let result =
            apply_custom_word_variants("something new, in the box", &custom_words, &variants);

        assert_eq!(result, "something new, in the box");
    }

    #[test]
    fn test_apply_custom_word_variants_skips_pure_punctuation_tokens() {
        let custom_words = words(&["Nguyen"]);
        let variants = variants(&[("Nguyen", &["new in"])]);

        let result = apply_custom_word_variants("so - new in - yes", &custom_words, &variants);

        assert_eq!(result, "so - Nguyen - yes");
    }

    #[test]
    fn test_apply_custom_word_variants_is_exact_not_fuzzy() {
        // "when" and "wine" share a Soundex code with "win"; a fuzzy match
        // would turn ordinary words into the name.
        let custom_words = words(&["Nguyen"]);
        let variants = variants(&[("Nguyen", &["win"])]);

        let result =
            apply_custom_word_variants("when we win we drink wine", &custom_words, &variants);

        assert_eq!(result, "when we Nguyen we drink wine");
    }

    #[test]
    fn test_apply_custom_word_variants_never_overrides_a_custom_word() {
        // "Stacy" is itself a custom word, so listing it under "Stacey" must
        // not rewrite it.
        let custom_words = words(&["Stacey", "Stacy"]);
        let variants = variants(&[("Stacey", &["Stacy", "Stacie"])]);

        let result = apply_custom_word_variants("Stacy and Stacie", &custom_words, &variants);

        assert_eq!(result, "Stacy and Stacey");
    }

    #[test]
    fn test_apply_custom_word_variants_ignores_spellings_of_removed_words() {
        let custom_words = words(&["Handy"]);
        let variants = variants(&[("Nguyen", &["win"])]);

        let result = apply_custom_word_variants("we win", &custom_words, &variants);

        assert_eq!(result, "we win");
    }

    #[test]
    fn test_apply_custom_word_variants_first_custom_word_wins_shared_spelling() {
        let custom_words = words(&["Zed", "Abby"]);
        let variants = variants(&[("Abby", &["zee"]), ("Zed", &["zee"])]);

        for _ in 0..20 {
            let result = apply_custom_word_variants("say zee", &custom_words, &variants);
            assert_eq!(result, "say Zed");
        }
    }

    #[test]
    fn test_apply_custom_word_variants_ignores_empty_and_overlong_spellings() {
        let custom_words = words(&["Stacey"]);
        let variants = variants(&[(
            "Stacey",
            &[
                "",
                "   ",
                "stacey",
                "!!!",
                "one two three four five six seven",
            ],
        )]);

        let result = apply_custom_word_variants(
            "Stacey !!! stacey one two three four five six seven",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "Stacey !!! stacey one two three four five six seven"
        );
        assert_eq!(
            apply_custom_word_variants("unchanged", &custom_words, &CustomWordVariants::new()),
            "unchanged"
        );
    }

    #[test]
    fn test_apply_custom_word_variants_matches_longest_spelling_first() {
        let custom_words = words(&["Mary Ann", "Anne"]);
        let variants = variants(&[("Mary Ann", &["marry an"]), ("Anne", &["an"])]);

        let result = apply_custom_word_variants("I told marry an and an", &custom_words, &variants);

        assert_eq!(result, "I told Mary Ann and Anne");
    }

    #[test]
    fn test_apply_custom_word_variants_keeps_word_boundaries() {
        // A one-word spelling must not match two words that spell it when
        // joined, and a two-word spelling must not match one word.
        let custom_words = words(&["Justyn", "Nguyen", "Simone", "Karmen"]);
        let variants = variants(&[
            ("Justyn", &["Justin"]),
            ("Nguyen", &["new in"]),
            ("Simone", &["some one"]),
            ("Karmen", &["Carmen"]),
        ]);

        let result = apply_custom_word_variants(
            "Bring an umbrella just in case, Justin. Someone called some one. The car men left with Carmen.",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "Bring an umbrella just in case, Justyn. Someone called Simone. The car men left with Karmen."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_matches_punctuation_inside_spelling() {
        let custom_words = words(&["Sinclair", "TJ Maxx", "AT&T"]);
        let variants = variants(&[
            ("Sinclair", &["St. Clair"]),
            ("TJ Maxx", &["T.J. Max"]),
            ("AT&T", &["AT & T"]),
        ]);

        let result = apply_custom_word_variants(
            "I met St. Clair at T.J. Max's door, not St Clair. Call AT & T.",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "I met Sinclair at TJ Maxx's door, not St Clair. Call AT&T."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_writes_the_custom_word_as_typed() {
        let custom_words = words(&["iPhone", "danah boyd", "Jae", "kubernetes"]);
        let variants = variants(&[
            ("iPhone", &["eye phone"]),
            ("danah boyd", &["Dana Boyd"]),
            ("Jae", &["J"]),
            ("kubernetes", &["cooper netties"]),
        ]);

        let result = apply_custom_word_variants(
            "Eye phone sales grew. I spoke with Dana Boyd and J today. Cooper netties runs it, and so does COOPER NETTIES.",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "iPhone sales grew. I spoke with danah boyd and Jae today. Kubernetes runs it, and so does KUBERNETES."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_leaves_text_alone_without_a_match() {
        let custom_words = words(&["Stacey"]);
        let variants = variants(&[("Stacey", &["Stacy"])]);
        let text = "first line\nsecond  line";

        assert_eq!(
            apply_custom_word_variants(text, &custom_words, &variants),
            text
        );
    }

    #[test]
    fn test_apply_custom_word_corrections_keeps_fuzzy_pass_off_rewritten_words() {
        let custom_words = words(&["Stacey", "Kubernetes", "Nguyen"]);
        let variants = variants(&[
            ("Stacey", &["Stacy"]),
            ("Kubernetes", &["cooper netties"]),
            ("Nguyen", &["win"]),
        ]);
        let correct =
            |text: &str| apply_custom_word_corrections(text, &custom_words, &variants, 0.18, true);

        // The fuzzy pass would read "Stacey's" as "Stacey" and "a Stacey",
        // "to Kubernetes" or "I Nguyen" as one longer word.
        assert_eq!(correct("Stacy's car"), "Stacey's car");
        assert_eq!(correct("I met a Stacy today"), "I met a Stacey today");
        assert_eq!(
            correct("deploy to cooper netties now"),
            "deploy to Kubernetes now"
        );
        assert_eq!(correct("Then I win said"), "Then I Nguyen said");
        // Text around a rewrite still gets fuzzy correction.
        assert_eq!(correct("Stacy and Kubernetis"), "Stacey and Kubernetes");
        // Without a listed spelling the fuzzy pass runs exactly as before.
        assert_eq!(correct("helo Stacie"), "helo Stacey");
    }

    #[test]
    fn test_apply_custom_word_corrections_without_fuzzy_only_rewrites_spellings() {
        let custom_words = words(&["Stacey"]);
        let variants = variants(&[("Stacey", &["Stacy"])]);

        let result = apply_custom_word_corrections(
            "Stacy and Stacie",
            &custom_words,
            &variants,
            0.18,
            false,
        );

        assert_eq!(result, "Stacey and Stacie");
    }

    #[test]
    fn test_apply_custom_word_variants_keeps_custom_words_that_contain_a_spelling() {
        let custom_words = words(&[
            "Stacey",
            "Stacy Miller",
            "Jon",
            "John Deere",
            "Wendi",
            "Wendy's",
        ]);
        let variants = variants(&[
            ("Stacey", &["Stacy"]),
            ("Jon", &["John"]),
            ("Wendi", &["Wendy"]),
        ]);

        let result = apply_custom_word_variants(
            "I emailed Stacy Miller and Stacy. Stacy Miller's John Deere is at Wendy's, John said, and Wendy agreed.",
            &custom_words,
            &variants,
        );

        assert_eq!(
            result,
            "I emailed Stacy Miller and Stacey. Stacy Miller's John Deere is at Wendy's, Jon said, and Wendi agreed."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_does_not_shout_spellings_listed_in_capitals() {
        let custom_words = words(&["Casey", "okay", "Stacey"]);
        let variants = variants(&[
            ("Casey", &["KC"]),
            ("okay", &["OK"]),
            ("Stacey", &["Stacy"]),
        ]);

        let result = apply_custom_word_variants(
            "Ask KC if that is OK with STACY.",
            &custom_words,
            &variants,
        );

        assert_eq!(result, "Ask Casey if that is okay with STACEY.");
    }

    #[test]
    fn test_apply_custom_word_variants_keeps_punctuation_typed_with_the_spelling() {
        let custom_words = words(&["Avenue", "Casey", "nineties"]);
        let variants = variants(&[
            ("Avenue", &["Ave."]),
            ("Casey", &["K.C."]),
            ("nineties", &["'90s"]),
        ]);
        let rewrite = |text: &str| apply_custom_word_variants(text, &custom_words, &variants);

        assert_eq!(
            rewrite("Turn on Main Ave. and go."),
            "Turn on Main Avenue and go."
        );
        assert_eq!(rewrite("Ask K.C. about it."), "Ask Casey about it.");
        // The period also ends the sentence here, so it stays.
        assert_eq!(
            rewrite("Turn on Main Ave. Then go."),
            "Turn on Main Avenue. Then go."
        );
        assert_eq!(rewrite("It is on Main Ave."), "It is on Main Avenue.");
        assert_eq!(
            rewrite("Back in the '90s, we met."),
            "Back in the nineties, we met."
        );
    }

    #[test]
    fn test_apply_custom_word_variants_does_not_double_the_custom_words_punctuation() {
        let custom_words = words(&["Acme Inc.", "Yahoo!"]);
        let variants = variants(&[("Acme Inc.", &["Acme Ink"]), ("Yahoo!", &["Ya hoo"])]);

        let result = apply_custom_word_variants(
            "I work at Acme Ink. I use Ya hoo! Acme Ink, too.",
            &custom_words,
            &variants,
        );

        assert_eq!(result, "I work at Acme Inc. I use Yahoo! Acme Inc., too.");
    }

    #[test]
    fn test_apply_custom_word_variants_detects_sentence_starts_around_quotes_and_titles() {
        let custom_words = words(&["danah boyd"]);
        let variants = variants(&[("danah boyd", &["Dana Boyd"])]);
        let rewrite = |text: &str| apply_custom_word_variants(text, &custom_words, &variants);

        assert_eq!(
            rewrite("He said \"Hi.\" Dana Boyd left."),
            "He said \"Hi.\" Danah boyd left."
        );
        assert_eq!(
            rewrite("I met Mr. Dana Boyd today."),
            "I met Mr. danah boyd today."
        );
        assert_eq!(rewrite("Ask J. Dana Boyd."), "Ask J. danah boyd.");
        assert_eq!(rewrite("Done! Dana Boyd next."), "Done! Danah boyd next.");
    }
}
