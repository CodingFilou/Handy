import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { SettingContainer } from "../ui/SettingContainer";
import { PathDisplay } from "../ui/PathDisplay";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import { useSettings } from "../../hooks/useSettings";

interface MeetingSettingsProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const MeetingSettings: React.FC<MeetingSettingsProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, resetSetting, isUpdating } =
      useSettings();

    const saveEnabled = getSetting("save_meeting_transcripts") ?? true;
    const folder = getSetting("meeting_folder") ?? null;
    const micName = getSetting("speaker_mic_name") ?? "";
    const sysName = getSetting("speaker_sys_name") ?? "";

    // Local draft state so typing doesn't fire a backend call per keystroke.
    const [micDraft, setMicDraft] = useState<string | null>(null);
    const [sysDraft, setSysDraft] = useState<string | null>(null);

    const handlePickFolder = async () => {
      const selected = await open({
        directory: true,
        multiple: false,
        title: t("settings.meeting.folderPickerTitle"),
      });
      if (typeof selected === "string") {
        await updateSetting("meeting_folder", selected);
      }
    };

    const commitName = async (
      key: "speaker_mic_name" | "speaker_sys_name",
      draft: string | null,
    ) => {
      if (draft === null) return;
      await updateSetting(key, draft);
    };

    return (
      <>
        <ToggleSwitch
          checked={saveEnabled}
          onChange={(enabled) =>
            updateSetting("save_meeting_transcripts", enabled)
          }
          isUpdating={isUpdating("save_meeting_transcripts")}
          label={t("settings.meeting.save.label")}
          description={t("settings.meeting.save.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        />
        <SettingContainer
          title={t("settings.meeting.folder.title")}
          description={t("settings.meeting.folder.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
          layout="stacked"
        >
          <div className="flex items-center gap-2">
            <div className="flex-1 min-w-0">
              <PathDisplay
                path={folder || t("settings.meeting.folder.default")}
                onOpen={handlePickFolder}
                disabled={false}
              />
            </div>
            <Button
              variant="secondary"
              size="sm"
              onClick={handlePickFolder}
              disabled={isUpdating("meeting_folder")}
            >
              {t("settings.meeting.folder.browse")}
            </Button>
            {folder && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => resetSetting("meeting_folder")}
                disabled={isUpdating("meeting_folder")}
              >
                {t("settings.meeting.folder.reset")}
              </Button>
            )}
          </div>
        </SettingContainer>
        <SettingContainer
          title={t("settings.meeting.speakers.title")}
          description={t("settings.meeting.speakers.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
          layout="stacked"
        >
          <div className="flex flex-col gap-2">
            <label className="flex items-center gap-2 text-sm">
              <span className="w-28 shrink-0 opacity-70">
                {t("settings.meeting.speakers.mic")}
              </span>
              <Input
                value={micDraft ?? micName}
                placeholder={t("settings.meeting.speakers.micPlaceholder")}
                onChange={(e) => setMicDraft(e.target.value)}
                onBlur={() => {
                  void commitName("speaker_mic_name", micDraft);
                  setMicDraft(null);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                }}
                disabled={isUpdating("speaker_mic_name")}
              />
            </label>
            <label className="flex items-center gap-2 text-sm">
              <span className="w-28 shrink-0 opacity-70">
                {t("settings.meeting.speakers.system")}
              </span>
              <Input
                value={sysDraft ?? sysName}
                placeholder={t("settings.meeting.speakers.systemPlaceholder")}
                onChange={(e) => setSysDraft(e.target.value)}
                onBlur={() => {
                  void commitName("speaker_sys_name", sysDraft);
                  setSysDraft(null);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                }}
                disabled={isUpdating("speaker_sys_name")}
              />
            </label>
          </div>
        </SettingContainer>
      </>
    );
  },
);
