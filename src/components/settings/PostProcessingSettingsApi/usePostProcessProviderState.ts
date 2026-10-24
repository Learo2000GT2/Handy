import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSettings } from "../../../hooks/useSettings";
import {
  commands,
  type PostProcessProblem,
  type PostProcessProvider,
} from "@/bindings";
import { isValidServerUrl } from "@/lib/utils/transcript";
import type { ModelOption } from "./types";
import type { DropdownOption } from "../../ui/Dropdown";

type PostProcessProviderState = {
  providerOptions: DropdownOption[];
  selectedProviderId: string;
  selectedProvider: PostProcessProvider | undefined;
  isCustomProvider: boolean;
  isLemonadeProvider: boolean;
  /** The provider is a server the user runs: its address can be edited and
   * an API key is optional. */
  canEditBaseUrl: boolean;
  isAppleProvider: boolean;
  appleIntelligenceUnavailable: boolean;
  baseUrl: string;
  handleBaseUrlChange: (value: string) => void;
  isBaseUrlUpdating: boolean;
  /** The last address typed for Lemonade couldn't be used. */
  baseUrlInvalid: boolean;
  apiKey: string;
  handleApiKeyChange: (value: string) => void;
  isApiKeyUpdating: boolean;
  model: string;
  handleModelChange: (value: string) => void;
  modelOptions: ModelOption[];
  isModelUpdating: boolean;
  isFetchingModels: boolean;
  /** Why the model list couldn't be loaded, if it couldn't. */
  modelProblem: PostProcessProblem | null;
  handleProviderSelect: (providerId: string) => void;
  handleModelSelect: (value: string) => void;
  handleModelCreate: (value: string) => void;
  handleRefreshModels: () => void;
};

const APPLE_PROVIDER_ID = "apple_intelligence";
export const LEMONADE_PROVIDER_ID = "lemonade";

