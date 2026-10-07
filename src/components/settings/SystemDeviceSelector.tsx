import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { ResetButton } from "../ui/ResetButton";
import { useSettings } from "../../hooks/useSettings";
import { commands } from "@/bindings";
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

    const [testing, setTesting] = useState(false);
    const [testResult, setTestResult] = useState<string | null>(null);
    const [testOk, setTestOk] = useState<boolean | null>(null);

    const handleTest = async () => {
      setTesting(true);
      setTestResult(null);
      setTestOk(null);
      try {
        const result = await commands.testSystemCapture();
        if (result.status === "ok") {
          setTestOk(true);
          setTestResult(result.data.message);
        } else {
          setTestOk(false);
          setTestResult(result.error);
        }
      } catch (e) {
        setTestOk(false);
        setTestResult(e instanceof Error ? e.message : String(e));
      } finally {
        setTesting(false);
      }
    };

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
          <button
            type="button"
            onClick={handleTest}
            disabled={testing || isLoading}
            className="px-2 py-[5px] text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 rounded-md hover:bg-logo-primary/10 hover:border-logo-primary disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {testing
              ? t("settings.sound.systemDevice.testing")
              : t("settings.sound.systemDevice.test")}
          </button>
        </div>
        {testResult !== null && (
          <div
            className={`text-sm mt-1 ${testOk ? "text-green-600" : "text-red-500"}`}
          >
            {testResult}
          </div>
        )}
      </SettingContainer>
    );
  });

SystemDeviceSelector.displayName = "SystemDeviceSelector";
