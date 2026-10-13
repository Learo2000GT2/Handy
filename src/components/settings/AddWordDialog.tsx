import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import { commands } from "@/bindings";
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

// Mirrors ADD_CUSTOM_WORD_EVENT in src-tauri/src/actions.rs: the Add Word
// shortcut, with the text selected in the focused app as payload.
const ADD_CUSTOM_WORD_EVENT = "add-custom-word-requested";
// Opens the dialog from inside the app (the Custom Words setting).
export const OPEN_ADD_WORD_EVENT = "handy:open-add-word";

export const openAddWordDialog = (prefill = "") =>
  window.dispatchEvent(
    new CustomEvent<string>(OPEN_ADD_WORD_EVENT, { detail: prefill }),
  );

// Longest pronunciation sample before it stops on its own.
const SAMPLE_SECONDS = 6;

// A selection longer than one custom word, or spanning lines, was not meant
// as a word to add; open the dialog empty instead.
const asSelectedWord = (selection: string) => {
  const text = selection.trim();
  return text.length > 0 && text.length <= MAX_ENTRY_LENGTH && !/\n/.test(text)
    ? text
    : "";
};

// What Handy typed for a pronunciation sample, without the sentence
// punctuation it adds around a single word ("Stacy." -> "Stacy").
const asSpokenForm = (text: string) =>
  normalizeSpelling(text).replace(/^[\s"“(]+|[\s.,!?;:"”)]+$/g, "");

type Step = "edit" | "train";
type TrainState =
  | { kind: "ready" }
  | { kind: "starting" }
  | { kind: "recording"; secondsLeft: number }
  | { kind: "checking" }
  | { kind: "result"; heard: string; saved: boolean; correct: boolean }
  | { kind: "error"; message: string };

/**
 * Dragon-style "Add word or phrase" dialog, opened by the Add Word shortcut
 * (with the selected text) or from the Custom Words setting. The written form
 * becomes a custom word; the spoken form, if different, is what Handy types
 * when the word is said ("Stacy" for "Stacey") and is rewritten to the
 * written form. "Train the pronunciation" records the user saying the word
 * and saves what Handy types as a spoken form when it is not already correct.
 */
