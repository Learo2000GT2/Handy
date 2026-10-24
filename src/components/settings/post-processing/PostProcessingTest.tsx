import React, { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import { commands, type PostProcessTest } from "@/bindings";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { SettingContainer } from "../../ui/SettingContainer";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { Textarea } from "../../ui/Textarea";
import {
  postProcessProblemDetail,
  postProcessProblemText,
} from "./problemText";

type TestState =
  | { phase: "idle" }
  | { phase: "testing" }
  | { phase: "done"; result: PostProcessTest }
  /** The test itself could not run (not a provider problem). */
  | { phase: "failed"; error: string };

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

/**
 * Runs the saved provider, model and prompt on a sample dictation through
 * the same backend path as a dictation, and shows the result or what went
 * wrong.
 */
export const PostProcessingTest: React.FC = () => {
  const { t } = useTranslation();
  const [text, setText] = useState(() =>
    t("settings.postProcessing.test.sample"),
  );
  const [state, setState] = useState<TestState>({ phase: "idle" });
  // Only the newest test may update the result.
  const testIdRef = useRef(0);

  const handleTest = async () => {
    const id = ++testIdRef.current;
    setState({ phase: "testing" });
    let next: TestState;
    try {
      const result = await commands.testPostProcess(text);
      next =
        result.status === "ok"
          ? { phase: "done", result: result.data }
          : { phase: "failed", error: result.error };
    } catch (error) {
      next = { phase: "failed", error: errorText(error) };
    }
    if (id === testIdRef.current) setState(next);
  };

  const testing = state.phase === "testing";

  const result = (() => {
    if (state.phase === "failed") {
      return (
        <Alert variant="error" contained>
          {t("settings.postProcessing.test.failed", { error: state.error })}
        </Alert>
      );
    }
    if (state.phase !== "done") return null;
    const { output, problem, elapsed_ms } = state.result;
    if (problem) {
      const detail = postProcessProblemDetail(t, problem);
      return (
        <Alert variant="error" contained>
          <p>{postProcessProblemText(t, problem)}</p>
          {detail && (
            <p className="mt-1 text-xs break-words opacity-80">{detail}</p>
          )}
        </Alert>
      );
    }
    const seconds = (elapsed_ms / 1000).toFixed(1);
    return (
      <div className="space-y-1">
        <p className="text-xs text-mid-gray">
          {t("settings.postProcessing.test.result", { seconds })}
        </p>
        {output?.trim() ? (
          <p className="p-3 text-sm whitespace-pre-wrap break-words rounded-md bg-mid-gray/10 select-text">
            {output}
          </p>
        ) : (
          <p className="p-3 text-sm rounded-md bg-mid-gray/10 text-mid-gray">
            {t("settings.postProcessing.test.emptyResult")}
          </p>
        )}
      </div>
    );
  })();

  return (
    <SettingsGroup title={t("settings.postProcessing.test.title")}>
      <SettingContainer
        title={t("settings.postProcessing.test.label")}
        description={t("settings.postProcessing.test.description")}
        descriptionMode="inline"
        layout="stacked"
        grouped={true}
      >
        <div className="space-y-3">
          <Textarea
            value={text}
            onChange={(event) => setText(event.target.value)}
            variant="compact"
            className="w-full"
            aria-label={t("settings.postProcessing.test.label")}
          />
          <Button
            onClick={() => void handleTest()}
            variant="primary"
            size="md"
            disabled={testing}
            className="inline-flex items-center gap-2"
          >
            {testing && <Loader2 className="h-4 w-4 animate-spin" />}
            {testing
              ? t("settings.postProcessing.test.testing")
              : t("settings.postProcessing.test.button")}
          </Button>
          {result}
        </div>
      </SettingContainer>
    </SettingsGroup>
  );
};
