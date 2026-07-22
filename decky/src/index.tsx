import {
  ButtonItem,
  ConfirmModal,
  DropdownItem,
  PanelSection,
  PanelSectionRow,
  TextField,
  showModal,
  staticClasses,
} from "@decky/ui";
import { definePlugin, toaster } from "@decky/api";
import { useCallback, useEffect, useMemo, useState } from "react";
import type { CSSProperties } from "react";
import { FaMagic } from "react-icons/fa";

import {
  applyConfig,
  getConfig,
  getSnapshot,
  resetEnrollment,
  restartService,
  rollbackPending,
  validateConfig,
} from "./api";
import {
  buildConfigRequest,
  normalizeConfig,
  normalizeSnapshot,
  transactionId,
  unwrapRpc,
} from "./model.mjs";
import type { ConfigDraft, RpcEnvelope, SnapshotView } from "./model.mjs";

const rowStyle: CSSProperties = {
  display: "flex",
  justifyContent: "space-between",
  alignItems: "baseline",
  width: "100%",
  gap: "12px",
};

const secondaryStyle: CSSProperties = { opacity: 0.72, fontSize: "0.86em" };

function errorMessage(error: unknown): string {
  return error instanceof Error && error.message ? error.message : "Sigil management request failed";
}

function StatusRow({ label, value }: { label: string; value: string }) {
  return (
    <PanelSectionRow>
      <div style={rowStyle}>
        <span>{label}</span>
        <span style={{ textAlign: "right" }}>{value}</span>
      </div>
    </PanelSectionRow>
  );
}

function NumericField({
  label,
  value,
  disabled,
  onChange,
}: {
  label: string;
  value: string;
  disabled: boolean;
  onChange(value: string): void;
}) {
  return (
    <PanelSectionRow>
      <TextField
        label={label}
        mustBeNumeric
        disabled={disabled}
        value={value}
        onChange={(event) => onChange(event.currentTarget.value)}
      />
    </PanelSectionRow>
  );
}