export const AddWordDialog: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();
  const [open, setOpen] = useState(false);
  const [step, setStep] = useState<Step>("edit");
  const [written, setWritten] = useState("");
  const [spoken, setSpoken] = useState("");
  const [train, setTrain] = useState(false);
  const [showHelp, setShowHelp] = useState(false);
  const [saving, setSaving] = useState(false);
  const [trainState, setTrainState] = useState<TrainState>({ kind: "ready" });
  const [trainedWord, setTrainedWord] = useState("");
  const [attempted, setAttempted] = useState(false);
  const writtenRef = useRef<HTMLInputElement>(null);
  const recordButtonRef = useRef<HTMLButtonElement>(null);
  // Bumped whenever the dialog opens or closes, so a sample that finishes
  // after the user moved on is dropped instead of saved.
  const session = useRef(0);
  // True while the microphone records a sample.
  const recording = useRef(false);
  const countdown = useRef<number | null>(null);
  const autoStop = useRef<number | null>(null);

  const customWords = getSetting("custom_words") || [];
  const spellingMap: SpellingMap = getSetting("custom_word_variants") || {};

  const stopTimers = () => {
    if (countdown.current !== null) window.clearInterval(countdown.current);
    if (autoStop.current !== null) window.clearTimeout(autoStop.current);
    countdown.current = null;
    autoStop.current = null;
  };

  // Ends the dialog's current session: stops timers and drops a sample that
  // is still recording. Uses only refs, so the event listeners registered
  // once on mount can call it.
  const endSession = () => {
    session.current += 1;
    stopTimers();
    if (recording.current) {
      recording.current = false;
      void commands.cancelPronunciationSample();
    }
  };

  const reset = (prefill: string) => {
    endSession();
    setStep("edit");
    setWritten(asSelectedWord(prefill));
    setSpoken("");
    setTrain(false);
    setShowHelp(false);
    setTrainState({ kind: "ready" });
    setOpen(true);
  };

  useEffect(() => {
    const unlisten = listen<string>(ADD_CUSTOM_WORD_EVENT, (event) =>
      reset(event.payload ?? ""),
    );
    const onOpen = (event: Event) =>
      reset((event as CustomEvent<string>).detail ?? "");
    window.addEventListener(OPEN_ADD_WORD_EVENT, onOpen);
    return () => {
      unlisten.then((fn) => fn());
      window.removeEventListener(OPEN_ADD_WORD_EVENT, onOpen);
      endSession();
    };
  }, []);

  // Select a prefilled word so typing replaces it, as Dragon does.
  useEffect(() => {
    if (open && step === "edit") {
      const id = window.setTimeout(() => writtenRef.current?.select(), 0);
      return () => window.clearTimeout(id);
    }
  }, [open, step]);

  const close = () => {
    endSession();
    setOpen(false);
  };

  const fail = (key: string, params: Record<string, unknown> = {}) => {
    toast.error(t(`settings.advanced.customWords.${key}`, params));
  };

  // Checks that `form` can be a spoken form of `word`; reports why not.
  const spokenFormProblem = (
    word: string,
    form: string,
    words: string[],
    map: SpellingMap,
  ): boolean => {
    if (!isUsableSpelling(form) || form.length > MAX_ENTRY_LENGTH) {
      fail("addWord.spokenInvalid", { spoken: form });
      return true;
    }
    if (form.split(" ").length > MAX_SPELLING_WORDS) {
      fail("spellingTooManyWords", { max: MAX_SPELLING_WORDS });
      return true;
    }
    if (words.some((existing) => phraseKey(existing) === phraseKey(form))) {
      fail("spellingIsCustomWord", { spelling: form });
      return true;
    }
    const owner = spellingOwner(words, map, form);
    if (owner && owner !== word) {
      fail("spellingDuplicate", { spelling: form, word: owner });
      return true;
    }
    return false;
  };

  // Adds `form` under `word` unless it is already listed.
  const saveSpokenForm = async (
    word: string,
    form: string,
    map: SpellingMap,
  ): Promise<void> => {
    const existing = spellingsOf(map, word);
    if (existing.some((listed) => phraseKey(listed) === phraseKey(form))) {
      return;
    }
    await updateSetting("custom_word_variants", {
      ...map,
      [word]: [...existing, form],
    });
  };

  // Validates and saves the written and spoken forms. Returns the saved
  // written form, or null when nothing was saved.
  const add = async (): Promise<string | null> => {
    const word = normalizeCustomWord(written);
    const form = normalizeSpelling(spoken);
    if (!word || word.length > MAX_ENTRY_LENGTH) return null;

    const wordOwner = spellingOwner(customWords, spellingMap, word);
    if (wordOwner) {
      fail("spellingDuplicate", { spelling: word, word: wordOwner });
      return null;
    }
    const addForm = form !== "" && phraseKey(form) !== phraseKey(word);
    const exists = customWords.includes(word);
    if (exists && !addForm && !train) {
      fail("duplicate", { word });
      return null;
    }
    if (addForm && spokenFormProblem(word, form, customWords, spellingMap)) {
      return null;
    }

    // The word goes in first: the backend drops spoken forms whose word is
    // not a custom word.
    if (!exists) {
      await updateSetting("custom_words", [...customWords, word]);
    }
    if (addForm) {
      await saveSpokenForm(word, form, spellingMap);
    }
    if (!exists || addForm) {
      toast.success(
        addForm
          ? t("settings.advanced.customWords.addWord.addedSpoken", {
              spoken: form,
              word,
            })
          : t("settings.advanced.customWords.addWord.addedWord", { word }),
      );
    }
    return word;
  };

  const handleAdd = async () => {
    if (saving) return;
    setSaving(true);
    try {
      const word = await add();
      if (!word) return;
      if (train) {
        setTrainedWord(word);
        setAttempted(false);
        setTrainState({ kind: "ready" });
        setStep("train");
        window.setTimeout(() => recordButtonRef.current?.focus(), 0);
      } else {
        setOpen(false);
      }
    } finally {
      setSaving(false);
    }
  };

  // The settings as saved now, not as of this render: the word was saved a
  // moment before the sample.
  const latestSettings = () => ({
    words: getSetting("custom_words") || [],
    map: (getSetting("custom_word_variants") || {}) as SpellingMap,
  });

  const finishSample = async (word: string) => {
    // The Stop button and the time limit can both end a sample.
    if (!recording.current) return;
    recording.current = false;
    stopTimers();
    const current = session.current;
    setTrainState({ kind: "checking" });
    const result = await commands.stopPronunciationSample();
    if (session.current !== current) return;
    if (result.status === "error") {
      setTrainState({ kind: "error", message: result.error });
      return;
    }
    const heard = asSpokenForm(result.data);
    if (!heard) {
      setTrainState({
        kind: "error",
        message: t("settings.advanced.customWords.addWord.nothingHeard"),
      });
      return;
    }
    if (phraseKey(heard) === phraseKey(word)) {
      setTrainState({ kind: "result", heard, saved: false, correct: true });
      return;
    }
    const { words, map } = latestSettings();
    if (spokenFormProblem(word, heard, words, map)) {
      setTrainState({ kind: "result", heard, saved: false, correct: false });
      return;
    }
    await saveSpokenForm(word, heard, map);
    if (session.current !== current) return;
    setTrainState({ kind: "result", heard, saved: true, correct: false });
  };

  const startSample = async () => {
    if (recording.current) return;
    const current = session.current;
    const word = trainedWord;
    setAttempted(true);
    setTrainState({ kind: "starting" });
    const result = await commands.startPronunciationSample();
    if (session.current !== current) {
      // Closed while the microphone was starting.
      if (result.status === "ok") void commands.cancelPronunciationSample();
      return;
    }
    if (result.status === "error") {
      setTrainState({ kind: "error", message: result.error });
      return;
    }
    recording.current = true;
    setTrainState({ kind: "recording", secondsLeft: SAMPLE_SECONDS });
    countdown.current = window.setInterval(() => {
      setTrainState((state) =>
        state.kind === "recording" && state.secondsLeft > 1
          ? { kind: "recording", secondsLeft: state.secondsLeft - 1 }
          : state,
      );
    }, 1000);
    autoStop.current = window.setTimeout(
      () => void finishSample(word),
      SAMPLE_SECONDS * 1000,
    );
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void handleAdd();
    }
  };

  const canAdd = !saving && normalizeCustomWord(written) !== "";
  const prefix = "settings.advanced.customWords.addWord";

  const editBody = (
    <div className="flex flex-col gap-3">
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
        <label className="flex flex-col justify-between gap-1 text-sm">
          <span>{t(`${prefix}.writtenLabel`)}</span>
          <Input
            ref={writtenRef}
            type="text"
            value={written}
            onChange={(e) => setWritten(e.target.value)}
            onKeyDown={handleKeyDown}
            variant="compact"
          />
        </label>
        <label className="flex flex-col justify-between gap-1 text-sm">
          <span>{t(`${prefix}.spokenLabel`)}</span>
          <Input
            type="text"
            value={spoken}
            onChange={(e) => setSpoken(e.target.value)}
            onKeyDown={handleKeyDown}
            variant="compact"
          />
        </label>
      </div>
      <label className="flex items-center gap-2 text-sm cursor-pointer">
        <input
          type="checkbox"
          checked={train}
          onChange={(e) => setTrain(e.target.checked)}
          className="accent-logo-primary"
        />
        <span>{t(`${prefix}.trainLabel`)}</span>
      </label>
      {showHelp && (
        <div className="text-xs text-mid-gray flex flex-col gap-1 border-t border-mid-gray/20 pt-2">
          <p>{t(`${prefix}.helpWritten`)}</p>
          <p>{t(`${prefix}.helpSpoken`)}</p>
          <p>{t(`${prefix}.helpTrain`)}</p>
          <p>{t(`${prefix}.helpCaution`)}</p>
        </div>
      )}
    </div>
  );

  const trainBody = (
    <div className="flex flex-col gap-3 text-sm">
      <p>
        {t(`${prefix}.trainPrompt`, {
          word: trainedWord,
          seconds: SAMPLE_SECONDS,
        })}
      </p>
      <div className="flex items-center gap-3">
        {trainState.kind === "recording" ? (
          <Button
            variant="danger"
            size="md"
            onClick={() => void finishSample(trainedWord)}
          >
            {t(`${prefix}.stop`)}
          </Button>
        ) : (
          <Button
            ref={recordButtonRef}
            variant="primary"
            size="md"
            onClick={() => void startSample()}
            disabled={
              trainState.kind === "starting" || trainState.kind === "checking"
            }
          >
            {attempted ? t(`${prefix}.tryAgain`) : t(`${prefix}.record`)}
          </Button>
        )}
        <span className="text-mid-gray" role="status" aria-live="polite">
          {trainState.kind === "starting" && t(`${prefix}.starting`)}
          {trainState.kind === "recording" &&
            t(`${prefix}.listening`, { seconds: trainState.secondsLeft })}
          {trainState.kind === "checking" && t(`${prefix}.checking`)}
        </span>
      </div>
      {trainState.kind === "result" && (
        <div className="flex flex-col gap-1" role="status">
          <p>{t(`${prefix}.heard`, { text: trainState.heard })}</p>
          <p className="text-mid-gray">
            {trainState.correct
              ? t(`${prefix}.alreadyCorrect`, { word: trainedWord })
              : trainState.saved
                ? t(`${prefix}.savedSpoken`, {
                    spoken: trainState.heard,
                    word: trainedWord,
                  })
                : t(`${prefix}.notSaved`)}
          </p>
        </div>
      )}
      {trainState.kind === "error" && (
        <p className="text-red-500" role="alert">
          {trainState.message}
        </p>
      )}
    </div>
  );

  const editFooter = (
    <div className="flex justify-end gap-2">
      <Button
        variant="primary"
        size="md"
        onClick={() => void handleAdd()}
        disabled={!canAdd}
      >
        {t(`${prefix}.add`)}
      </Button>
      <Button variant="secondary" size="md" onClick={close}>
        {t(`${prefix}.cancel`)}
      </Button>
      <Button
        variant="secondary"
        size="md"
        onClick={() => setShowHelp((shown) => !shown)}
        aria-expanded={showHelp}
      >
        {t(`${prefix}.help`)}
      </Button>
    </div>
  );

  const trainFooter = (
    <div className="flex justify-end gap-2">
      <Button
        variant="primary"
        size="md"
        onClick={close}
        disabled={
          trainState.kind === "starting" ||
          trainState.kind === "recording" ||
          trainState.kind === "checking"
        }
      >
        {t(`${prefix}.done`)}
      </Button>
    </div>
  );

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => (next ? setOpen(true) : close())}
      title={t(step === "edit" ? `${prefix}.title` : `${prefix}.trainTitle`)}
      closeLabel={t("common.close")}
      initialFocusRef={step === "edit" ? writtenRef : recordButtonRef}
      footer={step === "edit" ? editFooter : trainFooter}
    >
      {step === "edit" ? editBody : trainBody}
    </Dialog>
  );
};
