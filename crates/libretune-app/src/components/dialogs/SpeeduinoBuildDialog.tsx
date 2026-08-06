/**
 * Speeduino firmware build-from-source proof-of-concept dialog.
 *
 * Unlike FirmwareUpdateDialog (flashing an already-built STM32/rusEFI
 * image), Speeduino ships no pre-built .hex for its AVR boards — this walks
 * through downloading the firmware source, compiling it via an
 * auto-fetched arduino-cli, and flashing the result. Scoped to the Arduino
 * Mega 2560 board only; see commands/speeduino_build.rs for the backend.
 */
import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Download, Cpu } from 'lucide-react';
import { Dialog, Button } from '../common';
import { RiskAcknowledgement } from '../common/RiskAcknowledgement';
import './SpeeduinoBuildDialog.css';

interface SpeeduinoToolchainInfo {
  arduino_cli_path: string | null;
  avr_core_installed: boolean;
}

interface SpeeduinoRelease {
  version: string;
  published_at: string;
}

interface SpeeduinoBuildResult {
  success: boolean;
  hex_path: string | null;
  log: string[];
}

interface DownloadProgress {
  received_bytes: number;
  total_bytes: number;
}

export interface SpeeduinoBuildDialogProps {
  isOpen: boolean;
  onClose: () => void;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function SpeeduinoBuildDialog({ isOpen, onClose }: SpeeduinoBuildDialogProps) {
  const [toolchain, setToolchain] = useState<SpeeduinoToolchainInfo | null>(null);
  const [checkingToolchain, setCheckingToolchain] = useState(false);
  const [downloadingCli, setDownloadingCli] = useState(false);

  const [release, setRelease] = useState<SpeeduinoRelease | null>(null);
  const [checkingRelease, setCheckingRelease] = useState(false);

  const [downloadProgress, setDownloadProgress] = useState<DownloadProgress | null>(null);
  const [sourcePath, setSourcePath] = useState<string | null>(null);
  const [downloadingSource, setDownloadingSource] = useState(false);

  const [compiling, setCompiling] = useState(false);
  const [buildResult, setBuildResult] = useState<SpeeduinoBuildResult | null>(null);

  const [ports, setPorts] = useState<string[]>([]);
  const [selectedPort, setSelectedPort] = useState('');
  const [acknowledgeRisk, setAcknowledgeRisk] = useState(false);
  const [uploading, setUploading] = useState(false);
  const [uploadResult, setUploadResult] = useState<SpeeduinoBuildResult | null>(null);

  const [log, setLog] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  const busy = checkingToolchain || downloadingCli || checkingRelease || downloadingSource || compiling || uploading;

  const refreshToolchain = useCallback(async () => {
    setCheckingToolchain(true);
    try {
      const info = await invoke<SpeeduinoToolchainInfo>('get_speeduino_toolchain_info');
      setToolchain(info);
    } catch (e) {
      setError(String(e));
    } finally {
      setCheckingToolchain(false);
    }
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    setError(null);
    setLog([]);
    void refreshToolchain();
    invoke<string[]>('get_serial_ports')
      .then((p) => {
        setPorts(p);
        if (p.length > 0) setSelectedPort(p[0]);
      })
      .catch(() => setPorts([]));
  }, [isOpen, refreshToolchain]);

  useEffect(() => {
    if (!isOpen) return undefined;
    const unlistenLog = listen<{ line: string }>('speeduino-build:log', (event) => {
      setLog((prev) => [...prev, event.payload.line]);
    });
    const unlistenProgress = listen<DownloadProgress>('speeduino-build:download-progress', (event) => {
      setDownloadProgress(event.payload);
    });
    return () => {
      void unlistenLog.then((fn) => fn());
      void unlistenProgress.then((fn) => fn());
    };
  }, [isOpen]);

  const handleDownloadCli = useCallback(async () => {
    setDownloadingCli(true);
    setError(null);
    setDownloadProgress(null);
    try {
      await invoke<string>('download_arduino_cli');
      await refreshToolchain();
    } catch (e) {
      setError(String(e));
    } finally {
      setDownloadingCli(false);
      setDownloadProgress(null);
    }
  }, [refreshToolchain]);

  const handleCheckRelease = useCallback(async () => {
    setCheckingRelease(true);
    setError(null);
    try {
      const r = await invoke<SpeeduinoRelease>('check_latest_speeduino_release');
      setRelease(r);
    } catch (e) {
      setError(String(e));
    } finally {
      setCheckingRelease(false);
    }
  }, []);

  const handleDownloadSource = useCallback(async () => {
    if (!release) return;
    setDownloadingSource(true);
    setError(null);
    setDownloadProgress(null);
    setSourcePath(null);
    setBuildResult(null);
    try {
      const path = await invoke<string>('download_speeduino_source', { version: release.version });
      setSourcePath(path);
    } catch (e) {
      setError(String(e));
    } finally {
      setDownloadingSource(false);
      setDownloadProgress(null);
    }
  }, [release]);

  const handleCompile = useCallback(async () => {
    if (!sourcePath) return;
    setCompiling(true);
    setError(null);
    setBuildResult(null);
    setLog([]);
    try {
      const result = await invoke<SpeeduinoBuildResult>('compile_speeduino_firmware', {
        sketchPath: sourcePath,
      });
      setBuildResult(result);
      if (!result.success) {
        setError('Compile failed — see log below.');
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setCompiling(false);
    }
  }, [sourcePath]);

  const handleFlash = useCallback(async () => {
    if (!sourcePath || !selectedPort) return;
    setUploading(true);
    setError(null);
    setUploadResult(null);
    setLog([]);
    try {
      const result = await invoke<SpeeduinoBuildResult>('upload_speeduino_firmware', {
        sketchPath: sourcePath,
        port: selectedPort,
      });
      setUploadResult(result);
      if (!result.success) {
        setError('Flash failed — see log below.');
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setUploading(false);
    }
  }, [sourcePath, selectedPort]);

  const canDownloadSource = !!release && !downloadingSource;
  const canCompile = !!sourcePath && !!toolchain?.arduino_cli_path && !compiling;
  const canFlash =
    !!buildResult?.success && !!selectedPort && acknowledgeRisk && !uploading;

  return (
    <Dialog
      open={isOpen}
      onClose={onClose}
      title="Build & Flash Speeduino Firmware (Proof of Concept)"
      size="lg"
      closeOnBackdrop={!busy}
      className="speeduino-build-dialog"
    >
      <Dialog.Body>
        <div className="speeduino-build-intro">
          <Cpu size={18} aria-hidden />
          <p>
            Downloads the Speeduino firmware source, compiles it for an Arduino Mega 2560 using an
            auto-fetched arduino-cli, and flashes the result. Proof-of-concept scope: AVR Mega 2560
            only — ESP32/Teensy/STM32 &quot;Black&quot; variants are not supported here.
          </p>
        </div>

        <div className="speeduino-build-step">
          <label>1. Toolchain</label>
          <div className="speeduino-build-row">
            <span className={`speeduino-build-status ${toolchain?.arduino_cli_path ? 'ok' : 'missing'}`}>
              {checkingToolchain
                ? 'Checking…'
                : toolchain?.arduino_cli_path
                  ? `arduino-cli found`
                  : 'arduino-cli not found'}
            </span>
            {!toolchain?.arduino_cli_path && (
              <Button
                variant="secondary"
                leadingIcon={<Download size={14} />}
                onClick={() => void handleDownloadCli()}
                disabled={busy}
              >
                {downloadingCli ? 'Downloading…' : 'Download arduino-cli'}
              </Button>
            )}
          </div>
        </div>

        <div className="speeduino-build-step">
          <label>2. Firmware Release</label>
          <div className="speeduino-build-row">
            <span className="speeduino-build-status">
              {release ? `Latest: ${release.version} (${release.published_at.slice(0, 10)})` : 'Not checked'}
            </span>
            <Button variant="secondary" onClick={() => void handleCheckRelease()} disabled={busy}>
              {checkingRelease ? 'Checking…' : 'Check Latest Release'}
            </Button>
          </div>
        </div>

        <div className="speeduino-build-step">
          <label>3. Download Source</label>
          <div className="speeduino-build-row">
            <span className="speeduino-build-status">
              {sourcePath ? 'Source downloaded' : 'Not downloaded'}
            </span>
            <Button
              variant="secondary"
              onClick={() => void handleDownloadSource()}
              disabled={!canDownloadSource || busy}
            >
              {downloadingSource ? 'Downloading…' : 'Download Source'}
            </Button>
          </div>
          {downloadProgress && downloadProgress.total_bytes > 0 && (
            <progress
              className="speeduino-build-progress"
              value={downloadProgress.received_bytes}
              max={downloadProgress.total_bytes}
            />
          )}
          {downloadProgress && (
            <p className="speeduino-build-hint">
              {formatBytes(downloadProgress.received_bytes)}
              {downloadProgress.total_bytes > 0 ? ` / ${formatBytes(downloadProgress.total_bytes)}` : ''}
            </p>
          )}
        </div>

        <div className="speeduino-build-step">
          <label>4. Compile</label>
          <div className="speeduino-build-row">
            <span className="speeduino-build-status">
              {buildResult?.success ? `Compiled: ${buildResult.hex_path}` : 'Not compiled'}
            </span>
            <Button variant="secondary" onClick={() => void handleCompile()} disabled={!canCompile || busy}>
              {compiling ? 'Compiling…' : 'Compile'}
            </Button>
          </div>
        </div>

        <div className="speeduino-build-step">
          <label>5. Flash</label>
          <div className="speeduino-build-row">
            <select
              className="speeduino-build-port-select"
              value={selectedPort}
              onChange={(e) => setSelectedPort(e.target.value)}
              disabled={busy || ports.length === 0}
            >
              {ports.length === 0 && <option value="">No ports found</option>}
              {ports.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
            <Button variant="primary" onClick={() => void handleFlash()} disabled={!canFlash}>
              {uploading ? 'Flashing…' : 'Flash'}
            </Button>
          </div>
          <RiskAcknowledgement
            acknowledged={acknowledgeRisk}
            onAcknowledgedChange={setAcknowledgeRisk}
            risk="high"
            warning="Flashing overwrites the firmware on the connected board. Make sure the selected port is the correct Speeduino board — flashing the wrong device can leave it unable to boot."
            acknowledgementText="I understand the risk and have selected the correct port."
            disabled={!buildResult?.success || uploading}
          />
        </div>

        {error && <div className="speeduino-build-error">{error}</div>}
        {uploadResult?.success && (
          <div className="speeduino-build-success">Flash successful.</div>
        )}

        {log.length > 0 && (
          <div className="speeduino-build-log">
            {log.map((line, idx) => (
              <div key={`${idx}-${line}`} className="speeduino-build-log-line">
                {line}
              </div>
            ))}
          </div>
        )}
      </Dialog.Body>

      <Dialog.Footer>
        <Button variant="secondary" onClick={onClose} disabled={busy}>
          Close
        </Button>
      </Dialog.Footer>
    </Dialog>
  );
}

export default SpeeduinoBuildDialog;
