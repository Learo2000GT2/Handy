import type { TFunction } from "i18next";
import type { PostProcessProblem, PostProcessProblemKind } from "@/bindings";
import { isolateLtr } from "@/lib/utils/transcript";

/** Keys under `settings.postProcessing.problems`. */
const PROBLEM_KEYS = {
  empty_text: "emptyText",
  no_provider: "noProvider",
  no_model: "noModel",
  no_prompt: "noPrompt",
  empty_prompt: "emptyPrompt",
  apple_intelligence_unavailable: "appleIntelligenceUnavailable",
  missing_api_key: "missingApiKey",
  invalid_api_key: "invalidApiKey",
  invalid_address: "invalidAddress",
  unreachable: "unreachable",
  timeout: "timeout",
  unauthorized: "unauthorized",
  model_not_found: "modelNotFound",
  server_error: "serverError",
  no_answer: "noAnswer",
  bad_answer: "badAnswer",
  no_chat_models: "noChatModels",
  internal: "internal",
} as const satisfies Record<PostProcessProblemKind, string>;

/** What went wrong, in plain words. */
export const postProcessProblemText = (
  t: TFunction,
  problem: PostProcessProblem,
): string => {
  if (problem.kind === "apple_intelligence_unavailable") {
    return t("settings.postProcessing.api.appleIntelligence.unavailable");
  }
  const values = {
    provider: problem.provider ?? "",
    address: isolateLtr(problem.address ?? ""),
    model: isolateLtr(problem.model ?? ""),
    status: problem.status ?? "",
  };
  let key: string = PROBLEM_KEYS[problem.kind];
  if (problem.kind === "server_error") {
    // Nothing at the address, or an error that isn't an HTTP answer.
    if (problem.status === 404) key = "notFound";
    else if (problem.status === null) key = "serverErrorNoStatus";
  }
  return t(`settings.postProcessing.problems.${key}`, values);
};

/** The technical detail behind a problem, or null when there is none. */
export const postProcessProblemDetail = (
  t: TFunction,
  problem: PostProcessProblem,
): string | null => {
  if (!problem.detail) return null;
  if (problem.kind === "no_chat_models") {
    return t("settings.postProcessing.problems.otherModels", {
      models: isolateLtr(problem.detail),
    });
  }
  return t("settings.postProcessing.problems.detail", {
    detail: isolateLtr(problem.detail),
  });
};
