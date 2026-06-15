import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

/// Shared cloud-model provider configuration, kept byte-compatible with the
/// CLI's `provider_config.rs` so the CLI and the VS Code extension read/write
/// the SAME files:
///   - `<workspace>/.Himalaya/provider.json`         (model + base_url + optional api_key, no secret by default)
///   - `$Himalaya_CONFIG_HOME|$HOME/.Himalaya/provider_credentials.json` (api_key)
///
/// This unifies cloud-model setup: a model configured in either surface is
/// immediately usable from the other.

export interface ProviderProfile {
  model: string;
  base_url?: string;
}

export interface ProviderSelection {
  model: string;
  baseUrl?: string;
  apiKey?: string;
}

interface ProviderConfigFile {
  model: string;
  base_url?: string;
  api_key?: string;
  profiles?: Record<string, ProviderProfile>;
}

export function providerConfigPath(workspaceRoot: string): string {
  return path.join(workspaceRoot, '.Himalaya', 'provider.json');
}

export function providerCredentialsPath(): string {
  const configHome = process.env.Himalaya_CONFIG_HOME
    ? process.env.Himalaya_CONFIG_HOME
    : path.join(os.homedir() || '.', '.Himalaya');
  return path.join(configHome, 'provider_credentials.json');
}

function readConfigFile(workspaceRoot: string): ProviderConfigFile | null {
  try {
    const raw = fs.readFileSync(providerConfigPath(workspaceRoot), 'utf8');
    const parsed = JSON.parse(raw) as ProviderConfigFile;
    if (parsed && typeof parsed.model === 'string') { return parsed; }
    return null;
  } catch {
    return null;
  }
}

function readApiKey(): string | undefined {
  try {
    const raw = fs.readFileSync(providerCredentialsPath(), 'utf8');
    const parsed = JSON.parse(raw) as { api_key?: unknown };
    return typeof parsed.api_key === 'string' ? parsed.api_key : undefined;
  } catch {
    return undefined;
  }
}

/// Load the persisted default cloud selection (model + base_url + api_key),
/// matching the CLI's `load_wizard_selection`. Returns null when none exists.
export function loadProviderSelection(workspaceRoot: string): ProviderSelection | null {
  const config = readConfigFile(workspaceRoot);
  if (!config) { return null; }
  return {
    model: config.model,
    baseUrl: config.base_url,
    apiKey: config.api_key ?? readApiKey()
  };
}

/// List named profiles (always includes "default" from the top-level entry),
/// matching the CLI's `list_profiles`.
export function listProviderProfiles(workspaceRoot: string): Record<string, ProviderProfile> {
  const config = readConfigFile(workspaceRoot);
  if (!config) { return {}; }
  const profiles: Record<string, ProviderProfile> = { ...(config.profiles ?? {}) };
  if (!profiles.default) {
    profiles.default = { model: config.model, base_url: config.base_url };
  }
  return profiles;
}

/// Persist a cloud selection to provider.json (+ credentials file), the same
/// way the CLI does. The API key is written with 0600 permissions on Unix and
/// never stored in the project-local provider.json.
export function saveProviderSelection(workspaceRoot: string, selection: ProviderSelection): void {
  const configDir = path.join(workspaceRoot, '.Himalaya');
  fs.mkdirSync(configDir, { recursive: true });

  const existing = readConfigFile(workspaceRoot);
  const config: ProviderConfigFile = {
    model: selection.model,
    ...(selection.baseUrl ? { base_url: selection.baseUrl } : {}),
    ...(existing?.profiles ? { profiles: existing.profiles } : {})
  };
  fs.writeFileSync(providerConfigPath(workspaceRoot), JSON.stringify(config, null, 2), 'utf8');

  if (selection.apiKey) {
    const credsPath = providerCredentialsPath();
    fs.mkdirSync(path.dirname(credsPath), { recursive: true });
    fs.writeFileSync(credsPath, JSON.stringify({ api_key: selection.apiKey }, null, 2), 'utf8');
    if (process.platform !== 'win32') {
      try { fs.chmodSync(credsPath, 0o600); } catch { /* best effort */ }
    }
  }
}

/// Persist a named provider profile. Adds or updates the profile entry in
/// provider.json while preserving existing profiles and the default model.
/// The api_key is never written to provider.json; use saveProviderCredentials
/// to persist it in the separate credentials file.
export function saveProviderProfile(
  workspaceRoot: string,
  profileName: string,
  profile: ProviderProfile
): void {
  const configDir = path.join(workspaceRoot, '.Himalaya');
  fs.mkdirSync(configDir, { recursive: true });

  const existing = readConfigFile(workspaceRoot);
  const profiles: Record<string, ProviderProfile> = { ...(existing?.profiles ?? {}) };
  profiles[profileName] = profile;

  const config: ProviderConfigFile = {
    model: existing?.model ?? profile.model,
    ...(existing?.base_url || profile.base_url ? { base_url: existing?.base_url ?? profile.base_url } : {}),
    profiles
  };
  fs.writeFileSync(providerConfigPath(workspaceRoot), JSON.stringify(config, null, 2), 'utf8');
}

/// Read a named profile from provider.json. Returns null if the profile does
/// not exist. Does NOT include the api_key — call readApiKey() separately.
export function loadProviderProfile(
  workspaceRoot: string,
  profileName: string
): ProviderProfile | null {
  const profiles = listProviderProfiles(workspaceRoot);
  return profiles[profileName] ?? null;
}