function Content() {
  const [snapshot, setSnapshot] = useState<SnapshotView | null>(null);
  const [revision, setRevision] = useState("");
  const [draft, setDraft] = useState<ConfigDraft | null>(null);
  const [configPending, setConfigPending] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>("Loading");
  const [loadError, setLoadError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setBusy("Refreshing");
    setLoadError(null);
    try {
      const snapshotValue = unwrapRpc(await getSnapshot());
      setSnapshot(normalizeSnapshot(snapshotValue));
      const configValue = unwrapRpc(await getConfig());
      const config = normalizeConfig(configValue);
      setRevision(config.revision);
      setDraft(config.draft);
      setConfigPending(config.pendingTransaction);
    } catch (error) {
      setLoadError(errorMessage(error));
    } finally {
      setBusy(null);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const pendingId = useMemo(
    () => transactionId(configPending) ?? transactionId(snapshot?.pendingTransaction),
    [configPending, snapshot],
  );
  const controlsDisabled = busy !== null || draft === null || snapshot?.compatible === false;

  const run = useCallback(
    async (label: string, action: () => Promise<RpcEnvelope<unknown>>, success: string) => {
      setBusy(label);
      try {
        unwrapRpc(await action());
        toaster.toast({ title: "Sigil", body: success });
        await refresh();
      } catch (error) {
        const message = errorMessage(error);
        setLoadError(message);
        toaster.toast({ title: `${label} failed`, body: message });
      } finally {
        setBusy(null);
      }
    },
    [refresh],
  );

  const apply = async () => {
    if (!draft) return;
    let request;
    try {
      request = buildConfigRequest(revision, draft);
    } catch (error) {
      const message = errorMessage(error);
      setLoadError(message);
      toaster.toast({ title: "Invalid configuration", body: message });
      return;
    }
    setBusy("Applying configuration");
    try {
      unwrapRpc(await validateConfig(request));
      unwrapRpc(await applyConfig(request));
      toaster.toast({ title: "Sigil", body: "Configuration validated and applied" });
      await refresh();
    } catch (error) {
      const message = errorMessage(error);
      setLoadError(message);
      toaster.toast({ title: "Configuration failed", body: message });
    } finally {
      setBusy(null);
    }
  };

  const confirmRestart = (parent?: EventTarget) => {
    showModal(
      <ConfirmModal
        strTitle="Restart Sigil?"
        strDescription="The active Portal session will disconnect while the daemon restarts."
        strOKButtonText="Restart"
        strCancelButtonText="Cancel"
        onOK={() => void run("Restart", restartService, "Daemon restarted")}
      />,
      parent,
    );
  };

  const confirmEnrollmentReset = (parent?: EventTarget) => {
    if (!snapshot) return;
    showModal(
      <ConfirmModal
        strTitle="Reset Portal pairing?"
        strDescription={`Revoke the paired Portal and invalidate outstanding invitations for Sigil ${snapshot.hostFingerprint}. The host identity will not change.`}
        strOKButtonText="Reset pairing"
        strCancelButtonText="Cancel"
        bDestructiveWarning
        onOK={() =>
          void run(
            "Pairing reset",
            () => resetEnrollment(snapshot.hostFingerprint),
            "Portal pairing reset",
          )
        }
      />,
      parent,
    );
  };

  const setDraftField = <K extends keyof ConfigDraft>(key: K, value: ConfigDraft[K]) => {
    setDraft((current) => (current ? { ...current, [key]: value } : current));
  };

  return (
    <>
      <PanelSection title="Sigil">
        <StatusRow label="Status" value={busy ?? snapshot?.summary ?? "Unavailable"} />
        <StatusRow label="Service" value={snapshot?.serviceLabel ?? "Unknown"} />
        <StatusRow label="Version" value={snapshot?.version ?? "Unknown"} />
        <StatusRow label="Uptime" value={snapshot?.uptime ?? "Unavailable"} />
        <StatusRow label="Host" value={snapshot?.hostFingerprint ?? "Unavailable"} />
        {snapshot?.lastError && <StatusRow label="Last error" value={snapshot.lastError} />}
        {(snapshot?.managementError || loadError) && (
          <PanelSectionRow>
            <div style={{ color: "#ff8f8f" }}>{snapshot?.managementError ?? loadError}</div>
          </PanelSectionRow>
        )}
        <PanelSectionRow>
          <ButtonItem layout="below" disabled={busy !== null} onClick={() => void refresh()}>
            Refresh
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>

      <PanelSection title="Paired Portal">
        <StatusRow label="Device" value={snapshot?.peerFingerprint ?? "None"} />
        <StatusRow
          label="Grants"
          value={snapshot?.grants.length ? snapshot.grants.join(", ") : "None"}
        />
        <StatusRow label="Session" value={snapshot?.sessionActive ? "Active" : "Inactive"} />
        <StatusRow label="Enrollment epoch" value={String(snapshot?.epoch ?? 0)} />
      </PanelSection>

      {draft && (
        <PanelSection title="Stream configuration">
          <DropdownItem
            label="Resolution"
            description="Native follows the Gamescope surface; fixed preserves the chosen aspect ratio."
            rgOptions={[
              { data: "native", label: "Native" },
              { data: "fixed", label: "Fixed" },
            ]}
            selectedOption={draft.resolutionMode}
            disabled={controlsDisabled}
            onChange={(option) => setDraftField("resolutionMode", option.data)}
          />
          {draft.resolutionMode === "fixed" && (
            <>
              <NumericField
                label="Width"
                value={draft.width}
                disabled={controlsDisabled}
                onChange={(value) => setDraftField("width", value)}
              />
              <NumericField
                label="Height"
                value={draft.height}
                disabled={controlsDisabled}
                onChange={(value) => setDraftField("height", value)}
              />
            </>
          )}
          <NumericField
            label="Maximum frame rate"
            value={draft.framerate}
            disabled={controlsDisabled}
            onChange={(value) => setDraftField("framerate", value)}
          />
          {draft.rateMode === "unavailable" ? (
            <StatusRow label="Rate control" value="Unavailable for this source" />
          ) : (
            <>
              <DropdownItem
                label="Rate control"
                rgOptions={[
                  { data: "cbr", label: "CBR" },
                  { data: "cqp", label: "CQP" },
                ]}
                selectedOption={draft.rateMode}
                disabled={controlsDisabled}
                onChange={(option) => setDraftField("rateMode", option.data)}
              />
              {draft.rateMode === "cbr" ? (
                <NumericField
                  label="Bitrate (kbit/s)"
                  value={draft.bitrateKbps}
                  disabled={controlsDisabled}
                  onChange={(value) => setDraftField("bitrateKbps", value)}
                />
              ) : (
                <NumericField
                  label="Quantizer (1–51)"
                  value={draft.quantizer}
                  disabled={controlsDisabled}
                  onChange={(value) => setDraftField("quantizer", value)}
                />
              )}
            </>
          )}
          <PanelSectionRow>
            <ButtonItem layout="below" disabled={controlsDisabled} onClick={() => void apply()}>
              Validate and apply
            </ButtonItem>
          </PanelSectionRow>
          {pendingId && (
            <PanelSectionRow>
              <ButtonItem
                layout="below"
                disabled={busy !== null}
                onClick={() =>
                  void run(
                    "Rollback",
                    () => rollbackPending(pendingId),
                    "Pending configuration rolled back",
                  )
                }
              >
                Roll back pending change
              </ButtonItem>
            </PanelSectionRow>
          )}
        </PanelSection>
      )}

      <PanelSection title="Service">
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={busy !== null || snapshot?.installed === false}
            onClick={(event) => confirmRestart(event.currentTarget ?? undefined)}
          >
            Restart daemon
          </ButtonItem>
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={busy !== null || !snapshot}
            onClick={(event) => confirmEnrollmentReset(event.currentTarget ?? undefined)}
          >
            Reset Portal pairing…
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>

      <PanelSection title="Diagnostics and recovery">
        <StatusRow
          label="Stream diagnostics"
          value={snapshot?.streamDiagnosticsAvailable ? "Available" : "Unavailable in this build"}
        />
        <StatusRow label="Identity factory reset" value="Unavailable" />
        <PanelSectionRow>
          <div style={secondaryStyle}>
            Pairing reset preserves this Sigil’s identity. Factory reset is intentionally not exposed
            until Sigil provides a separate guarded contract.
          </div>
        </PanelSectionRow>
      </PanelSection>
    </>
  );
}

export default definePlugin(() => ({
  name: "Sigil",
  titleView: <div className={staticClasses.Title}>Sigil appliance</div>,
  content: <Content />,
  icon: <FaMagic />,
  onDismount() {},
}));
