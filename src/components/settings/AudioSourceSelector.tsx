import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { ResetButton } from "../ui/ResetButton";
import { useSettings } from "../../hooks/useSettings";
import type { AudioSource } from "@/bindings";

interface AudioSourceSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AudioSourceSelector: React.FC<AudioSourceSelectorProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const {
      getSetting,
      updateSetting,
      resetSetting,
      isUpdating,
      isLoading,
    } = useSettings();

    const selectedSource: AudioSource =
      (getSetting("audio_source") as AudioSource | undefined) ?? "microphone";

    const handleSourceSelect = async (source: string) => {
      await updateSetting("audio_source", source as AudioSource);
    };

    const handleReset = async () => {
      await resetSetting("audio_source");
    };

    const sourceOptions: { value: AudioSource; label: string }[] = [
      { value: "microphone", label: t("settings.sound.audioSource.microphone") },
      { value: "system", label: t("settings.sound.audioSource.system") },
      { value: "both", label: t("settings.sound.audioSource.both") },
    ];

    return (
      <SettingContainer
        title={t("settings.sound.audioSource.title")}
        description={t("settings.sound.audioSource.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="flex items-center space-x-1">
          <Dropdown
            options={sourceOptions}
            selectedValue={selectedSource}
            onSelect={handleSourceSelect}
            placeholder={t("settings.sound.audioSource.placeholder")}
            disabled={isUpdating("audio_source") || isLoading}
          />
          <ResetButton
            onClick={handleReset}
            disabled={isUpdating("audio_source") || isLoading}
          />
        </div>
      </SettingContainer>
    );
  },
);

AudioSourceSelector.displayName = "AudioSourceSelector";