export const usePostProcessProviderState = (): PostProcessProviderState => {
  const {
    settings,
    isUpdating,
    setPostProcessProvider,
    updatePostProcessBaseUrl,
    updatePostProcessApiKey,
    updatePostProcessModel,
    fetchPostProcessModels,
    postProcessModelOptions,
    postProcessModelProblems,
  } = useSettings();

  // Settings are guaranteed to have providers after migration
  const providers = settings?.post_process_providers || [];

  const selectedProviderId = useMemo(() => {
    return settings?.post_process_provider_id || providers[0]?.id || "openai";
  }, [providers, settings?.post_process_provider_id]);

  const selectedProvider = useMemo(() => {
    return (
      providers.find((provider) => provider.id === selectedProviderId) ||
      providers[0]
    );
  }, [providers, selectedProviderId]);

  const isAppleProvider = selectedProvider?.id === APPLE_PROVIDER_ID;
  const canEditBaseUrl = selectedProvider?.allow_base_url_edit ?? false;
  const [appleIntelligenceUnavailable, setAppleIntelligenceUnavailable] =
    useState(false);
  const [baseUrlInvalid, setBaseUrlInvalid] = useState(false);
  // The provider whose models were last loaded without a click on refresh,
  // so a provider is loaded once, by its selection or by the page showing it.
  const autoFetchedRef = useRef<string | null>(null);

  // Use settings directly as single source of truth
  const baseUrl = selectedProvider?.base_url ?? "";
  const apiKey = settings?.post_process_api_keys?.[selectedProviderId] ?? "";
  const model = settings?.post_process_models?.[selectedProviderId] ?? "";

  const providerOptions = useMemo<DropdownOption[]>(() => {
    return providers.map((provider) => ({
      value: provider.id,
      label: provider.label,
    }));
  }, [providers]);

  const handleProviderSelect = useCallback(
    async (providerId: string) => {
      // Clear error state on any selection attempt (allows dismissing the error)
      setAppleIntelligenceUnavailable(false);
      setBaseUrlInvalid(false);

      if (providerId === selectedProviderId) return;
      autoFetchedRef.current = providerId;

      // Check Apple Intelligence availability before selecting
      if (providerId === APPLE_PROVIDER_ID) {
        const available = await commands.checkAppleIntelligenceAvailable();
        if (!available) {
          setAppleIntelligenceUnavailable(true);
          // Don't return - still set the provider so dropdown shows the selection
          // The backend gracefully handles unavailable Apple Intelligence
        }
      }

      await setPostProcessProvider(providerId);

      // Auto-fetch available models for the new provider so the model dropdown
      // reflects what's actually valid. Without this, a stale model value from
      // a previous provider/base_url can persist and silently 404 at runtime.
      // Skip when the provider isn't configured yet (no API key / empty base URL)
      // to avoid unnecessary backend errors.
      if (providerId !== APPLE_PROVIDER_ID) {
        const provider = providers.find((p) => p.id === providerId);
        const apiKey = settings?.post_process_api_keys?.[providerId] ?? "";
        const hasBaseUrl = (provider?.base_url ?? "").trim() !== "";
        const hasApiKey = apiKey.trim() !== "";

        // A server the user runs may need no API key.
        if (provider?.allow_base_url_edit ? hasBaseUrl : hasApiKey) {
          void fetchPostProcessModels(providerId);
        }
      }
    },
    [
      selectedProviderId,
      setPostProcessProvider,
      fetchPostProcessModels,
      providers,
      settings,
    ],
  );

  const handleBaseUrlChange = useCallback(
    (value: string) => {
      if (!selectedProvider || !selectedProvider.allow_base_url_edit) {
        return;
      }
      const trimmed = value.trim();
      const isLemonade = selectedProvider.id === LEMONADE_PROVIDER_ID;
      // The backend refuses a Lemonade address it can't build requests from.
      if (isLemonade && !isValidServerUrl(trimmed)) {
        setBaseUrlInvalid(true);
        return;
      }
      setBaseUrlInvalid(false);
      if (trimmed && trimmed !== baseUrl) {
        const providerId = selectedProvider.id;
        void updatePostProcessBaseUrl(providerId, trimmed).then((saved) => {
          if (!saved) {
            setBaseUrlInvalid(isLemonade);
          } else if (isLemonade) {
            // Show the new server's models straight away.
            void fetchPostProcessModels(providerId);
          }
        });
      }
    },
    [
      selectedProvider,
      baseUrl,
      updatePostProcessBaseUrl,
      fetchPostProcessModels,
    ],
  );

  const handleApiKeyChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== apiKey) {
        void updatePostProcessApiKey(selectedProviderId, trimmed);
      }
    },
    [apiKey, selectedProviderId, updatePostProcessApiKey],
  );

  const handleModelChange = useCallback(
    (value: string) => {
      const trimmed = value.trim();
      if (trimmed !== model) {
        void updatePostProcessModel(selectedProviderId, trimmed);
      }
    },
    [model, selectedProviderId, updatePostProcessModel],
  );

  const handleModelSelect = useCallback(
    (value: string) => {
      void updatePostProcessModel(selectedProviderId, value.trim());
    },
    [selectedProviderId, updatePostProcessModel],
  );

  const handleModelCreate = useCallback(
    (value: string) => {
      void updatePostProcessModel(selectedProviderId, value);
    },
    [selectedProviderId, updatePostProcessModel],
  );

  const handleRefreshModels = useCallback(() => {
    if (isAppleProvider) return;
    void fetchPostProcessModels(selectedProviderId);
  }, [fetchPostProcessModels, isAppleProvider, selectedProviderId]);

  const availableModelsRaw = postProcessModelOptions[selectedProviderId] || [];

  const modelOptions = useMemo<ModelOption[]>(() => {
    const seen = new Set<string>();
    const options: ModelOption[] = [];

    const upsert = (value: string | null | undefined) => {
      const trimmed = value?.trim();
      if (!trimmed || seen.has(trimmed)) return;
      seen.add(trimmed);
      options.push({ value: trimmed, label: trimmed });
    };

    // Add available models from API
    for (const candidate of availableModelsRaw) {
      upsert(candidate);
    }

    // Ensure current model is in the list
    upsert(model);

    return options;
  }, [availableModelsRaw, model]);

  const isBaseUrlUpdating = isUpdating(
    `post_process_base_url:${selectedProviderId}`,
  );
  const isApiKeyUpdating = isUpdating(
    `post_process_api_key:${selectedProviderId}`,
  );
  const isModelUpdating = isUpdating(
    `post_process_model:${selectedProviderId}`,
  );
  const isFetchingModels = isUpdating(
    `post_process_models_fetch:${selectedProviderId}`,
  );

  const isCustomProvider = selectedProvider?.id === "custom";
  const isLemonadeProvider = selectedProvider?.id === LEMONADE_PROVIDER_ID;
  const modelProblem = postProcessModelProblems[selectedProviderId] ?? null;

  // A Lemonade Server needs no API key, so its models are loaded once when
  // the page shows it; other providers wait for the refresh button.
  useEffect(() => {
    if (!isLemonadeProvider || autoFetchedRef.current === selectedProviderId) {
      return;
    }
    autoFetchedRef.current = selectedProviderId;
    if (availableModelsRaw.length === 0 && !isFetchingModels) {
      void fetchPostProcessModels(selectedProviderId);
    }
  }, [
    isLemonadeProvider,
    selectedProviderId,
    availableModelsRaw.length,
    isFetchingModels,
    fetchPostProcessModels,
  ]);

  return {
    providerOptions,
    selectedProviderId,
    selectedProvider,
    isCustomProvider,
    isLemonadeProvider,
    canEditBaseUrl,
    isAppleProvider,
    appleIntelligenceUnavailable,
    baseUrl,
    handleBaseUrlChange,
    isBaseUrlUpdating,
    baseUrlInvalid,
    apiKey,
    handleApiKeyChange,
    isApiKeyUpdating,
    model,
    handleModelChange,
    modelOptions,
    isModelUpdating,
    isFetchingModels,
    modelProblem,
    handleProviderSelect,
    handleModelSelect,
    handleModelCreate,
    handleRefreshModels,
  };
};
