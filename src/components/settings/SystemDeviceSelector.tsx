import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { ResetButton } from "../ui/ResetButton";
import { useSettings } from "../../hooks/useSettings";
import type { AudioDevice } from "@/bindings";

interface SystemDeviceSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/**
 * Picks which output device is captured when the audio source includes
 * system audio (meetings, calls, media playback). Hidden unless the audio
 * source is `system` or `both` (see GeneralSettings).
 */
export const SystemDeviceSelector: React.FC<SystemDeviceSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const {
      getSetting,
      updateSetting,
      resetSetting,
      isUpdating,
      isLoading,
      systemDevices,
      refreshSystemDevices,
    } = useSettings();

    const selectedSystemDevice =
      getSetting("selected_system_device") === "default"
        ? "Default"
        : getSetting("selected_system_device") || "Default";

    const handleSystemDeviceSelect = async (deviceName: string) => {
      await updateSetting("selected_system_device", deviceName);
    };

    const handleReset = async () => {
      await resetSetting("selected_system_device");
    };

    const systemDeviceOptions = systemDevices.map((device: AudioDevice) => ({
      value: device.name,
      label: device.name,
    }));

    return (
      <SettingContainer
        title={t("settings.sound.systemDevice.title")}
        description={t("settings.sound.systemDevice.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="flex items-center space-x-1">
          <Dropdown
            options={systemDeviceOptions}
            selectedValue={selectedSystemDevice}
            onSelect={handleSystemDeviceSelect}
            placeholder={
              isLoading || systemDevices.length === 0
                ? t("settings.sound.systemDevice.loading")
                : t("settings.sound.systemDevice.placeholder")
            }
            disabled={
              isUpdating("selected_system_device") ||
              isLoading ||
              systemDevices.length === 0
            }
            onOpen={refreshSystemDevices}
          />
          <ResetButton
            onClick={handleReset}
            disabled={isUpdating("selected_system_device") || isLoading}
          />
        </div>
      </SettingContainer>
    );
  });

SystemDeviceSelector.displayName = "SystemDeviceSelector";
